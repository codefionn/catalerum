//! catalerum — the binary entrypoint (SOUL §4, §13, §16).
//!
//! Boot sequence:
//! 1. Parse the TOML config (`config/catalerum.toml` by default) and apply
//!    `CATALERUM_`-prefixed environment overrides.
//! 2. Initialise tracing.
//! 3. Connect Postgres and run the store migrations on startup (the store is
//!    the single source of truth and the only migrator, SOUL §6.1; IAM shares
//!    the pool and never migrates).
//! 4. Connect the Valkey/in-process bus and build the llmleaf chat client.
//! 5. If dev-login is enabled, seed the admin + default workspace and print the
//!    dev magic-link login URL to stdout (the `just dev` login line, SOUL §17).
//! 6. Build the Axum router and serve.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use tracing::info;

use catalerum_api::{
    build_router, serve, AppState, Config, FetchConfig, McpAuthConfig, SearchConfig,
    SkillPromptProvider, TelemetryContent, WorkspaceResourceProvider,
};
use catalerum_bus::Bus;
use catalerum_core::id::{UserId, WorkspaceId};
use catalerum_core::model::Role;
use catalerum_core::provider::{WebFetcher, WebSearcher};
use catalerum_core::tool::ToolContext;
use catalerum_fetch::{
    BackendKind, FetchPolicy, FirecrawlFetcher, HttpFetcher, HttpWebhookSender, MultiFetcher,
    WebhookSender,
};
use catalerum_iam::{base_capabilities, IamService, PgIamStore, DEFAULT_WORKSPACE_SLUG};
use catalerum_llm::{LlmTraceConfig, OpenRouterClient, TraceContent};
use catalerum_mcp::McpServer;
use catalerum_search::MultiSearcher;
use catalerum_store::Store;

mod telemetry;

/// catalerum — self-hostable, automated, fully-integrated LLM assistant.
#[derive(Debug, Parser)]
#[command(name = "catalerum", version, about)]
struct Cli {
    /// Path to the TOML config file. Falls back to `$CATALERUM_CONFIG`, then
    /// `config/catalerum.toml`.
    #[arg(short, long)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands. No subcommand → run the HTTP/WebSocket API + workers (the default).
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the external MCP server over stdio (SOUL §26): exposes the scoped tool
    /// registry to MCP clients (Claude Code / Codex / opencode). JSON-RPC on
    /// stdout, logs on stderr. Scoped by `CATALERUM_MCP_TOKEN` (a service token),
    /// else (dev-login) the default workspace's owner.
    Mcp,
    /// Mint a long-lived service token (a bearer) for the dev workspace owner and
    /// print it to stdout (SOUL §18/§26) — set it as `CATALERUM_MCP_TOKEN` for
    /// `catalerum mcp` (or send it as an API `Authorization: Bearer`). `--revoke
    /// <token>` revokes one instead.
    Token {
        /// Token lifetime in days (clamped to ≥ 1).
        #[arg(long, default_value_t = 365)]
        ttl_days: i64,
        /// Revoke this token instead of minting a new one.
        #[arg(long, value_name = "TOKEN")]
        revoke: Option<String>,
        /// Scope the token to a named §19 grant (by grant id or name) in the dev
        /// workspace (SOUL §19/§26): the minted bearer then carries the grant's
        /// attenuated capabilities instead of the owner's full role, so an MCP
        /// client (Claude Code / Codex / opencode) is bounded by the grant. The
        /// grant must be ⊆ the owner's authority. Omit for a full-role token.
        #[arg(long, value_name = "GRANT")]
        grant: Option<String>,
    },
    /// Take a one-off backup now (SOUL §30): dump Postgres (the source of truth)
    /// and copy the object blobs to `[backup.destination]` (an S3 bucket, a WebDAV
    /// collection, or a local directory), then prune to `[backup].keep`. Prints
    /// the new backup id. Works whether or not the scheduled worker is `enabled`,
    /// as long as a destination is configured.
    Backup,
    /// Restore from a backup id (SOUL §30). **Destructive** — it REPLACES the
    /// current Postgres contents and object blobs. Requires `--yes`. Run with no
    /// id to list available backups. Afterward the derived stores (Neo4j/Qdrant)
    /// rebuild via re-ingest (§6.3/§6.4).
    Restore {
        /// The backup id to restore (its `<prefix>/<id>/` directory name). Omit
        /// to list available backups and exit without changing anything.
        #[arg(value_name = "BACKUP_ID")]
        id: Option<String>,
        /// Confirm the destructive restore. Without it, nothing is changed.
        #[arg(long)]
        yes: bool,
        /// Restore even if the backup's schema version differs from the live DB
        /// (normally a mismatch is refused so data never loads into a changed
        /// schema).
        #[arg(long)]
        force: bool,
    },
}

impl Cli {
    /// The resolved config path: `--config`, else `$CATALERUM_CONFIG`, else the
    /// default.
    fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .or_else(|| std::env::var_os("CATALERUM_CONFIG").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("config/catalerum.toml"))
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = load_config(&cli.config_path())?;

    let _telemetry = telemetry::init(&config.telemetry)?;

    // These subcommands own stdout (JSON-RPC / the token), so they must run before
    // any banner/stdout print and with logs on stderr (SOUL §26).
    match cli.command {
        Some(Command::Mcp) => return run_mcp(config).await,
        Some(Command::Token {
            ttl_days,
            revoke,
            grant,
        }) => return run_token(config, ttl_days, revoke, grant).await,
        Some(Command::Backup) => return run_backup(config).await,
        Some(Command::Restore { id, yes, force }) => {
            return run_restore(config, id, yes, force).await
        }
        None => {}
    }

    print_banner();
    info!(listen = %config.server.listen, "starting catalerum");

    // --- Relational source of truth + migrations -----------------------------
    let store = Store::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to database at {}", config.database.url))?;
    store.migrate().await.context("running store migrations")?;
    info!("store migrations applied");

    // NB: terminal/sandbox boot reconcile (SOUL §20) is now **pod-scoped** and runs
    // after `AppState` is built, so it can use `state.pod_id()` — the same identity
    // the managers stamp on rows. See the reconcile block just after `AppState::new`.

    // catalerum-store is the single Postgres source of truth and the only
    // migrator (SOUL §6.1); IAM never migrates — it shares the store's pool and
    // delegates persistence to the store's repositories.
    let iam_store = PgIamStore::new(store.pool().clone());

    let base_url = config.server.effective_base_url();
    let iam = IamService::new(iam_store).with_base_url(base_url.clone());

    // --- Bus (Valkey or in-process fallback) ---------------------------------
    let bus = if config.valkey.enabled {
        match Bus::connect(&config.valkey.url).await {
            Ok(b) => {
                info!(url = %config.valkey.url, "connected to Valkey bus");
                b
            }
            Err(e) => {
                tracing::warn!(error = %e, "Valkey connect failed; using in-process bus");
                Bus::in_process()
            }
        }
    } else {
        info!("Valkey disabled; using in-process bus");
        Bus::in_process()
    };

    // --- LLM client (llmleaf / OpenRouter) -----------------------------------
    let llm = build_llm_client(&config);
    info!(base_url = %config.llm.base_url, model = %config.llm.default_model, "llm client ready");

    // --- Web fetcher (HTTP / browser-CDP / Firecrawl, SOUL §27) --------------
    // The plain-HTTP backend is always built (local-first); Firecrawl and the
    // browser backend activate from `[fetch]` config. Powers `POST /fetch` and
    // the `fetch_url` tool, with the SSRF egress guard baked in.
    let fetcher = build_fetcher(&config.fetch).context("building web fetcher")?;
    // Outbound webhook delivery (SOUL §11/§27) rides the same egress policy.
    let webhook_sender = build_webhook_sender(&config.fetch);
    let searcher = build_searcher(&config.search);
    info!(
        backend = %config.fetch.backend,
        allow_private_hosts = config.fetch.allow_private_hosts,
        "web fetcher ready"
    );

    // --- Dev magic-link seed -------------------------------------------------
    if config.auth.dev_login {
        match iam.ensure_dev_login().await {
            Ok(link) => {
                println!();
                println!("  Dev login (open in your browser to sign in):");
                println!("    {}", link.url);
                println!();
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not seed dev login");
            }
        }

        if let Some(token) = std::env::var("CATALERUM_DEV_AUTHORIZATION_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
        {
            match iam
                .ensure_dev_authorization_token_days(token.trim(), 365)
                .await
            {
                Ok(session) => {
                    println!("  Dev authorization token (stable for this dev run):");
                    println!("    Authorization: Bearer {}", session.token);
                    println!();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not seed dev authorization token");
                }
            }
        }

        // Seed the first-party skills (SOUL §23) into the default workspace that
        // `ensure_dev_login` just ensured, so `use_skill`/`list_skills` work out of
        // the box. Idempotent (upsert-by-name) — a harmless definition refresh on
        // every boot. Real (SSO-provisioned) workspaces will seed these in their
        // own provisioning path once that lands (M7); for now the dev default
        // workspace is the only one that exists.
        match store
            .workspaces()
            .get_by_slug(catalerum_iam::DEFAULT_WORKSPACE_SLUG)
            .await
        {
            Ok(ws) => match catalerum_skills::seed_first_party(&store, ws.id).await {
                Ok(skills) => {
                    info!(count = skills.len(), workspace = %ws.id, "seeded first-party skills")
                }
                Err(e) => tracing::warn!(error = %e, "could not seed first-party skills"),
            },
            Err(e) => {
                tracing::warn!(error = %e, "default workspace absent; skipped first-party skill seed")
            }
        }

        // Seed the dev admin as **Owner** of the default organisation (SOUL §18).
        // The `0046` migration seeds the default org itself + backfills org
        // memberships from *existing* workspace memberships, but on a fresh DB the
        // dev admin's workspace membership is minted by `ensure_dev_login` after the
        // migration ran — so add the admin's org membership here. Idempotent
        // (upsert-by-key); mode-drives the default org's workspace-creation policy.
        match store
            .users()
            .get_by_email(catalerum_iam::DEFAULT_ADMIN_EMAIL)
            .await
        {
            Ok(admin) => {
                if let Err(e) = store
                    .org_memberships()
                    .upsert(
                        catalerum_iam::DEFAULT_ORGANISATION_ID,
                        admin.id,
                        catalerum_core::model::OrgRole::Owner,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "could not seed default org membership");
                }
                // Align the default org's workspace-creation policy with the
                // deployment mode's default (members in single-user, admins in
                // multi-user) so the seeded org matches the running mode.
                if let Err(e) = store
                    .organisations()
                    .set_workspace_creation(
                        catalerum_iam::DEFAULT_ORGANISATION_ID,
                        config.server.default_workspace_creation(),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "could not set default org policy");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "default admin absent; skipped default org membership seed")
            }
        }
    }

    // --- Ingest worker (calendar sync + note embedding, SOUL §6.2/§6.4/§10) ---
    // Spawn the durable-queue worker as a detached background task before
    // serving. It drains `sync_calendar` jobs from the Postgres `job_queue`
    // (`FOR UPDATE SKIP LOCKED`) and never blocks the API. When a Qdrant is
    // configured + enabled (`[vector]`), it also gets an embed context so it runs
    // `ingest_note` jobs (chunk → llmleaf embed → Qdrant upsert, §6.4/§10). The
    // store/llm are cloned (cheap — pool + client are Arc-shared) since `state`
    // takes ownership below.
    let mut ingest_worker = catalerum_ingest::SyncWorker::new(store.clone());
    if config.qdrant.enabled {
        match catalerum_vector::VectorStore::new(&config.qdrant.url) {
            Ok(vector) => {
                let embedder: Arc<dyn catalerum_core::provider::Embedder> = Arc::new(llm.clone());
                let ingest_cfg = catalerum_ingest::IngestConfig::new(&config.llm.embedding_model);
                ingest_worker = ingest_worker.with_embed_context(
                    catalerum_ingest::EmbedContext::new(embedder, vector, ingest_cfg),
                );
                info!(
                    qdrant = %config.qdrant.url,
                    embed_model = %config.llm.embedding_model,
                    "note embedding enabled (ingest_note jobs)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, url = %config.qdrant.url,
                    "invalid [qdrant].url; note embedding disabled"
                );
            }
        }
    }
    // When a Neo4j is configured + enabled (`[neo4j]`), attach a graph context so
    // the worker also runs `project_note` jobs (note → :Note + :Topic nodes,
    // SOUL §6.3/§10). Indexes are ensured once at startup (idempotent). A clone of
    // the same handle is held for the schedule worker's `GraphQuery` poll (§11).
    let mut schedule_graph: Option<catalerum_graph::GraphStore> = None;
    if config.neo4j.enabled {
        match catalerum_graph::GraphStore::new(&config.neo4j.url) {
            Ok(graph) => {
                let graph = graph
                    .with_auth(config.neo4j.user.clone(), config.neo4j.password.expose())
                    .with_database(config.neo4j.database.clone());
                if let Err(e) = graph.ensure_indexes().await {
                    tracing::warn!(error = %e, "neo4j ensure_indexes failed; continuing");
                }
                schedule_graph = Some(graph.clone());
                ingest_worker =
                    ingest_worker.with_graph_context(catalerum_ingest::GraphContext::new(graph));
                info!(neo4j = %config.neo4j.url, "note graph projection enabled (project_note jobs)");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e, url = %config.neo4j.url,
                    "invalid [neo4j].url; graph projection disabled"
                );
            }
        }
    }
    // When memory auto-curation is enabled (`[curation]`), attach a curate
    // context so the worker also mines conversations for memories via the LLM
    // (SOUL §22). The same llmleaf client extracts; the model defaults to the
    // chat model unless `[curation].model` is set.
    if config.curation.enabled {
        let model = if config.curation.model.is_empty() {
            config.llm.default_model.clone()
        } else {
            config.curation.model.clone()
        };
        let llm_client: Arc<dyn catalerum_core::provider::LlmClient> = Arc::new(llm.clone());
        ingest_worker = ingest_worker
            .with_curate_context(catalerum_ingest::CurateContext::new(llm_client, &model));
        info!(model = %model, "memory auto-curation enabled (extract_memories jobs)");
    }
    // The worker is spawned below, after `AppState` is built — the automation
    // action runner (SOUL §11) needs the tool registry `AppState` owns, and the
    // object-ingest context (SOUL §9/§10) reuses the backend `AppState` builds.

    // --- Command executor (SOUL §20) -----------------------------------------
    // Protected + opt-in: built only when `[exec].enabled`. Even then the
    // `run_command` tool requires `exec:run` (no base role holds it, §19), and
    // the local executor enforces its program allow-list. `None` → no executor,
    // no `run_command` tool.
    // The local executor still backs `run_command`; the terminal manager (SOUL
    // §20) gets a map of executor backends — local + a scrubbed-env **sandbox**
    // variant now, with container/k8s added in their slices.
    type DynExec = Arc<dyn catalerum_core::provider::Executor>;
    let (executor, terminal_backends): (
        Option<DynExec>,
        std::collections::HashMap<catalerum_core::model::ExecutorKind, DynExec>,
    ) = if config.exec.enabled {
        let build_local = || {
            if config.exec.allow.is_empty() {
                catalerum_exec::LocalExecutor::new()
            } else {
                catalerum_exec::LocalExecutor::with_allow_list(config.exec.allow.clone())
            }
        };
        let local: DynExec = Arc::new(build_local());
        let sandbox: DynExec = Arc::new(build_local().sandboxed());
        let container: DynExec = Arc::new(catalerum_exec::ContainerExecutor::new(
            config.exec.podman.binary_name().to_string(),
            config.exec.podman.image.clone(),
            config.exec.podman.network.clone(),
        ));
        let k8s: DynExec = Arc::new(catalerum_exec::KubernetesExecutor::new(
            config.exec.k8s.namespace.clone(),
            config.exec.k8s.image.clone(),
        ));
        let mut backends = std::collections::HashMap::new();
        backends.insert(catalerum_core::model::ExecutorKind::Local, local.clone());
        backends.insert(catalerum_core::model::ExecutorKind::Sandbox, sandbox);
        backends.insert(catalerum_core::model::ExecutorKind::Container, container);
        backends.insert(catalerum_core::model::ExecutorKind::Kubernetes, k8s);
        info!(
            allow = ?config.exec.allow,
            podman = %config.exec.podman.binary_name(),
            "command executor enabled (run_command + terminals: local, sandbox, container, kubernetes; exec:run required)"
        );
        (Some(local), backends)
    } else {
        (None, std::collections::HashMap::new())
    };

    // --- Per-workspace sandbox (SOUL §20) -------------------------------------
    // `[exec].per_workspace` runs ONE long-lived, secure sandbox per workspace:
    // every terminal session + `run_command` execs into it (shared `/work`
    // volume), instead of a fresh container per call. The container (podman/
    // docker) backend is built here; the kubernetes backend declares a
    // `WorkspaceSandbox` CR for the catalerum-operator (a later slice).
    let sandbox: Option<Arc<dyn catalerum_exec::WorkspaceSandbox>> =
        if config.exec.enabled && config.exec.per_workspace {
            match config.exec.backend_kind() {
                catalerum_core::model::ExecutorKind::Container => {
                    info!(
                        podman = %config.exec.podman.binary_name(),
                        idle_secs = config.exec.sandbox_idle_timeout_secs(),
                        "per-workspace sandbox enabled (one container per workspace)"
                    );
                    Some(Arc::new(catalerum_exec::PodmanSandbox::new(
                        config.exec.podman.binary_name().to_string(),
                        config.exec.sandbox_spec(),
                    )))
                }
                catalerum_core::model::ExecutorKind::Kubernetes => {
                    info!(
                        idle_secs = config.exec.sandbox_idle_timeout_secs(),
                        "per-workspace sandbox enabled (one WorkspaceSandbox CR per workspace; \
                         requires the catalerum-operator + CRD installed)"
                    );
                    Some(Arc::new(catalerum_exec::K8sSandbox::new(
                        "kubectl".to_string(),
                        config.exec.sandbox_spec(),
                        config.exec.sandbox_idle_timeout_secs(),
                    )))
                }
                other => {
                    tracing::warn!(
                        backend = other.as_token(),
                        "[exec].per_workspace requires the container/kubernetes backend; ignoring"
                    );
                    None
                }
            }
        } else {
            None
        };

    // The §11 Phase-B inline-code runner (Boa-sandboxed JS for Code/Condition
    // nodes), installed on the worker's automation context below. When `[exec]` is
    // enabled it also delegates non-JS runtimes (`shell`/`python`/…) to the same
    // §20 executor; otherwise it is JS-only. Built here so the executor `Arc` can
    // be shared before `AppState::new` takes ownership of it; the tool-call host
    // bridge (`catalerum.callTool`) is attached below once the registry exists.
    let code_runner_base = match executor.as_ref() {
        Some(exec) => catalerum_script::ScriptCodeRunner::with_executor(exec.clone()),
        None => catalerum_script::ScriptCodeRunner::new(),
    };

    // --- External MCP servers (MCP client, SOUL §26) -------------------------
    // Connect to each configured MCP server over stdio (spawn, e.g. Playwright
    // MCP) or HTTP/SSE (a hosted server, with optional bearer/header/OAuth2-SSO
    // auth) and import its tools into the §7 registry as `{server}_{tool}`, each
    // gated on `mcp:use@{server}` (a protected scope, §19 — like `run_command`).
    // A server that fails to connect is logged and skipped so it never blocks boot.
    let mut mcp_tools: Vec<Arc<dyn catalerum_core::tool::Tool>> = Vec::new();
    for server in &config.mcp.servers {
        if !server.enabled || !server.is_configured() {
            continue;
        }
        let result = if server.is_http() {
            catalerum_mcp::load_http_server_tools(
                &server.name,
                &server.url,
                build_mcp_auth(&server.auth),
                &server.tools,
            )
            .await
        } else {
            catalerum_mcp::load_server_tools(
                &server.name,
                &server.command,
                &server.args,
                &server.env_pairs(),
                &server.tools,
            )
            .await
        };
        match result {
            Ok(tools) => {
                info!(
                    server = %server.name,
                    transport = %server.transport,
                    tools = tools.len(),
                    "external MCP server connected (mcp:use@{} required)",
                    server.name
                );
                mcp_tools.extend(tools);
            }
            Err(e) => tracing::warn!(
                server = %server.name,
                transport = %server.transport,
                error = %e,
                "external MCP server failed to connect; skipped"
            ),
        }
    }

    // --- Build app state -----------------------------------------------------
    // `AppState` owns the LLM tool registry (SOUL §7); the automation action
    // runner is a thin client of it, so the worker is wired + spawned after this.
    let listen = config.server.listen.clone();
    // The `[ocr]` engine chain (SOUL §7/§10) — built here (the tesseract probe
    // is async) and handed to `AppState` for the tool/route plus the ingest
    // worker's `OcrContext` below. `None` = OCR off (binary objects catalogue
    // no text, as before).
    let ocr_chain = catalerum_api::build_ocr_chain(&config.ocr, &llm).await;
    let state = AppState::new(
        store,
        iam,
        llm,
        bus,
        config,
        Some(fetcher),
        webhook_sender,
        searcher,
        executor,
        terminal_backends,
        sandbox,
        mcp_tools,
        ocr_chain,
    );

    // --- First pod heartbeat (pod-heartbeat follow-up, SOUL §20/§16 M7) ----------
    // Stamp this pod's liveness *before* boot reconcile and before we serve — i.e.
    // before this pod can create any terminal/sandbox row. Ordering matters: the
    // stale-pod reclaim sweep reclaims rows whose owning pod has a *stale* heartbeat,
    // so a pod's own rows must never look stale from the moment it can own them. By
    // writing the first heartbeat here (awaited, up front), any row this pod later
    // opens is backed by a fresh heartbeat, so no peer's concurrent sweep can reclaim
    // it. Best-effort: a write failure warns (the sweep's never-heartbeated rule
    // still protects a pod with no heartbeat row at all) — it must never crash boot.
    if let Err(e) = state
        .store()
        .pod_heartbeats()
        .heartbeat(state.pod_id())
        .await
    {
        tracing::warn!(error = %e, pod_id = %state.pod_id(), "initial pod heartbeat failed");
    }
    // Announce this pod's reachable address on the bus registry (cross-pod session
    // forwarding, SOUL §16 M7) before serving, so a peer can route to it from the
    // first request. Re-announced on the heartbeat clock below; a no-op when pod
    // comms are disabled (no `[secrets].master_key` / no advertisable address).
    state.announce_pod().await;

    // --- Boot reconcile: this pod's orphaned terminal/sandbox rows (SOUL §20/§16 M7) ---
    // Every interactive PTY / container / Pod handle lives in this process's memory,
    // so a restart leaves any still-`active` row *this pod owned* a phantom (`list`
    // would report it active while no session exists). Under the N-replica Deployment
    // we must reclaim ONLY this pod's rows (matched by `state.pod_id()`) plus legacy
    // NULL-pod rows — never a peer pod's live sessions. A permanently-dead pod's rows
    // are additionally self-healed by the periodic stale-pod reclaim sweep spawned
    // below (heartbeat + sweep loop); sticky `sessionAffinity` keeps a session on its
    // owning pod. Runs once here, before any reaper/worker spawns; non-fatal on error.
    match state
        .store()
        .terminal_sessions()
        .close_all_active_for_pod(state.pod_id())
        .await
    {
        Ok(0) => {}
        Ok(n) => {
            info!(count = n, pod_id = %state.pod_id(), "closed this pod's orphaned terminal sessions from a prior run")
        }
        Err(e) => tracing::warn!(error = %e, "terminal session boot reconcile failed"),
    }
    match state
        .store()
        .workspace_sandboxes()
        .mark_all_stopped_for_pod(state.pod_id())
        .await
    {
        Ok(0) => {}
        Ok(n) => {
            info!(count = n, pod_id = %state.pod_id(), "reconciled this pod's orphaned workspace sandboxes from a prior run")
        }
        Err(e) => tracing::warn!(error = %e, "workspace sandbox boot reconcile failed"),
    }

    // --- Reconnect DB-defined MCP servers (MCP client, SOUL §26) --------------
    // Config-file `[mcp.servers]` connected above (static); the runtime-managed
    // ones (created via the `*_mcp_server` tools, persisted in `mcp_servers`) are
    // reconnected here into the registry's live overlay so they survive a restart.
    // A failure is logged and skipped — it never blocks boot, and the agent can
    // retry via `edit_mcp_server`.
    match state.store().mcp_servers().list_enabled().await {
        Ok(servers) => {
            let manager = state.mcp_manager();
            for def in &servers {
                match manager.connect(def).await {
                    Ok(n) => info!(
                        server = %def.name,
                        transport = %def.transport,
                        tools = n,
                        "reconnected DB-defined MCP server"
                    ),
                    Err(e) => tracing::warn!(
                        server = %def.name,
                        error = %e,
                        "DB-defined MCP server failed to reconnect; skipped"
                    ),
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not list DB-defined MCP servers"),
    }

    // --- Pre-warm the tool-search index (SOUL §7) ----------------------------
    // Embed every registered tool now (static + the MCP tools connected above) so
    // the first `search_tools` call is instant. Best-effort: a transient embedder
    // outage just defers embedding to the first search (the index self-syncs).
    match state.tool_index().reconcile(state.registry()).await {
        Ok(n) => info!(embedded = n, "tool-search index pre-warmed"),
        Err(e) => {
            tracing::warn!(error = %e, "tool-search index pre-warm failed; will sync lazily")
        }
    }

    // --- Pre-warm the automation node-type index (SOUL §11) ------------------
    // Embed the static node-type catalog now so the first `search_automation_node_types`
    // call (and the editor's node search) is instant. Best-effort, like above.
    match state.node_index().reconcile().await {
        Ok(n) => info!(embedded = n, "automation node-type index pre-warmed"),
        Err(e) => {
            tracing::warn!(error = %e, "automation node-type index pre-warm failed; will sync lazily")
        }
    }

    // --- Pre-warm the internal-articles index (SOUL §11) ---------------------
    // Embed the static how-to article corpus now so the first `search_articles` call
    // (and the editor's article search) is instant. Best-effort, like above.
    match state.article_index().reconcile().await {
        Ok(n) => info!(embedded = n, "internal-articles index pre-warmed"),
        Err(e) => {
            tracing::warn!(error = %e, "internal-articles index pre-warm failed; will sync lazily")
        }
    }

    // --- Attach the automation runner + spawn the ingest worker --------------
    // Give the worker an `AutomationContext` so it actually runs `run_automation`
    // jobs (SOUL §11) — a matched trigger enqueues one; this is what executes it.
    // Interim authority (the §19 grants table + policy engine don't exist yet):
    // each automation runs **as its workspace's owner** under bounded base-Member
    // capabilities — ordinary read/write, never delete / exec / admin, so
    // protected scopes stay unreachable until a real grant says otherwise.
    // `with_llm` lets an `LlmAgent` action run the §7 agent loop against llmleaf
    // (SOUL §11/§7); the default model is used unless the action pins its own.
    let action_runner = catalerum_api::ToolActionRunner::workspace_owner_authority(
        state.registry().clone(),
        state.store().clone(),
    )
    .with_llm(
        state.llm().clone(),
        state.config().llm.default_model.clone(),
    )
    // Storage backends so the collect pipeline archives a collected message's raw
    // `.eml` + attachments as objects and links them onto the row (SOUL §9/§28/§29);
    // runtime backends + the per-user default files store resolve through the store.
    // A no-op when no store is configured (archival is opt-in, like chat uploads).
    .with_storage(std::sync::Arc::new(state.storage().clone()));
    // Let a code/condition node's JS call registry tools (`catalerum.callTool`) under
    // the automation's own authority — backed by the SAME `ToolActionRunner` that
    // runs Action nodes, so a code node reaches no tool an action node couldn't
    // (SOUL §11/§19). Attached here (post-`state`) since the host needs the registry.
    let code_runner: Arc<catalerum_script::ScriptCodeRunner> =
        Arc::new(code_runner_base.with_tool_host(Arc::new(action_runner.clone())));
    let mut ingest_worker = ingest_worker
        .with_automation_context(
            catalerum_ingest::AutomationContext::new(Arc::new(action_runner))
                // Install the real §11 Phase-B inline-code runner so a deployed
                // automation's JS Code/Condition nodes actually execute (the default is
                // `FailCodeRunner`, which fails them).
                .with_code_runner(code_runner),
        )
        // The encrypted secret store (SOUL §13) so the worker can build the
        // OAuth-backed Google calendar provider — its tokens are sealed behind the
        // connection's `credential_ref` (SOUL §16 M7). `None` when
        // `[secrets].master_key` is unset, in which case Google sources fail closed.
        .with_secret_store(state.secret_store().cloned())
        // The coordination bus (SOUL §6.6/§16 M7) so overlapping collect jobs for
        // one source SKIP instead of racing the same uncommitted ledger and fanning
        // out duplicate per-item runs across pods.
        .with_bus(state.bus().clone());
    // Object ingestion (SOUL §9/§10): when `[storage]` is configured, the worker
    // runs `ingest_object` jobs — extract a stored object's text into `documents`
    // + link `extracted_text_id`, and embed it when Qdrant is enabled — reusing
    // the backend `AppState` already built.
    let storage = state.storage();
    if !storage.is_empty() {
        // Provision every config backend's container on startup (creates the S3
        // bucket / WebDAV collection if absent; a no-op for local FS). Best-effort —
        // a failure is logged, not fatal (a container may be created out-of-band).
        for (name, _kind) in storage.infos() {
            if let Some(handle) = storage.get(&name) {
                if let Err(e) = handle.backend.ensure_container().await {
                    tracing::warn!(error = %e, store = %name, bucket = %handle.bucket,
                        "could not ensure storage container; uploads may fail");
                }
            }
        }
        // The object-ingest worker resolves each object's backend by its bucket's
        // connection: config backends from this map (keyed by connection name),
        // runtime (user-added) backends built on demand. The default store's
        // backend is the fallback for objects whose connection can't be resolved.
        let mut object_ctx =
            catalerum_ingest::ObjectIngestContext::new(storage.backends_by_connection())
                .with_browse_connections(storage.browse_connections());
        if let Some(handle) = state.storage_default_handle() {
            object_ctx = object_ctx.with_fallback(handle.backend.clone());
        }
        // Image/PDF OCR at ingest (SOUL §7/§10): the `[ocr]` chain built above,
        // shared with the tool/route. Config-level only — ingest jobs carry no
        // acting user, so the workspace's documents stay deterministic.
        if let Some(chain) = state.ocr() {
            let cfg = &state.config().ocr;
            object_ctx = object_ctx.with_ocr(
                catalerum_ingest::OcrContext::new(chain.clone())
                    .with_limits(cfg.max_image_bytes, cfg.max_document_bytes),
            );
            info!(engines = ?chain.engine_names(), "image OCR enabled for object ingestion");
        }
        ingest_worker = ingest_worker.with_object_context(object_ctx);
        info!(
            stores = storage.infos().len(),
            "object ingestion enabled (ingest_object jobs)"
        );
    }
    let _ingest_worker = ingest_worker.spawn();
    info!("ingest worker spawned (automation runner attached)");

    // --- Schedule worker (time-driven automations, SOUL §11) -----------------
    // A clock loop that enqueues work for the poll-driven triggers — `Schedule {
    // cron, tz }` crons, `CalendarEvent` lead reminders, (when a Neo4j is attached)
    // `GraphQuery` polls, and `CollectEmail`/`CollectCalendar` source-collect jobs
    // (SOUL §10/§28) — alongside the push sources (Kanban moves, webhooks). The
    // collect scan only *enqueues* a lightweight collect job per due trigger; the
    // sync worker below does the heavy provider pull + per-item fan-out off this
    // clock. No catch-up across restarts; each occurrence is single-fired across
    // pods via the bus's distributed lock (SOUL §11/§6.2).
    let mut schedule_worker =
        catalerum_ingest::ScheduleWorker::new(state.store().clone(), state.bus().clone());
    if let Some(graph) = schedule_graph {
        schedule_worker = schedule_worker.with_graph(graph);
        info!("schedule worker: GraphQuery polling enabled");
    }
    let _schedule_worker = schedule_worker.spawn();
    info!("schedule worker spawned (Schedule + CalendarEvent + GraphQuery + Collect triggers)");

    // --- MCP `GET /mcp` cross-pod push bridge (SOUL §26/§16 M7) --------------
    // Fan the per-workspace server→client SSE push hub out over the bus so a client
    // sees notifications from any pod, not just the one holding its stream. With the
    // in-process bus (single-pod dev) it stays local-only. No producer publishes yet.
    let _mcp_push_bridge = catalerum_api::install_mcp_push_bridge(state.bus().clone()).await;

    // --- Google Calendar push-channel worker (SOUL §8/§16 M7 — push half) ----
    // Opt-in (`[google].push`): registers/renews an `events.watch` channel per
    // Google-calendar connection that has an enabled `CollectCalendar` automation,
    // so a calendar change triggers a collect promptly instead of waiting for the
    // poll cadence. Needs a public https `[server].base_url` + `[secrets].master_key`
    // (the worker no-ops and warns otherwise). The poll cadence remains the
    // correctness fallback, so this is purely a latency optimization.
    let _google_watch_worker = if state.config().google.push && state.config().google.is_enabled() {
        let handle = catalerum_api::GoogleWatchWorker::new(state.clone()).spawn();
        info!("google watch worker spawned (Calendar push channels; [google].push enabled)");
        Some(handle)
    } else {
        None
    };

    // --- Storage watch worker (keep watched stores' §10 index in sync, §9/§10) ---
    // For every `watch`-enabled store it reconciles the catalogue with the backend
    // (catalogue + ingest new/changed files, purge vanished ones); local stores get
    // real-time inotify, remote stores re-scan on `[storage].watch_interval_secs`.
    // Cheap when nothing is watched (a periodic enumerate that finds no targets).
    let _storage_watch_worker = catalerum_api::StorageWatchWorker::new(state.clone()).spawn();
    info!("storage watch worker spawned (real-time local + periodic remote re-index)");

    // --- Terminal reaper (close self-exited sessions, SOUL §20) --------------
    // PTY/container/Pod handles live only in the `TerminalManager`'s memory, so a
    // shell that exits on its own (`exit`, a crash) would otherwise leak its
    // child + keep-alive container/Pod and a phantom `active` row until the next
    // restart. A light periodic pass reaps them. Only spawned when terminals are
    // configured; the boot reconcile (after migrate) handles prior-process rows.
    if let Some(terminals) = state.terminal_manager().cloned() {
        const REAP_INTERVAL_SECS: u64 = 30;
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(REAP_INTERVAL_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                match terminals.reap().await {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "reaped self-exited terminal sessions"),
                    Err(e) => tracing::warn!(error = %e, "terminal reaper pass failed"),
                }
            }
        });
        info!(
            interval_secs = REAP_INTERVAL_SECS,
            "terminal reaper spawned"
        );
    }

    // The per-workspace sandbox reaper (SOUL §20): reaps self-exited sessions and
    // destroys idle workspace containers (podman; the k8s operator GCs its own).
    if let Some(sandbox) = state.sandbox_manager().cloned() {
        let _sandbox_reaper = sandbox.spawn();
        info!("workspace sandbox reaper spawned");
    }

    // --- Pod heartbeat + stale-pod reclaim (pod-heartbeat follow-up, §20/§16 M7) ---
    // The pod-scoped boot reconcile only reclaims a pod's OWN rows on restart; under
    // the shipped Deployment a replaced pod gets a fresh random HOSTNAME and never
    // returns, so a permanently-dead pod's `active` terminal/sandbox rows would
    // otherwise linger forever. This loop closes that: every ~30 s each pod (a) writes
    // its heartbeat, (b) reclaims rows whose owning pod is provably dead — has a
    // heartbeat that has gone stale past a generous grace (5 min ≫ 30 s; a
    // paused-but-alive pod that resumes within grace keeps its rows), and (c) prunes
    // ancient heartbeat rows so the table stays bounded. The reclaim is rows-only
    // (the dead pod's PTYs/containers died with it), naturally idempotent (it only
    // touches non-closed rows), and never reclaims a pod with NO heartbeat row — so
    // a pre-heartbeat-code pod mid-rolling-upgrade is left to the legacy boot path
    // (see `reclaim_stale_for_dead_pods` docs). Runs on every pod regardless of
    // `[exec]` config (a no-op when the tables are empty); all failures warn.
    {
        const HEARTBEAT_INTERVAL_SECS: u64 = 30;
        const STALE_GRACE: std::time::Duration = std::time::Duration::from_secs(5 * 60);
        const HEARTBEAT_HORIZON: std::time::Duration =
            std::time::Duration::from_secs(7 * 24 * 60 * 60);
        let store = state.store().clone();
        let pod_id = state.pod_id().to_string();
        let announce_state = state.clone();
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let heartbeats = store.pod_heartbeats();
                if let Err(e) = heartbeats.heartbeat(&pod_id).await {
                    tracing::warn!(error = %e, "pod heartbeat refresh failed");
                }
                // Refresh the TTL'd pod-address announcement (cross-pod session
                // forwarding, SOUL §16 M7) on the same clock.
                announce_state.announce_pod().await;
                match store
                    .terminal_sessions()
                    .reclaim_stale_for_dead_pods(STALE_GRACE)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "reclaimed terminal sessions from dead pods"),
                    Err(e) => tracing::warn!(error = %e, "terminal stale-pod reclaim failed"),
                }
                match store
                    .workspace_sandboxes()
                    .reclaim_stale_for_dead_pods(STALE_GRACE)
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "reclaimed workspace sandboxes from dead pods"),
                    Err(e) => tracing::warn!(error = %e, "sandbox stale-pod reclaim failed"),
                }
                match heartbeats.prune(HEARTBEAT_HORIZON).await {
                    Ok(0) => {}
                    Ok(n) => info!(count = n, "pruned ancient pod heartbeat rows"),
                    Err(e) => tracing::warn!(error = %e, "pod heartbeat prune failed"),
                }
            }
        });
        info!(
            interval_secs = HEARTBEAT_INTERVAL_SECS,
            grace_secs = STALE_GRACE.as_secs(),
            "pod heartbeat + stale-pod reclaim loop spawned"
        );
    }

    // Email/calendar ingestion is no longer an always-on poller (SOUL §10/§28,
    // revised): a user-authored automation headed by a `CollectEmail` /
    // `CollectCalendar` trigger pulls its provider on a cadence (the collect jobs the
    // schedule worker above enqueues, run by the sync worker that already holds the
    // automation context). Adding a connection (`POST /email/connections`) provisions
    // nothing; until a Collect automation exists the source is dormant. The old
    // `EmailSyncWorker` + `[email]` config were removed with that change.

    // --- Channel listener (inbound chat → ChannelMessage triggers, SOUL §11/§25) -
    // Subscribe to every inbound-enabled channel (Matrix `/sync`, Telegram
    // `getUpdates`; `[channels.*].inbound = true`) and dispatch a `ChannelMessage`
    // trigger per message, so an agent replies on the same room/chat — the
    // multiplayer loop. Like the `[email]` pre-seed, static `[channels]` config
    // binds to the default workspace. Empty (no inbound channel) → not started.
    let inbound_channels = state.inbound_channels().clone();
    if !inbound_channels.is_empty() {
        match state
            .store()
            .workspaces()
            .get_by_slug(DEFAULT_WORKSPACE_SLUG)
            .await
        {
            Ok(ws) => {
                let names: Vec<String> = inbound_channels.keys().cloned().collect();
                let _channel_listener = catalerum_api::ChannelListener::new(
                    state.store().clone(),
                    ws.id,
                    inbound_channels,
                    state.bus().clone(),
                )
                .spawn();
                info!(
                    channels = ?names,
                    workspace = %ws.id,
                    "channel listener spawned (inbound → ChannelMessage triggers)"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "default workspace absent; channel listener not started")
            }
        }
    }

    // --- Backup worker (scheduled disaster-recovery, SOUL §30) ---------------
    // When `[backup].enabled`, spawn a clock loop that periodically dumps Postgres
    // (the source of truth, §6.1) + copies the object blobs (§9) to an independent
    // `[backup.destination]` (S3 / WebDAV / local), single-firing each window
    // across pods via the bus lock (§6.2) and pruning to `keep`. Derived stores
    // (Neo4j/Qdrant) are rebuildable, so they're not backed up (§6.3/§6.4). The
    // on-demand `catalerum backup`/`restore` subcommands run the same engine.
    #[cfg(not(feature = "sqlite"))]
    if state.config().backup.enabled {
        match catalerum_api::build_storage_backend(&state.config().backup.destination) {
            Some(dest) => {
                let mut engine = catalerum_backup::BackupEngine::new(
                    state.store().pool().clone(),
                    dest,
                    env!("CARGO_PKG_VERSION"),
                )
                .with_prefix(state.config().backup.prefix_name())
                .with_keep(state.config().backup.keep())
                .with_include_objects(state.config().backup.include_objects);
                // Backup mirrors every **config** store's blobs (SOUL §30), each
                // under its own `objects/<name>/`. Runtime (user-added) backends —
                // DB connection rows — are not yet mirrored (tracked follow-up).
                for (name, backend) in state.storage().sources() {
                    engine = engine.with_named_source(name, backend);
                }
                let interval = state.config().backup.interval();
                let _backup_worker = catalerum_backup::BackupWorker::new(
                    Arc::new(engine),
                    state.bus().clone(),
                    interval,
                )
                .spawn();
                info!(
                    prefix = state.config().backup.prefix_name(),
                    interval_secs = interval.as_secs(),
                    keep = state.config().backup.keep(),
                    include_objects = state.config().backup.include_objects,
                    "backup worker spawned (scheduled Postgres + object backup, SOUL §30)"
                );
            }
            None => tracing::warn!(
                "[backup].enabled = true but no [backup.destination] configured; backups disabled"
            ),
        }
    }
    #[cfg(feature = "sqlite")]
    if state.config().backup.enabled {
        tracing::warn!(
            "scheduled logical backup is PostgreSQL-only; back up the all-in-one SQLite data volume instead"
        );
    }

    // One-glance backing-services summary to stdout (the `just dev` terminal),
    // mirroring the Settings → Status panel (`GET /status`) so you can see what's
    // wired without a round-trip.
    print_status_summary(&state);

    let app = build_router(state);

    info!(addr = %listen, "listening");
    serve(&listen, app)
        .await
        .with_context(|| format!("serving on {listen}"))?;

    Ok(())
}

/// Translate a server's `[mcp.servers.auth]` config into an HTTP-MCP
/// [`AuthProvider`](catalerum_mcp::AuthProvider) (SOUL §26). An unknown/empty
/// `kind` is `none` (deny-safe: no credential is sent rather than a wrong one).
/// Secrets are exposed here, at the single point a client is built.
fn build_mcp_auth(cfg: &McpAuthConfig) -> Arc<dyn catalerum_mcp::AuthProvider> {
    match cfg.kind.trim().to_ascii_lowercase().as_str() {
        "bearer" => catalerum_mcp::auth::bearer(cfg.token.expose().to_string()),
        "header" => catalerum_mcp::auth::header(
            cfg.header_name.clone(),
            cfg.header_value.expose().to_string(),
        ),
        "oauth2" => catalerum_mcp::auth::oauth2(catalerum_mcp::OAuth2Params {
            token_url: cfg.token_url.clone(),
            grant_type: if cfg.grant_type.trim().is_empty() {
                "client_credentials".to_string()
            } else {
                cfg.grant_type.clone()
            },
            client_id: cfg.client_id.clone(),
            client_secret: cfg.client_secret.expose().to_string(),
            refresh_token: cfg.refresh_token.expose().to_string(),
            scope: cfg.scope.clone(),
        }),
        _ => catalerum_mcp::auth::none(),
    }
}

/// Build the web fetcher from `[fetch]` config (SOUL §27). The local-first HTTP
/// backend is always present; Firecrawl and (with the `browser` build feature)
/// the CDP backend are added when configured. `Auto` requests resolve to the
/// `backend` field, falling back toward HTTP when a choice isn't available.
/// Build the outbound webhook sender from the same `[fetch]` config (SOUL
/// §11/§27): one egress policy governs fetching *and* delivering, so
/// `allow_private_hosts` opts both in (or neither). Backs the `send_webhook`
/// tool + the `Webhook` automation action; a build failure logs and disables
/// delivery rather than aborting boot.
fn build_webhook_sender(cfg: &FetchConfig) -> Option<Arc<dyn WebhookSender>> {
    let policy = FetchPolicy {
        allow_private_hosts: cfg.allow_private_hosts,
        max_bytes: cfg.max_bytes,
    };
    match HttpWebhookSender::new(cfg.user_agent.as_deref(), cfg.timeout_secs, policy) {
        Ok(sender) => Some(Arc::new(sender)),
        Err(e) => {
            tracing::warn!(error = %e, "failed to build webhook sender; send_webhook disabled");
            None
        }
    }
}

fn build_fetcher(cfg: &FetchConfig) -> anyhow::Result<Arc<dyn WebFetcher>> {
    let policy = FetchPolicy {
        allow_private_hosts: cfg.allow_private_hosts,
        max_bytes: cfg.max_bytes,
    };
    let http = HttpFetcher::new(cfg.user_agent.as_deref(), cfg.timeout_secs, policy.clone())?;
    let default = BackendKind::parse(&cfg.backend).unwrap_or(BackendKind::Http);

    let mut fetcher = MultiFetcher::new(http, default);
    if cfg.firecrawl.is_enabled() {
        fetcher = fetcher.with_firecrawl(FirecrawlFetcher::new(
            Some(cfg.firecrawl.base_url.as_str()),
            cfg.firecrawl.api_key.expose(),
            policy.clone(),
        )?);
    } else if matches!(default, BackendKind::Firecrawl) {
        tracing::warn!(
            "fetch.backend = firecrawl but no firecrawl.api_key set; falling back to http"
        );
    }

    #[cfg(feature = "browser")]
    if cfg.browser.is_enabled() {
        fetcher = fetcher.with_browser(catalerum_fetch::browser::CdpFetcher::new(
            cfg.browser.cdp_url.clone(),
            cfg.timeout_secs,
            policy,
        ));
    }
    #[cfg(not(feature = "browser"))]
    if matches!(default, BackendKind::Browser) || cfg.browser.is_enabled() {
        tracing::warn!(
            "fetch browser backend requested but the `browser` build feature is off; \
             rebuild with `--features browser`. Falling back to http/firecrawl."
        );
    }

    Ok(Arc::new(fetcher))
}

/// Build the web searcher from `[search]` config (SOUL §27). Each provider client
/// is constructed only when its credential is set (for SearXNG, its `base_url`);
/// they are routed by a [`MultiSearcher`] whose default is `[search].backend`.
/// Returns `None` when no provider is configured — then the `web_search` tool is
/// simply not registered (parity with `fetch_url` and an unset fetch backend).
fn build_searcher(cfg: &SearchConfig) -> Option<Arc<dyn WebSearcher>> {
    let mut backends: Vec<Arc<dyn WebSearcher>> = Vec::new();
    // Construct each enabled backend. `new()` only fails to build the shared HTTP
    // client (effectively never); a failure logs and skips that provider rather
    // than aborting boot, so one bad client can't take down search.
    macro_rules! add {
        ($enabled:expr, $id:literal, $build:expr) => {
            if $enabled {
                match $build {
                    Ok(b) => backends.push(Arc::new(b) as Arc<dyn WebSearcher>),
                    Err(e) => tracing::warn!(provider = $id, error = %e,
                        "failed to build search provider; skipped"),
                }
            }
        };
    }
    add!(
        cfg.brave.is_enabled(),
        "brave",
        catalerum_search::BraveSearcher::new(cfg.brave.api_key.expose())
    );
    add!(
        cfg.tavily.is_enabled(),
        "tavily",
        catalerum_search::TavilySearcher::new(cfg.tavily.api_key.expose())
    );
    add!(
        cfg.exa.is_enabled(),
        "exa",
        catalerum_search::ExaSearcher::new(cfg.exa.api_key.expose())
    );
    add!(
        cfg.searxng.is_enabled(),
        "searxng",
        catalerum_search::SearxngSearcher::new(cfg.searxng.base_url.clone())
    );
    add!(
        cfg.google.is_enabled(),
        "google",
        catalerum_search::GoogleSearcher::new(cfg.google.api_key.expose(), cfg.google.cx.clone())
    );
    add!(
        cfg.serpapi.is_enabled(),
        "serpapi",
        catalerum_search::SerpApiSearcher::new(
            cfg.serpapi.api_key.expose(),
            cfg.serpapi.engine.clone()
        )
    );

    if backends.is_empty() {
        return None;
    }
    // Resolve the default backend: the configured `backend` if it's actually
    // wired, else the first enabled provider (so a no-`provider` search always
    // hits something usable rather than erroring).
    let default = if backends.iter().any(|b| b.name() == cfg.backend) {
        cfg.backend.clone()
    } else {
        let fallback = backends[0].name().to_string();
        tracing::warn!(
            configured = %cfg.backend, using = %fallback,
            "search.backend is not an enabled provider; using the first enabled one"
        );
        fallback
    };
    Some(Arc::new(MultiSearcher::new(backends, default)) as Arc<dyn WebSearcher>)
}

/// Run the external MCP server over stdio (SOUL §26): the same scoped tool
/// registry the chat agent + REST use, in MCP clothing — no backdoor (principle
/// 15). Builds a minimal stack (store + IAM + the registry; no HTTP, no workers,
/// fetch/exec off), scopes it (`mcp_scope`), and serves JSON-RPC on stdout.
async fn run_mcp(config: Config) -> anyhow::Result<()> {
    let state = build_mcp_state(config).await?;
    let ctx = mcp_scope(&state).await?;
    let workspace_id = ctx
        .workspace_id
        .context("MCP scope is missing a workspace")?;
    // Expose the workspace's skills (§23) as MCP prompts and its notes/tasks as MCP
    // resources (read views) alongside the tools (§26). The same providers back the
    // `POST /mcp` HTTP route (`catalerum_api::routes::mcp`) — one definition, two
    // transports.
    let prompts = SkillPromptProvider::new(state.store().clone(), workspace_id);
    let resources = WorkspaceResourceProvider::new(state.store().clone(), workspace_id);
    let server = McpServer::new(state.registry().clone(), ctx)
        .with_prompts(std::sync::Arc::new(prompts))
        .with_resources(std::sync::Arc::new(resources));

    info!("catalerum MCP server ready (stdio); JSON-RPC on stdout, logs on stderr");
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    catalerum_mcp::serve(&server, stdin, tokio::io::stdout())
        .await
        .context("serving MCP over stdio")?;
    Ok(())
}

/// Build the minimal stack the `mcp`/`token` subcommands share: store + IAM + the
/// §7 `ToolRegistry` (`AppState` owns it). No HTTP, no workers; fetch/exec `None`
/// (not exposed over MCP by default — a later, capability-gated decision).
async fn build_mcp_state(config: Config) -> anyhow::Result<AppState> {
    let store = Store::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to Postgres at {}", config.database.url))?;
    store.migrate().await.context("running store migrations")?;
    let iam_store = PgIamStore::new(store.pool().clone());
    let iam = IamService::new(iam_store).with_base_url(config.server.effective_base_url());
    let llm = build_llm_client(&config);
    let bus = Bus::in_process();
    // The MCP stack still serves `ocr_document`, so build the `[ocr]` chain here
    // too (cheap when unconfigured: no probe runs with tesseract disabled, and
    // the default probe is one `--list-langs` exec).
    let ocr_chain = catalerum_api::build_ocr_chain(&config.ocr, &llm).await;
    Ok(AppState::new(
        store,
        iam,
        llm,
        bus,
        config,
        None,
        None,
        None,
        None,
        std::collections::HashMap::new(),
        None,
        Vec::new(),
        ocr_chain,
    ))
}

/// Resolve the MCP client's scope (SOUL §18/§26). A `CATALERUM_MCP_TOKEN` bearer (a
/// service token / session) scopes MCP to **that principal**, deny-by-default under
/// its role's capabilities — the production path, no dev-login needed and no
/// backdoor (principle 15). Absent, the zero-config dev fallback scopes to the
/// default workspace's owner; with neither, it errors.
async fn mcp_scope(state: &AppState) -> anyhow::Result<ToolContext> {
    if let Some(token) = std::env::var("CATALERUM_MCP_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
    {
        let p = state
            .iam()
            .verify_bearer(token.trim())
            .await
            .context("CATALERUM_MCP_TOKEN is invalid or expired")?;
        info!(workspace = %p.workspace_id, user = %p.user_id, role = ?p.role, "MCP scoped to the token's principal");
        return Ok(scope_for(p.workspace_id, p.user_id, p.role));
    }
    if !state.config().auth.dev_login {
        anyhow::bail!(
            "no CATALERUM_MCP_TOKEN set and dev-login is off — mint one with `catalerum token` \
             (or enable dev-login)"
        );
    }
    let (workspace_id, user_id, role) = dev_workspace_owner(state).await?;
    info!(workspace = %workspace_id, user = %user_id, "MCP scoped to the dev workspace owner");
    Ok(scope_for(workspace_id, user_id, role))
}

/// A [`ToolContext`] scoped to `(workspace, user)` under `role`'s base capabilities.
fn scope_for(workspace_id: WorkspaceId, user_id: UserId, role: Role) -> ToolContext {
    ToolContext {
        workspace_id: Some(workspace_id),
        user_id: Some(user_id),
        capabilities: Some(base_capabilities(role)),
        ..Default::default()
    }
}

/// Zero-config dev: seed the admin + default workspace + first-party skills, then
/// resolve that workspace's owner `(workspace, user, role)`.
async fn dev_workspace_owner(state: &AppState) -> anyhow::Result<(WorkspaceId, UserId, Role)> {
    state
        .iam()
        .ensure_dev_login()
        .await
        .context("seeding dev login")?;
    let ws = state
        .store()
        .workspaces()
        .get_by_slug(DEFAULT_WORKSPACE_SLUG)
        .await
        .context("default workspace not found")?;
    // Seed first-party skills (§23) so they surface as MCP prompts (§26).
    if let Err(e) = catalerum_skills::seed_first_party(state.store(), ws.id).await {
        tracing::warn!(error = %e, "could not seed first-party skills for MCP");
    }
    let members = state.store().memberships().list_by_workspace(ws.id).await?;
    let owner = members
        .iter()
        .find(|m| m.role == Role::Owner)
        .or_else(|| members.first())
        .ok_or_else(|| anyhow::anyhow!("default workspace has no members"))?;
    Ok((ws.id, owner.user_id, owner.role))
}

/// Mint (or, with `revoke`, revoke) a long-lived service token for the dev
/// workspace owner (SOUL §18/§26). A minted token's raw bearer is printed to stdout
/// (info on stderr) — set it as `CATALERUM_MCP_TOKEN` for `catalerum mcp`, or send
/// it as an API `Authorization: Bearer`. With `grant`, the token is **scoped** to a
/// named §19 grant (gated by attenuation) so it carries the grant's capabilities
/// rather than the owner's full role.
async fn run_token(
    config: Config,
    ttl_days: i64,
    revoke: Option<String>,
    grant: Option<String>,
) -> anyhow::Result<()> {
    if !config.auth.dev_login {
        anyhow::bail!("`catalerum token` mints a dev workspace-owner token; enable dev-login");
    }
    let state = build_mcp_state(config).await?;

    if let Some(token) = revoke {
        let existed = state
            .iam()
            .revoke_session(token.trim())
            .await
            .context("revoking service token")?;
        eprintln!(
            "{}",
            if existed {
                "token revoked"
            } else {
                "token not found (already gone or invalid)"
            }
        );
        return Ok(());
    }

    let (workspace_id, user_id, role) = dev_workspace_owner(&state).await?;

    // Optionally scope the token to a named §19 grant (SOUL §19/§26), gated by the
    // attenuation invariant: the grant must be ⊆ the owner's own authority, so a
    // scoped token is strictly *less* than a full-role one (never an escalation).
    let grant_id = match grant.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(reference) => {
            let grant = resolve_cli_grant(&state, workspace_id, reference).await?;
            let ceiling = base_capabilities(role);
            for cap in &grant.capabilities {
                if catalerum_core::capability::attenuate(&ceiling, cap).is_err() {
                    anyhow::bail!(
                        "grant `{}` exceeds the dev workspace owner's authority; cannot mint",
                        grant.name
                    );
                }
            }
            eprintln!("scoped to grant `{}` ({})", grant.name, grant.id);
            Some(grant.id)
        }
        None => None,
    };

    let session = state
        .iam()
        .issue_session_with_ttl_days(workspace_id, user_id, role, grant_id, ttl_days)
        .await
        .context("minting service token")?;
    eprintln!(
        "service token for workspace {workspace_id} (role {role:?}), expires {}",
        session.expires_at
    );
    // The token alone on stdout, so `TOKEN=$(catalerum token)` captures just it.
    println!("{}", session.token);
    Ok(())
}

/// Resolve a `catalerum token --grant` reference (a grant id **or** name) to a
/// grant in the dev workspace (SOUL §19). Errors if no such grant exists.
async fn resolve_cli_grant(
    state: &AppState,
    workspace_id: WorkspaceId,
    reference: &str,
) -> anyhow::Result<catalerum_core::model::Grant> {
    if let Ok(id) = reference.parse::<catalerum_core::GrantId>() {
        if let Ok(grant) = state.store().grants().get(workspace_id, id).await {
            return Ok(grant);
        }
    }
    let grants = state
        .store()
        .grants()
        .list_by_workspace(workspace_id)
        .await
        .context("listing grants")?;
    grants
        .into_iter()
        .find(|g| g.name == reference)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no grant `{reference}` in the dev workspace — define one via `POST /grants` first"
            )
        })
}

/// Build the on-demand backup engine (SOUL §30): connect + migrate Postgres,
/// resolve the `[backup.destination]` backend (errors if none), and attach every
/// config `[storage]` store as a named blob source (matching the running server's
/// registry naming, so a CLI backup/restore covers the same stores). Shared by
/// `catalerum backup` and `catalerum restore`.
#[cfg(not(feature = "sqlite"))]
async fn build_backup_engine(config: &Config) -> anyhow::Result<catalerum_backup::BackupEngine> {
    let store = Store::connect(&config.database.url)
        .await
        .with_context(|| format!("connecting to Postgres at {}", config.database.url))?;
    store.migrate().await.context("running store migrations")?;
    let dest = catalerum_api::build_storage_backend(&config.backup.destination).context(
        "no [backup.destination] configured — set [backup.destination].local_path, \
         [backup.destination.s3], or [backup.destination.webdav] to choose where backups go",
    )?;
    let mut engine =
        catalerum_backup::BackupEngine::new(store.pool().clone(), dest, env!("CARGO_PKG_VERSION"))
            .with_prefix(config.backup.prefix_name())
            .with_keep(config.backup.keep())
            .with_include_objects(config.backup.include_objects);
    // Every config store is a named blob source the backup copies in — same names
    // the server's registry uses, so restore reunites each store's blobs with the
    // right backend. Runtime (user-added) stores aren't enumerated on this path.
    for (name, bcfg) in config.storage.resolved_backends() {
        if let Some(backend) = catalerum_api::build_backend(&bcfg, &name) {
            engine = engine.with_named_source(name, backend);
        }
    }
    Ok(engine)
}

/// `catalerum backup`: run one backup + retention prune now, printing the summary.
#[cfg(not(feature = "sqlite"))]
async fn run_backup(config: Config) -> anyhow::Result<()> {
    let engine = build_backup_engine(&config).await?;
    let summary = engine.run().await.context("running backup")?;
    println!(
        "backup {} complete: {} tables, {} rows, {} objects ({} KiB compressed Postgres dump)",
        summary.id,
        summary.tables,
        summary.rows,
        summary.objects,
        summary.postgres_bytes / 1024,
    );
    match engine.prune().await {
        Ok(n) if n > 0 => println!("pruned {n} old backup(s) (keep = {})", config.backup.keep()),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "backup prune failed"),
    }
    Ok(())
}

#[cfg(feature = "sqlite")]
async fn run_backup(_config: Config) -> anyhow::Result<()> {
    anyhow::bail!(
        "logical backup is PostgreSQL-only; snapshot the all-in-one /data volume (SQLite uses WAL)"
    )
}

/// `catalerum restore [id] [--yes] [--force]`: list backups (no id) or restore
/// one (destructive — replaces Postgres + blobs).
#[cfg(not(feature = "sqlite"))]
async fn run_restore(
    config: Config,
    id: Option<String>,
    yes: bool,
    force: bool,
) -> anyhow::Result<()> {
    let engine = build_backup_engine(&config).await?;
    let Some(id) = id else {
        // No id → non-destructive listing.
        let ids = engine.list().await.context("listing backups")?;
        if ids.is_empty() {
            println!(
                "no backups found under destination prefix `{}`",
                config.backup.prefix_name()
            );
        } else {
            println!("available backups (oldest → newest):");
            for id in ids {
                println!("  {id}");
            }
            println!("\nrestore with:  catalerum restore <BACKUP_ID> --yes");
        }
        return Ok(());
    };
    if !yes {
        anyhow::bail!(
            "restore is destructive — it REPLACES the current Postgres contents and object blobs \
             with backup `{id}`. Re-run with --yes to confirm."
        );
    }
    let summary = engine
        .restore(&id, force)
        .await
        .with_context(|| format!("restoring backup {id}"))?;
    println!(
        "restored backup {}: {} rows, {} objects",
        summary.id, summary.rows, summary.objects
    );
    println!(
        "note: Neo4j + Qdrant are derived indexes and were NOT restored — rebuild them from \
         Postgres via re-ingest (SOUL §6.3/§6.4)."
    );
    Ok(())
}

#[cfg(feature = "sqlite")]
async fn run_restore(
    _config: Config,
    _id: Option<String>,
    _yes: bool,
    _force: bool,
) -> anyhow::Result<()> {
    anyhow::bail!(
        "logical restore is PostgreSQL-only; restore the all-in-one /data volume while the container is stopped"
    )
}

/// Load config from `path` (if it exists) and apply env overrides. A missing
/// file falls back to built-in defaults so `catalerum` runs zero-config.
fn load_config(path: &PathBuf) -> anyhow::Result<Config> {
    let config = if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str::<Config>(&text)
            .with_context(|| format!("parsing config {}", path.display()))?
    } else {
        eprintln!(
            "config {} not found; using built-in defaults",
            path.display()
        );
        Config::default()
    };
    Ok(config.with_env_overrides())
}

fn build_llm_client(config: &Config) -> OpenRouterClient {
    let convert = |content| match content {
        TelemetryContent::MetadataOnly => TraceContent::MetadataOnly,
        TelemetryContent::AllExceptSystemPrompts => TraceContent::AllExceptSystemPrompts,
        TelemetryContent::Everything => TraceContent::Everything,
    };
    let trace = LlmTraceConfig {
        otlp: config
            .telemetry
            .otlp
            .enabled
            .then(|| convert(config.telemetry.otlp.content)),
        langfuse: config
            .telemetry
            .langfuse
            .enabled
            .then(|| convert(config.telemetry.langfuse.content)),
    };
    OpenRouterClient::builder()
        .base_url(&config.llm.base_url)
        .api_key(config.llm.api_key.expose())
        .tracing(trace)
        .build()
}

/// Print a one-glance backing-services summary to stdout at boot, mirroring the
/// Settings → Status panel (`GET /status`): the same `name / detail / state`
/// columns, but **config-derived** rather than live-probed — at boot the echo
/// llmleaf `just dev` backgrounds may not have come up yet, so a probe would race.
/// Optional derived stores read `disabled` until `[qdrant]`/`[neo4j]` are enabled;
/// enabled-but-unbuildable (bad URL) reads `down`. Non-secret (URLs only).
fn print_status_summary(state: &AppState) {
    let cfg = state.config();

    #[cfg(not(feature = "sqlite"))]
    let database = ("Postgres", "source of truth".to_string(), "up");
    #[cfg(feature = "sqlite")]
    let database = ("SQLite", "source of truth (single-node)".to_string(), "up");

    // (name, detail, state) — the Settings → Status column order.
    let rows: [(&str, String, &str); 5] = [
        database,
        ("LLM gateway", state.llm().base_url().to_string(), "up"),
        (
            "Coordination bus",
            if state.bus().is_distributed() {
                "Valkey / Redis (distributed)".to_string()
            } else {
                "in-process (single-node)".to_string()
            },
            "up",
        ),
        if cfg.qdrant.enabled {
            match state.vector() {
                Some(_) => ("Qdrant (vectors)", cfg.qdrant.url.clone(), "up"),
                None => (
                    "Qdrant (vectors)",
                    "configured but unavailable".to_string(),
                    "down",
                ),
            }
        } else {
            ("Qdrant (vectors)", "not configured".to_string(), "disabled")
        },
        if cfg.neo4j.enabled {
            if state.graph_available() {
                ("Neo4j (graph)", cfg.neo4j.url.clone(), "up")
            } else {
                (
                    "Neo4j (graph)",
                    "configured but unavailable".to_string(),
                    "down",
                )
            }
        } else {
            ("Database graph", "relational fallback".to_string(), "up")
        },
    ];

    let name_w = rows.iter().map(|(n, ..)| n.len()).max().unwrap_or(0);
    let detail_w = rows.iter().map(|(_, d, _)| d.len()).max().unwrap_or(0);

    println!("  Backing services:");
    for (name, detail, st) in &rows {
        println!("    {name:<name_w$}   {detail:<detail_w$}   {st}");
    }
    println!();
}

fn print_banner() {
    println!(
        r#"
  _____      _        _
 / ____|    | |      | |
| |     __ _| |_ __ _| | ___ _ __ _   _ _ __ ___
| |    / _` | __/ _` | |/ _ \ '__| | | | '_ ` _ \
| |___| (_| | || (_| | |  __/ |  | |_| | | | | | |
 \_____\__,_|\__\__,_|_|\___|_|   \__,_|_| |_| |_|

 catalerum — a catalogue of things
 self-hostable, automated, fully-integrated LLM assistant
 v{version}
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
}
