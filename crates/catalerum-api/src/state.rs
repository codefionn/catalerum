//! Shared application state (SOUL §12).
//!
//! [`AppState`] carries everything a handler needs: the Postgres store (truth),
//! the IAM service (auth + workspace scoping), the llmleaf chat client, the
//! Valkey/in-process bus (token relay), and the parsed config. It is `Clone`
//! (all fields are cheap `Arc`-backed handles) and lives as Axum router state.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use catalerum_bus::Bus;
use catalerum_channels::{
    Channel, DiscordWebhookChannel, MatrixChannel, SlackWebhookChannel, TelegramChannel,
};
use catalerum_core::provider::{
    strip_workspace_key, workspace_object_key, Embedder, Executor, Previewer, StorageBackend,
    WebFetcher, WebSearcher, WebhookSender,
};
use catalerum_core::tool::{Tool, ToolRegistry};

use crate::article_index::ArticleIndex;
use crate::mcp_manager::McpManager;
use crate::node_index::NodeDocIndex;
use crate::sandbox::WorkspaceSandboxManager;
use crate::subagent_runs::{register_subagent_run_tools, SubagentRunManager};
use crate::terminal::{register_terminal_tools, TerminalManager};
use crate::tool_index::ToolIndex;
use catalerum_core::{ConversationId, EventId, ExecutorKind, LinkId, NoteId, UserId, WorkspaceId};
use catalerum_exec::WorkspaceSandbox;
use catalerum_iam::{IamService, PgIamStore};
use catalerum_llm::{OpenRouterClient, VisionOcr};
use catalerum_ocr::{FallbackOcr, MistralOcr, TesseractOcr};

use crate::preview_client::HttpPreviewer;
use catalerum_storage::{LocalFsBackend, S3Backend, WebDavBackend};
use catalerum_store::Store;
use catalerum_vector::VectorStore;

use crate::config::Config;
use crate::download_link::DownloadSigner;
use crate::external_db::ExternalDbRegistry;
use crate::google_channel_link::GoogleChannelSigner;
use crate::google_oauth_state::GoogleStateSigner;
use crate::mcp_endpoint_link::EndpointSigner;
use crate::personalization_cache::{PersonalizationCache, DEFAULT_TTL};
use crate::sso_state::SsoStateSigner;
use crate::tools::{
    build_registry, register_web_search_tool, GraphQuery, NoteIngest, SemanticSearch,
};
use crate::trigger_link::TriggerSigner;
use catalerum_store::SecretStore;

/// The concrete IAM service used by the API: an [`IamService`] backed by the
/// Postgres IAM store.
pub type Iam = IamService<PgIamStore>;

/// The catalogue connection name backing the **default** store — kept as the
/// legacy value so objects catalogued before multi-backend support stay attached
/// to the same connection + bucket (SOUL §6.1/§9).
pub const DEFAULT_STORAGE_CONNECTION: &str = "local-storage";

/// A resolved, live storage backend a file op reads/writes through (SOUL §9): the
/// backend itself plus where its objects are catalogued. One per `?store=`
/// selection — a workspace can hold many, so this is no longer the *single* store.
#[derive(Clone)]
pub struct StorageHandle {
    pub backend: Arc<dyn StorageBackend>,
    /// The user-facing store name (the `?store=` selector value).
    pub store: String,
    /// The catalogue connection name objects under this store attach to.
    pub connection: String,
    /// The bucket name objects are catalogued under.
    pub bucket: String,
    /// Whether object keys are **workspace-namespaced** on this backend (SOUL
    /// §18): `true` (the default) prepends `<workspace_id>/` to every physical
    /// key for tenant isolation; `false` (a *browse* store, see
    /// [`StorageBackendConfig::browse`](crate::config::StorageBackendConfig)) uses
    /// the raw key so an existing on-disk directory's files are visible as-is.
    pub namespaced: bool,
}

impl StorageHandle {
    /// The physical backend key for a user-facing `key` in `workspace`: the
    /// namespaced `<workspace_id>/key` on an isolated store, or the raw `key` on a
    /// browse store. The single place the §18 namespacing decision is applied for
    /// the storage routes — its inverse is [`user_key`](Self::user_key).
    #[must_use]
    pub fn physical_key(&self, workspace: WorkspaceId, key: &str) -> String {
        if self.namespaced {
            workspace_object_key(workspace, key)
        } else {
            key.trim_start_matches('/').to_string()
        }
    }

    /// The user-facing key for a `physical` backend key (the inverse of
    /// [`physical_key`](Self::physical_key)): strips the `<workspace_id>/` prefix on
    /// an isolated store, or returns the key unchanged on a browse store.
    #[must_use]
    pub fn user_key(&self, workspace: WorkspaceId, physical: &str) -> String {
        if self.namespaced {
            strip_workspace_key(workspace, physical)
        } else {
            physical.to_string()
        }
    }
}

/// One config-defined backend in the [`StorageRegistry`]: the live backend, its
/// catalogue identity (connection + bucket), and its kind (for listings).
#[derive(Clone)]
pub struct ConfigStore {
    pub backend: Arc<dyn StorageBackend>,
    pub connection: String,
    pub bucket: String,
    pub kind: &'static str,
    /// Whether keys on this store are workspace-namespaced (see
    /// [`StorageHandle::namespaced`]); `false` for a browse store.
    pub namespaced: bool,
    /// Workspace assignment (SOUL §9/§18) — slugs/UUIDs from the backend's
    /// [`workspaces`](crate::config::StorageBackendConfig::workspaces) config.
    /// Empty = visible to every workspace.
    pub workspaces: Vec<String>,
}

impl ConfigStore {
    /// Whether this store is assigned to workspace `ws` (empty assignment =
    /// every workspace).
    #[must_use]
    pub fn allows(&self, ws: &catalerum_core::model::Workspace) -> bool {
        crate::config::workspace_assigned(&self.workspaces, ws)
    }

    /// Resolve this config store to a [`StorageHandle`] for store name `store`.
    #[must_use]
    pub fn handle(&self, store: String) -> StorageHandle {
        StorageHandle {
            backend: self.backend.clone(),
            store,
            connection: self.connection.clone(),
            bucket: self.bucket.clone(),
            namespaced: self.namespaced,
        }
    }
}

/// The set of **config-defined** storage backends (SOUL §9), built once at boot
/// from `[storage]` + `[storage.backends.*]` and keyed by store name. Runtime
/// (user-added) backends are *not* held here — they resolve on demand from their
/// storage `Connection` rows — so this is the static base layer (SOUL principle
/// 10: config is the base, runtime state layers on). Empty disables the config
/// side of the `/storage` routes (a workspace may still have runtime backends).
#[derive(Clone, Default)]
pub struct StorageRegistry {
    stores: HashMap<String, ConfigStore>,
    default: Option<String>,
}

impl StorageRegistry {
    /// The config store named `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ConfigStore> {
        self.stores.get(name)
    }

    /// The default store name — the destination a file op picks when it names
    /// none (the legacy `"default"` backend, or the sole config store).
    #[must_use]
    pub fn default_name(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// Whether any config backend is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// Config stores as `(name, kind)`, sorted by name (stable listing).
    #[must_use]
    pub fn infos(&self) -> Vec<(String, &'static str)> {
        let mut v: Vec<(String, &'static str)> = self
            .stores
            .iter()
            .map(|(n, s)| (n.clone(), s.kind))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Whether any config store carries a workspace assignment
    /// ([`ConfigStore::workspaces`]) — when `false`, visibility never needs the
    /// workspace row, so resolvers can skip loading it.
    #[must_use]
    pub fn has_assignments(&self) -> bool {
        self.stores.values().any(|s| !s.workspaces.is_empty())
    }

    /// Whether the config store `name` exists and is **visible** to the given
    /// workspace (SOUL §9/§18). `ws = None` means the caller skipped the
    /// workspace lookup (because [`has_assignments`](Self::has_assignments) is
    /// `false`, or no workspace is in scope) — an assigned store then fails
    /// closed.
    #[must_use]
    pub fn visible(&self, name: &str, ws: Option<&catalerum_core::model::Workspace>) -> bool {
        self.stores.get(name).is_some_and(|s| match ws {
            Some(w) => s.allows(w),
            None => s.workspaces.is_empty(),
        })
    }

    /// [`infos`](Self::infos) filtered to the stores visible to `ws` (same
    /// `None` semantics as [`visible`](Self::visible)).
    #[must_use]
    pub fn infos_for(
        &self,
        ws: Option<&catalerum_core::model::Workspace>,
    ) -> Vec<(String, &'static str)> {
        let mut v: Vec<(String, &'static str)> = self
            .stores
            .iter()
            .filter(|(n, _)| self.visible(n, ws))
            .map(|(n, s)| (n.clone(), s.kind))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Config backends keyed by **catalogue connection name** — the map the ingest
    /// worker uses to read an object from the backend its bucket lives on.
    #[must_use]
    pub fn backends_by_connection(&self) -> HashMap<String, Arc<dyn StorageBackend>> {
        self.stores
            .values()
            .map(|s| (s.connection.clone(), s.backend.clone()))
            .collect()
    }

    /// Catalogue connection names of the config-defined **browse** stores (keys are
    /// *not* workspace-namespaced). The §10 object-ingest worker uses this to read a
    /// browse store's objects from their raw key rather than the `<workspace_id>/`
    /// namespaced one (matching how the storage routes wrote them).
    #[must_use]
    pub fn browse_connections(&self) -> HashSet<String> {
        self.stores
            .values()
            .filter(|s| !s.namespaced)
            .map(|s| s.connection.clone())
            .collect()
    }

    /// The default store as a handle, if a default exists. Used by the terminal
    /// flush, which targets the default store.
    #[must_use]
    pub fn default_handle(&self) -> Option<StorageHandle> {
        let name = self.default.as_ref()?;
        self.stores.get(name).map(|s| s.handle(name.clone()))
    }

    /// Every config store as `(name, backend)`, sorted by name — the blob sources
    /// a backup mirrors (SOUL §30), each under its own `objects/<name>/` sub-tree.
    /// Runtime (user-added) stores are **not** included: they live as DB
    /// connection rows and would need an async enumeration at build time.
    #[must_use]
    pub fn sources(&self) -> Vec<(String, Arc<dyn StorageBackend>)> {
        let mut v: Vec<(String, Arc<dyn StorageBackend>)> = self
            .stores
            .iter()
            .map(|(n, s)| (n.clone(), s.backend.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Test-only: a registry holding a single config store, set as the default —
    /// enough to exercise the store-resolver + object write/delete path without a
    /// full [`AppState`] boot (the deferred "test-only `StorageRegistry` constructor"
    /// the chat-uploads/archival tests need).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn single_for_test(name: &str, store: ConfigStore) -> Self {
        let mut stores = HashMap::new();
        stores.insert(name.to_string(), store);
        Self {
            stores,
            default: Some(name.to_string()),
        }
    }

    /// Test-only: a registry over arbitrary config stores + default, for tests
    /// outside this module (the fields are private).
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_test(stores: HashMap<String, ConfigStore>, default: Option<String>) -> Self {
        Self { stores, default }
    }
}

/// Build a [`StorageBackend`] from one backend's
/// [`StorageBackendConfig`](crate::config::StorageBackendConfig) by precedence:
/// **S3** when `[…s3]` has credentials, else **WebDAV** when `[…webdav].url` is
/// set, else **local FS** when `local_path` is set, else `None` (SOUL §9).
/// `bucket_fallback` names the S3 bucket when the entry omits its own `bucket`
/// (usually the store name). The factory for both config-defined backends (the
/// `[storage]` default + each `[storage.backends.<name>]`) and the
/// `[backup.destination]` backend (SOUL §30).
#[must_use]
pub fn build_backend(
    cfg: &crate::config::StorageBackendConfig,
    bucket_fallback: &str,
) -> Option<Arc<dyn StorageBackend>> {
    if cfg.s3.enabled() {
        let s3 = &cfg.s3;
        Some(Arc::new(S3Backend::new(
            &s3.endpoint,
            s3.region_name(),
            &s3.access_key,
            s3.secret_key.expose(),
            cfg.bucket_name(bucket_fallback).to_string(),
            s3.path_style,
        )))
    } else if cfg.webdav.enabled() {
        let w = &cfg.webdav;
        match WebDavBackend::new(&w.url, &w.username, w.password.expose()) {
            Ok(b) => Some(Arc::new(b)),
            Err(e) => {
                tracing::warn!(error = %e, "invalid webdav url; storage backend disabled");
                None
            }
        }
    } else if !cfg.local_path.trim().is_empty() {
        Some(Arc::new(LocalFsBackend::new(cfg.local_path.clone())))
    } else {
        None
    }
}

/// Build the single backend a [`StorageConfig`](crate::config::StorageConfig)'s
/// **default** (legacy top-level) fields describe (SOUL §9). Used by the
/// `[backup.destination]` backend (SOUL §30), which is always a single store;
/// the live multi-backend storage uses [`StorageRegistry`] over
/// [`StorageConfig::resolved_backends`](crate::config::StorageConfig::resolved_backends).
#[must_use]
pub fn build_storage_backend(
    cfg: &crate::config::StorageConfig,
) -> Option<Arc<dyn StorageBackend>> {
    build_backend(&cfg.default_backend(), cfg.bucket_name())
}

/// Build the `[ocr]` engine chain from config (SOUL §7/§10): the Mistral-dialect
/// API when its key is set, the vision chat engine when `vision_model` is set,
/// and the offline tesseract fallback when enabled **and** its binary + language
/// packs probe OK — in that fixed order. `None` when nothing is configured
/// (image objects then catalogue no text, exactly the pre-OCR behavior). Async
/// because of the tesseract probe, so the binary calls it before
/// [`AppState::new`] and passes the chain in.
pub async fn build_ocr_chain(
    config: &crate::config::OcrConfig,
    llm: &OpenRouterClient,
) -> Option<Arc<FallbackOcr>> {
    let mut engines: Vec<Arc<dyn catalerum_core::provider::OcrEngine>> = Vec::new();
    if config.mistral.is_enabled() {
        engines.push(Arc::new(MistralOcr::new(
            &config.mistral.base_url,
            config.mistral.api_key.expose(),
            &config.mistral.model,
        )));
    }
    let vision_model = config.vision_model.trim();
    if !vision_model.is_empty() {
        engines.push(Arc::new(VisionOcr::new(llm.clone(), vision_model)));
    }
    if config.tesseract.enabled {
        let tesseract = TesseractOcr::new(&config.tesseract.path, &config.tesseract.languages);
        if tesseract.probe().await {
            engines.push(Arc::new(tesseract));
        } else {
            // Default-on by design, so an absent binary is normal — debug, not
            // warn. A configured custom path failing is still worth surfacing.
            tracing::debug!(
                path = %config.tesseract.path,
                languages = %config.tesseract.languages,
                "tesseract OCR unavailable (binary or language pack missing); skipping"
            );
        }
    }
    if engines.is_empty() {
        None
    } else {
        let chain = FallbackOcr::new(engines);
        tracing::info!(engines = ?chain.engine_names(), "OCR engines configured");
        Some(Arc::new(chain))
    }
}

/// Build the `[preview]` client (SOUL §9/§10): a thin HTTP [`HttpPreviewer`] to
/// the standalone `catalerum-preview-service` (its own slim LibreOffice+poppler
/// image). `None` when previews are disabled or no `service_url` is set — the
/// distroless API carries no render toolchain, so with no service the preview
/// routes report "not configured". Synchronous, so it builds inline in
/// [`AppState::new`].
fn build_http_previewer(config: &crate::config::PreviewConfig) -> Option<Arc<dyn Previewer>> {
    if !config.enabled {
        return None;
    }
    let url = config.service_url.trim();
    if url.is_empty() {
        tracing::debug!("[preview].service_url unset; preview routes disabled");
        return None;
    }
    let token = Some(config.service_token.expose().to_string());
    match HttpPreviewer::new(url, token, config.timeout_secs) {
        Ok(previewer) => {
            tracing::info!(service_url = url, "preview service client configured");
            Some(Arc::new(previewer))
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to build preview client; previews disabled");
            None
        }
    }
}

/// Application state shared across all routes. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    store: Store,
    iam: Iam,
    llm: OpenRouterClient,
    bus: Bus,
    config: Config,
    fetcher: Option<Arc<dyn WebFetcher>>,
    /// The web-search backend (SOUL §27) — the `MultiSearcher` over enabled
    /// `[search]` providers; `None` when none is configured. Backs the
    /// `web_search` tool and the `/search-providers` route's "live" check.
    searcher: Option<Arc<dyn WebSearcher>>,
    /// The LLM tool registry the chat agent loop dispatches against (SOUL §7),
    /// built from the store + fetch backend at construction.
    registry: ToolRegistry,
    /// Pod-local lifecycle registry shared by every background-capable subagent
    /// launcher and the parent-facing monitor/wait/stop tools.
    subagent_runs: SubagentRunManager,
    /// Live external-MCP-server manager (SOUL §26): connects/disconnects servers
    /// into the registry's runtime overlay, driven by the `*_mcp_server` tools and
    /// the boot loader. Shares the registry's overlay `Arc`.
    mcp_manager: McpManager,
    /// Semantic index over the tool registry (SOUL §7) backing `search_tools`.
    /// Pre-warmed at boot and self-syncing with the registry on each search.
    tool_index: Arc<ToolIndex>,
    /// Semantic index over the automation node-type catalog (SOUL §11) backing
    /// `search_automation_node_types` + the `/automations/node-types/search` route.
    /// Pre-warmed at boot; the corpus is static so it embeds once.
    node_index: Arc<NodeDocIndex>,
    /// Semantic index over the internal how-to articles (SOUL §11) backing
    /// `search_articles` + the `/articles/search` route. Pre-warmed at boot; the
    /// corpus is static so it embeds once.
    article_index: Arc<ArticleIndex>,
    /// Re-ingest hook: enqueues `ingest_note` after a note write so its chunks
    /// are (re-)embedded (SOUL §6.4/§10/§21). Enabled by `[qdrant].enabled`.
    note_ingest: NoteIngest,
    /// Semantic-search backend (when `[qdrant].enabled`), also used for
    /// auto-recall of relevant memories into the chat system prompt (SOUL §22).
    search: Option<SemanticSearch>,
    /// Whether to mine conversations for memories (`[curation].enabled`): when on,
    /// each chat turn enqueues an `extract_memories` job (SOUL §22).
    curation_enabled: bool,
    /// Config-defined object-storage backends (`[storage]` + `[storage.backends.*]`),
    /// powering the `/storage` routes (SOUL §9). Runtime (user-added) backends layer
    /// on per-request from their storage `Connection` rows. Empty + no runtime
    /// backend → those routes return `404`.
    storage: StorageRegistry,
    /// Interactive terminal session manager (SOUL §20), when `[exec]` is enabled.
    /// Owns live PTY sessions across the configured backends; the `*_terminal`
    /// tools + the terminal ws/REST routes are its clients. `None` → no terminals.
    terminal_manager: Option<Arc<TerminalManager>>,
    /// Per-workspace sandbox manager (SOUL §20), when `[exec].per_workspace` is on
    /// for the container/kubernetes backend. Owns the one-long-lived-sandbox-per-
    /// workspace lifecycle; the terminal manager + `run_command` route through it.
    sandbox_manager: Option<Arc<WorkspaceSandboxManager>>,
    /// This process's stable pod identity (multi-pod HA, SOUL §16 M7). Stamped on
    /// every terminal/sandbox row this pod creates so boot reconcile reclaims only
    /// its own (+ legacy NULL) rows, never a peer pod's live sessions. Resolved
    /// once at construction (`CATALERUM_POD_ID` → `HOSTNAME` → random UUID).
    pod_id: String,
    /// Cross-pod comms handle (multi-pod HA, SOUL §16 M7), when
    /// `[secrets].master_key` is set: the sealed-envelope cipher + this pod's
    /// advertised address. Backs the `/internal/pod` forwarding route and the
    /// registry announcements; `None` keeps that route `404` and the manager's
    /// precise "route to that pod" errors.
    pod_comms: Option<Arc<crate::pod_forward::PodComms>>,
    /// Graph-query backend (SOUL §6.3), when `[neo4j]` is configured — the same
    /// configured client baked into the `query_graph` tool, kept here for the
    /// `/graph/query` route. `None` → that route returns `404`.
    graph: Option<GraphQuery>,
    /// Inbound-capable channels keyed by name (SOUL §25) — Matrix/Telegram channels
    /// configured with `inbound = true`. The channel listener subscribes to each and
    /// dispatches a `ChannelMessage` trigger per message (§11). Empty → no listener.
    inbound_channels: HashMap<String, Arc<dyn Channel>>,
    /// Signs + verifies `download_link` tokens (SOUL §9): the `download_link` tool
    /// mints them, the public `GET /download/{token}` route verifies them. Keyed by
    /// `[server].download_secret` (or a random per-process key when unset).
    download_signer: DownloadSigner,
    /// Signs + verifies scoped MCP-endpoint tokens (SOUL §26): the
    /// `POST /mcp-endpoints/{id}/token` route mints them, the public
    /// `POST /mcp/s/{token}` route verifies them. Shares the download signer's key
    /// (one operator secret covers both).
    endpoint_signer: EndpointSigner,
    /// Signs + verifies `trigger_link` tokens (SOUL §11/§12): the `trigger_link` tool
    /// and `POST /triggers/mint/{name}` route mint them, the public
    /// `POST /triggers/fire/{token}` route verifies them. Keyed by
    /// `[server].trigger_secret` (or a random per-process key when unset).
    trigger_signer: TriggerSigner,
    /// Encrypted credential store for external-provider secrets (SOUL §13), when
    /// `[secrets].master_key` is set. `None` disables credentialed features.
    secrets: Option<Arc<SecretStore>>,
    /// External-Postgres pool registry (SOUL §11/§19): lazily builds + caches a
    /// pool per `ConnectionKind::Postgres` connection. Backs the `sql_query` tool,
    /// the `SqlQuery` automation action, and the schema-migration routes.
    external_db: Arc<ExternalDbRegistry>,
    /// The OIDC single-sign-on provider (SOUL §18/§29), when `[sso]` is configured
    /// (issuer + client id set). Handles discovery/JWKS/token + `id_token`
    /// validation; `None` makes the `/auth/sso/*` routes return `404`.
    sso: Option<Arc<catalerum_iam::OidcProvider>>,
    /// Signs + verifies the short-lived SSO **state cookie** (SOUL §18) carrying
    /// `state`/`nonce`/PKCE-verifier across the IdP round-trip. Keyed by
    /// `[sso].state_secret` (or a random per-process key when unset).
    sso_state: SsoStateSigner,
    /// Signs + verifies the short-lived **Google-OAuth state cookie** (SOUL §16 M7)
    /// carrying the CSRF `state` + workspace/connection across the Google consent
    /// round-trip. Keyed by `[google].state_secret` (independent of the SSO key;
    /// random per-process when unset). Always built; the `/auth/google/*` routes
    /// still `404` unless `[google]` is configured.
    google_state: GoogleStateSigner,
    /// Signs + verifies the short-lived **Microsoft-OAuth state cookie** (SOUL §8)
    /// across the Entra consent round-trip — the Google signer's twin (same token
    /// machinery, an independent key from `[microsoft].state_secret`). Always
    /// built; the `/auth/microsoft/*` routes still `404` unless `[microsoft]` is
    /// configured.
    microsoft_state: GoogleStateSigner,
    /// Signs + verifies the per-channel **Google push token** (SOUL §8/§16 M7): the
    /// [`GoogleWatchWorker`](crate::google_watch::GoogleWatchWorker) mints one into
    /// each `events.watch` channel, the public `POST /webhooks/google/calendar`
    /// route verifies it. Keyed by `[google].push_secret` (independent of the state
    /// cookie key; random per-process when unset).
    google_channel: GoogleChannelSigner,
    /// Sole-user personalization cache (SOUL §18/§29): a workspace-scoped
    /// generation counter + per-`(workspace, user)` profile snapshots, consulted
    /// only in `single_user` mode. Always constructed; [`AppState::cached_profile`]
    /// mode-gates whether it is read.
    personalization: PersonalizationCache,
    /// Lazy process-wide cache of the gateway catalog's per-model **context
    /// windows** (SOUL §7), feeding the agent loop's auto-compaction trigger
    /// ([`AppState::model_context_window`]). `None` until the first successful
    /// catalog fetch; a failed fetch leaves it unfilled so a later turn retries.
    /// The catalog changes rarely enough that one fetch per process is fine.
    model_windows: tokio::sync::RwLock<Option<HashMap<String, u32>>>,
    /// Lazy process-wide cache of every gateway-catalog model's accepted input
    /// modalities (SOUL §7/§9). It gates native image inputs from
    /// uploads and binary-aware file tools. `None` until the first successful
    /// catalog fetch; failures leave it unfilled so a later turn retries.
    model_input_modalities: tokio::sync::RwLock<Option<HashMap<String, HashSet<String>>>>,
    /// Process-level registry of **detached** chat turns (SOUL §7/§12): each
    /// spawned agent run registers here for its lifetime so a Stop or a
    /// reconnecting socket that lands on this pod reaches the live run without a
    /// Valkey round-trip. Not a source of truth — a miss falls back to the
    /// cross-pod control channel / active-turn Registry key.
    active_turns: crate::active_turns::ActiveTurns,
    /// Pod-local registry of **live computer-agent connections** (SOUL §19/§20):
    /// installed daemons on servers/desktops that the `computer_*` tools drive over
    /// an authenticated WebSocket. In-memory and not a source of truth — an agent
    /// connected to another pod reads as offline here; the durable truth is the
    /// `computer_agents` table.
    computer_registry: Arc<crate::computer_registry::ComputerRegistry>,
    /// The boot-built `[ocr]` engine chain (SOUL §7/§10): mistral → vision →
    /// tesseract, each member present only when configured (and, for tesseract,
    /// probed). `None` = no engine — image objects catalogue no text, exactly
    /// the pre-OCR behavior. Built by [`build_ocr_chain`] in the binary (the
    /// tesseract probe is async) and shared with the ingest worker's
    /// `OcrContext`.
    ocr: Option<Arc<FallbackOcr>>,
    /// The boot-built `[preview]` engine chain (SOUL §9/§10): the in-process
    /// image thumbnailer plus, when a container/kubernetes exec backend runs the
    /// batteries sandbox image, the document (PDF/office/presentation) renderer.
    /// `None` when previews are disabled. Serves the `/storage/preview` routes.
    previewer: Option<Arc<dyn Previewer>>,
}

impl AppState {
    /// Assemble the application state from already-constructed services.
    ///
    /// `fetcher` is the web-fetch backend (SOUL §27); `None` disables the
    /// `/fetch` endpoint (it returns 500 "not configured"). The binary builds it
    /// from `[fetch]` config (`catalerum-fetch`); tests can pass `None`.
    ///
    /// `webhook` is the outbound webhook sender (SOUL §11/§27) — the guarded
    /// `HttpWebhookSender` the binary builds from the same `[fetch]` config
    /// (shared SSRF policy). `None` omits the `send_webhook` tool, so the
    /// `Webhook` automation action reports "unknown tool" (parity with
    /// `fetch_url` and an unset fetch backend). Tests pass `None`.
    ///
    /// `searcher` is the web-search backend (SOUL §27) — the `MultiSearcher` over
    /// the providers enabled in `[search]` config (`catalerum-search`); `None`
    /// omits the `web_search` tool (no provider configured). Tests pass `None`.
    ///
    /// `mcp_tools` are tools imported from external MCP servers (SOUL §26),
    /// already connected by the binary (the handshake is async); each is
    /// registered into the §7 registry under its `mcp:use@{server}` gate. Tests
    /// and the embedded MCP-server path pass an empty vec.
    // Each arg is a distinct already-built service the binary wires from config;
    // bundling them into a struct would only move the long list one call up.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        store: Store,
        iam: Iam,
        llm: OpenRouterClient,
        bus: Bus,
        config: Config,
        fetcher: Option<Arc<dyn WebFetcher>>,
        webhook: Option<Arc<dyn WebhookSender>>,
        searcher: Option<Arc<dyn WebSearcher>>,
        executor: Option<Arc<dyn Executor>>,
        terminal_backends: HashMap<ExecutorKind, Arc<dyn Executor>>,
        sandbox: Option<Arc<dyn WorkspaceSandbox>>,
        mcp_tools: Vec<Arc<dyn Tool>>,
        ocr: Option<Arc<FallbackOcr>>,
    ) -> Self {
        // The `[preview]` client (SOUL §9/§10): a thin HTTP client to the
        // standalone preview render service (its own LibreOffice+poppler image).
        let previewer = build_http_previewer(&config.preview);
        // Re-projection hook, shared by the REST handlers and the note LLM tools.
        // Each derived-store job is enqueued only when a worker can serve it
        // (`[qdrant]`/`[neo4j]`), so we never enqueue jobs nothing can run
        // (SOUL §6.3/§6.4/§10/§21).
        let note_ingest =
            NoteIngest::new(store.clone(), config.qdrant.enabled, config.neo4j.enabled);
        let curation_enabled = config.curation.enabled;
        // Encrypted credential store (SOUL §13): built from `[secrets].master_key`.
        // A malformed key fails loudly here (disabled + error log) rather than
        // silently storing external DB passwords in the clear; an *unset* key just
        // disables credentialed features (`None`).
        let secrets: Option<Arc<SecretStore>> = match config.secrets.master_key_bytes() {
            Ok(Some(key)) => match store.secret_store(&key) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::error!(error = %e, "failed to init secret store; credentialed features disabled");
                    None
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!(error = %e, "invalid [secrets].master_key; credentialed features disabled");
                None
            }
        };
        // External-Postgres pool registry (SOUL §11/§19). Shared by the sql_query
        // tool + schema-migration routes; pools are built lazily on first use.
        let external_db = Arc::new(
            ExternalDbRegistry::new(
                store.clone(),
                secrets.clone(),
                config.external_db.pool_max_connections,
                config.external_db.statement_timeout_ms,
            )
            .with_configured(config.external_db.connections.clone()),
        );
        // Signer for `download_link` tokens (SOUL §9): a stable configured secret so
        // links survive restarts / span pods, else a random per-process key. The API
        // base URL is what the links are rendered against (the file is served by the
        // API, not the web SPA).
        let download_signer =
            DownloadSigner::from_config(config.server.download_secret.as_ref().map(|s| s.expose()));
        // Scoped MCP-endpoint tokens reuse the download key (one operator secret).
        let endpoint_signer = EndpointSigner::new(download_signer.clone());
        let download_base_url = config.server.effective_base_url();
        // Signer for public `trigger_link` tokens (SOUL §11/§12): a stable configured
        // secret so links survive restarts / span pods, else a random per-process key.
        // Rendered against the same API base URL (the fire route is served by the API).
        let trigger_signer =
            TriggerSigner::from_config(config.server.trigger_secret.as_ref().map(|s| s.expose()));
        // Single sign-on (SOUL §18/§29). The state-cookie signer is always built (a
        // random key when no secret is set); the OIDC provider only when `[sso]` is
        // configured — a malformed provider config logs + disables SSO (routes 404)
        // rather than failing boot.
        let sso_state =
            SsoStateSigner::from_config(config.sso.state_secret.as_ref().map(|s| s.expose()));
        // Google-OAuth state-cookie signer (SOUL §16 M7) — an independent key from
        // the SSO signer so it rotates separately (a random per-process key when no
        // `[google].state_secret` is set).
        let google_state =
            GoogleStateSigner::from_config(config.google.state_secret.as_ref().map(|s| s.expose()));
        // Microsoft-OAuth state-cookie signer (SOUL §8) — same machinery, its own
        // key so it rotates independently of the Google + SSO signers.
        let microsoft_state = GoogleStateSigner::from_config(
            config.microsoft.state_secret.as_ref().map(|s| s.expose()),
        );
        // Signer for per-channel Google push tokens (SOUL §8/§16 M7): a stable
        // configured secret so channel tokens survive restarts / span pods, else a
        // random per-process key. Independent of the state-cookie key.
        let google_channel = GoogleChannelSigner::from_config(
            config.google.push_secret.as_ref().map(|s| s.expose()),
        );
        let sso = if config.sso.is_enabled() {
            let api_base = config.server.effective_base_url();
            let settings = catalerum_iam::OidcSettings {
                issuer: config.sso.issuer.trim().trim_end_matches('/').to_string(),
                client_id: config.sso.client_id.trim().to_string(),
                client_secret: config.sso.client_secret.expose().to_string(),
                redirect_uri: config.sso.effective_redirect_url(&api_base),
                scopes: config.sso.effective_scopes(),
                trust_email: config.sso.trust_email,
                token_auth_basic: config.sso.token_auth_basic,
                leeway_secs: config.sso.leeway(),
            };
            match catalerum_iam::OidcProvider::new(settings) {
                Ok(p) => Some(Arc::new(p)),
                Err(e) => {
                    tracing::error!(error = %e, "invalid [sso] config; single sign-on disabled");
                    None
                }
            }
        } else {
            None
        };
        // Semantic-search backend for the `search_semantic` tool — built only
        // when a vector index is configured (`[qdrant].enabled`); the same
        // llmleaf client serves as the query embedder (SOUL §6.4/§6.5/§7).
        let search = if config.qdrant.enabled {
            match VectorStore::new(&config.qdrant.url) {
                Ok(vector) => Some(SemanticSearch {
                    embedder: Arc::new(llm.clone()) as Arc<dyn Embedder>,
                    vector,
                    embed_model: config.llm.embedding_model.clone(),
                }),
                Err(e) => {
                    tracing::warn!(error = %e, url = %config.qdrant.url,
                        "invalid [qdrant].url; search_semantic disabled");
                    None
                }
            }
        } else {
            None
        };
        // Graph-query backend for the `query_graph` tool — built only when a
        // graph is configured (`[neo4j].enabled`); the same HTTP transactional
        // client `catalerum-graph` uses (SOUL §6.3/§6.5/§7).
        let graph = if config.neo4j.enabled {
            match catalerum_graph::GraphStore::new(&config.neo4j.url) {
                Ok(g) => Some(GraphQuery::Neo4j(
                    g.with_auth(config.neo4j.user.clone(), config.neo4j.password.expose())
                        .with_database(config.neo4j.database.clone()),
                )),
                Err(e) => {
                    tracing::warn!(error = %e, url = %config.neo4j.url,
                        "invalid [neo4j].url; query_graph falling back to database");
                    Some(GraphQuery::Database(store.clone()))
                }
            }
        } else {
            Some(GraphQuery::Database(store.clone()))
        };
        // The chat tool registry: note tools + (when configured) `fetch_url` +
        // (vector index) `search_semantic` + (graph) `query_graph`, each a
        // workspace-scoped thin client of the same truth the REST routes use (§7).
        // Keep a handle to the configured graph store for the `/graph/query`
        // route before the `GraphQuery` tool wrapper is moved into the registry.
        let graph_handle = graph.clone();
        // This process's stable pod identity (multi-pod HA, SOUL §16 M7). Resolved
        // once here (the random fallback differs per call) and handed to both the
        // sandbox + terminal managers so every row they create is owned by this
        // pod; boot reconcile (main.rs) then reclaims only this pod's rows.
        let pod_id = resolve_pod_id();
        // The per-workspace sandbox manager (SOUL §20): wraps the resolved backend
        // so terminal sessions + `run_command` exec into one long-lived sandbox per
        // workspace. Built before `build_registry` so `run_command` can hold it.
        let sandbox_manager = sandbox.map(|sb| {
            Arc::new(WorkspaceSandboxManager::new(
                sb,
                store.clone(),
                config.exec.backend_kind(),
                config.exec.sandbox_spec(),
                std::time::Duration::from_secs(config.exec.sandbox_idle_timeout_secs()),
                pod_id.clone(),
            ))
        });
        let mut registry = build_registry(
            &store,
            fetcher.as_ref(),
            note_ingest.clone(),
            search.clone(),
            graph,
            executor,
            config.ui.handler_tools.clone(),
            sandbox_manager.clone(),
            secrets.clone(),
        );
        // External MCP tools (SOUL §26): the binary already connected each
        // **config-file** server and handshook; we fold those (static) tools into
        // the registry. Each carries its own `mcp:use@{server}` gate (§19).
        for tool in mcp_tools {
            registry.register(tool);
        }
        // The live MCP manager shares the registry's runtime overlay (the clone
        // shares the overlay `Arc`), so runtime-created servers register into the
        // same set the agent loop dispatches against. Built before the management
        // tools so they can hold it; the four `*_mcp_server` tools are admin-gated
        // on the `mcp` domain (§19), registered statically here.
        let mcp_manager = McpManager::new(registry.clone());
        crate::tools::register_mcp_tools(&mut registry, &store, &mcp_manager);
        // Tool-search index (SOUL §6.4/§7): embeds each tool's `name: description`
        // so the `search_tools` tool can rank the registry by intent. Uses the same
        // llmleaf embedder as semantic search but is in-memory — it works without
        // Qdrant and stays in sync with hot-connected MCP tools (it reconciles on
        // each search). `search_tools` is registered LAST (below) so its snapshot
        // sees every other tool.
        let tool_index = Arc::new(crate::tool_index::ToolIndex::new(
            Arc::new(llm.clone()) as Arc<dyn Embedder>,
            config.llm.embedding_model.clone(),
        ));
        // Node-type-catalog index (SOUL §11): embeds the static automation node-type
        // docs so `search_automation_node_types` + the editor's node search rank them by
        // intent. Same embedder; the corpus is static so it embeds once (pre-warmed
        // at boot below alongside the tool index).
        let node_index = Arc::new(crate::node_index::NodeDocIndex::new(
            Arc::new(llm.clone()) as Arc<dyn Embedder>,
            config.llm.embedding_model.clone(),
        ));
        // Internal-articles index (SOUL §11): the worked-recipe corpus that sits above
        // the node-type catalog, backing `search_articles` + the editor's article
        // search. Same embedder; static corpus, so it embeds once (pre-warmed at boot).
        let article_index = Arc::new(crate::article_index::ArticleIndex::new(
            Arc::new(llm.clone()) as Arc<dyn Embedder>,
            config.llm.embedding_model.clone(),
        ));
        // `notify` (SOUL §25): registered when any channel is configured
        // (`[channels]`) — webhook senders (Discord/Slack) + token senders
        // (Telegram/Matrix), routed by name across providers. Built
        // post-`build_registry` to avoid threading channels through its signature.
        let mut channels: HashMap<String, Arc<dyn Channel>> = HashMap::new();
        // Names whose channel can also *receive* (an inbound long-poll), so the
        // channel listener subscribes to them — the multiplayer half (SOUL §11/§25).
        let mut inbound_names: Vec<String> = Vec::new();
        // Webhook senders (outbound only).
        for (name, url) in &config.channels.discord {
            if url.trim().is_empty() {
                continue;
            }
            channels.insert(
                name.clone(),
                Arc::new(DiscordWebhookChannel::new(url.clone())) as Arc<dyn Channel>,
            );
        }
        for (name, url) in &config.channels.slack {
            if url.trim().is_empty() {
                continue;
            }
            channels.insert(
                name.clone(),
                Arc::new(SlackWebhookChannel::new(url.clone())) as Arc<dyn Channel>,
            );
        }
        // Telegram bots: send via `sendMessage`, receive via `getUpdates` (when
        // `inbound = true`).
        for (name, tg) in &config.channels.telegram {
            if !tg.is_configured() {
                continue;
            }
            let mut channel = TelegramChannel::new(tg.bot_token.expose(), tg.chat_id.clone());
            if !tg.base_url.trim().is_empty() {
                channel = channel.with_base_url(tg.base_url.clone());
            }
            channels.insert(name.clone(), Arc::new(channel) as Arc<dyn Channel>);
            if tg.inbound {
                inbound_names.push(name.clone());
            }
        }
        // Matrix bots: send + receive over the same access token (`/sync`). Inbound
        // requires the bot's own `user_id` to filter its echo — without it an agent
        // reply would re-trigger itself, so inbound is refused (delivery still works).
        for (name, mx) in &config.channels.matrix {
            if !mx.is_configured() {
                continue;
            }
            let mut channel = MatrixChannel::new(
                mx.homeserver.clone(),
                mx.access_token.expose(),
                mx.room_id.clone(),
            );
            if !mx.user_id.trim().is_empty() {
                channel = channel.with_user_id(mx.user_id.clone());
            }
            channels.insert(name.clone(), Arc::new(channel) as Arc<dyn Channel>);
            if mx.inbound {
                if mx.user_id.trim().is_empty() {
                    tracing::warn!(
                        channel = %name,
                        "matrix channel has inbound=true but no user_id; inbound disabled (would echo-loop)"
                    );
                } else {
                    inbound_names.push(name.clone());
                }
            }
        }
        // The inbound subset the listener subscribes to (cheap `Arc` clones of the
        // same channel objects `notify` delivers through — so a reply routes back to
        // the room it came from).
        let inbound_channels: HashMap<String, Arc<dyn Channel>> = inbound_names
            .iter()
            .filter_map(|n| channels.get(n).map(|c| (n.clone(), c.clone())))
            .collect();
        if !channels.is_empty() {
            registry.register(Arc::new(crate::tools::NotifyTool::new(channels)));
        }
        // `web_search` (SOUL §27): registered when a search backend is configured
        // (`[search]` has an enabled provider), mirroring `fetch_url`'s
        // availability. Carries the per-user default-provider resolver. Built
        // post-`build_registry` so the searcher needn't thread through its
        // signature (like `notify`); gated on `web:search` by the tool itself.
        if let Some(searcher) = searcher.clone() {
            register_web_search_tool(&mut registry, &store, searcher);
        }
        // `send_webhook` (SOUL §11/§27): outbound webhook delivery — the egress-
        // write counterpart to `fetch_url`, backing the `Webhook` automation
        // action. Registered when the binary built a sender (it does whenever it
        // builds the fetcher — the same `[fetch]` SSRF policy governs both);
        // gated on `web:write` by the tool itself. Post-`build_registry` like
        // `web_search`.
        if let Some(webhook) = webhook {
            registry.register(Arc::new(catalerum_fetch::SendWebhookTool::new(webhook)));
        }
        // Object storage (SOUL §9): build every config-defined backend (the legacy
        // `[storage]` default + each `[storage.backends.<name>]`), keyed by store
        // name, so a file can choose where it lives. Runtime (user-added) backends
        // layer on per-request (SOUL principle 10). Powers the `/storage` routes +
        // the `StorageObject` trigger; built before the terminal manager (which
        // flushes to the default store) and the `search_tools` snapshot.
        let mut stores: HashMap<String, ConfigStore> = HashMap::new();
        for (name, bcfg) in config.storage.resolved_backends() {
            let Some(backend) = build_backend(&bcfg, &name) else {
                continue;
            };
            // The default store keeps the legacy catalogue connection so existing
            // objects stay attached; named stores catalogue under their own name.
            let connection = if name == crate::config::DEFAULT_STORE_NAME {
                DEFAULT_STORAGE_CONNECTION.to_string()
            } else {
                name.clone()
            };
            let bucket = bcfg.bucket_name(&name).to_string();
            let kind = bcfg.kind().unwrap_or("local");
            stores.insert(
                name.clone(),
                ConfigStore {
                    backend,
                    connection,
                    bucket,
                    kind,
                    // A browse store exposes its raw root (no `<workspace_id>/`
                    // namespacing) so an existing directory's files show up (§9/§18).
                    namespaced: !bcfg.browse,
                    workspaces: bcfg.workspaces.clone(),
                },
            );
        }
        // The default store: the legacy `"default"` when present, else the sole
        // configured store (so a single named backend needs no `?store=`), else none.
        let default = if stores.contains_key(crate::config::DEFAULT_STORE_NAME) {
            Some(crate::config::DEFAULT_STORE_NAME.to_string())
        } else if stores.len() == 1 {
            stores.keys().next().cloned()
        } else {
            None
        };
        let storage = StorageRegistry { stores, default };
        // Re-register `query_structured` WITH the registry (replacing the
        // store-only copy from `build_registry`) so its object operations
        // resolve each bucket's `?store=` name — object labels key on it (§9).
        crate::tools::register_query_structured(&mut registry, store.clone(), storage.clone());
        // `copy_object` (SOUL §9): register when object storage is configured (a
        // config-defined store exists). Runtime stores remain reachable by name
        // through the resolver. Also Boa-callable via the shared registry.
        if !storage.is_empty() {
            crate::tools::register_copy_object_tool(&mut registry, storage.clone(), store.clone());
            // `delete_object` / `create_directory` (SOUL §9): remove a stored file or a
            // whole directory, and make an empty directory. Both gate on `storage:write`
            // (like `copy_object`) and are Boa-callable via the shared registry.
            crate::tools::register_storage_file_tools(
                &mut registry,
                storage.clone(),
                store.clone(),
            );
            // `speech_to_text` / `text_to_speech` (SOUL §7): audio ⇄ text over stored
            // files — the STT input and TTS output are both files, so they gate on
            // storage and register with it. Config models/voice are the last-resort
            // fallback under each caller's per-user override. Boa-callable like copy.
            crate::tools::register_audio_tools(
                &mut registry,
                llm.clone(),
                storage.clone(),
                store.clone(),
                config.llm.transcription_model.clone(),
                config.llm.speech_model.clone(),
                config.llm.speech_voice.clone(),
            );
            // `ocr_document` (SOUL §7/§10): image/PDF → text over stored files,
            // served by the boot-built `[ocr]` engine chain (or an explicit
            // vision model). Registers even with no chain — a per-user/arg
            // model still works, and the tool errors clearly otherwise.
            crate::tools::register_ocr_tool(
                &mut registry,
                llm.clone(),
                storage.clone(),
                store.clone(),
                ocr.clone(),
                config.ocr.max_image_bytes,
                config.ocr.max_document_bytes,
            );
            // `download_link` (SOUL §9): mint a signed, short-lived URL the agent can
            // hand the user to download a stored file (or a directory as a `.tar.gz`)
            // from `GET /download/{token}`. Gated on `storage:read`; Boa-callable like
            // copy.
            crate::tools::register_download_link_tool(
                &mut registry,
                storage.clone(),
                store.clone(),
                download_signer.clone(),
                download_base_url.clone(),
            );
        }
        // The default store handle the terminal flush targets (unchanged behavior).
        let storage_default = storage.default_handle();
        // Background delegate/computer/terminal workers share one pod-local lifecycle
        // registry. Its control tools are always present because computer
        // subagents exist even when no terminal backend is configured.
        let subagent_runs = SubagentRunManager::default();
        register_subagent_run_tools(&mut registry, subagent_runs.clone());
        // Interactive terminals (SOUL §20): the manager owns live PTY sessions
        // across the configured executor backends; its terminal tools (open/write/
        // read/list/close, the workdir file tools, and the storage-backed persist/
        // sync) register here (before `search_tools`, so they're indexed) when an
        // executor backend exists. `None` → no terminals (no `[exec]`).
        let terminal_manager = if terminal_backends.is_empty() {
            None
        } else {
            let manager = Arc::new(TerminalManager::new(
                terminal_backends,
                store.clone(),
                storage_default.clone(),
                &config.exec,
                sandbox_manager.clone(),
                pod_id.clone(),
            ));
            register_terminal_tools(
                &mut registry,
                manager.clone(),
                storage.clone(),
                store.clone(),
                llm.clone(),
                config.llm.default_model.clone(),
                subagent_runs.clone(),
            );
            Some(manager)
        };
        // Cross-pod session forwarding (multi-pod HA, SOUL §16 M7): built only
        // when a master key exists (the sealed envelope is keyed off it — and a
        // multi-pod deployment already requires a shared key, §13). A key error
        // was logged by the secret-store build above; don't re-log here.
        let pod_comms = match config.secrets.master_key_bytes() {
            Ok(Some(key)) => {
                let addr = crate::pod_forward::advertised_addr(
                    &config.server.listen,
                    &config.server.pod_ip,
                );
                if addr.is_none() {
                    tracing::warn!(
                        listen = %config.server.listen,
                        "no advertisable pod address; cross-pod session forwarding disabled"
                    );
                }
                Some(Arc::new(crate::pod_forward::PodComms::new(
                    &key,
                    pod_id.clone(),
                    addr,
                )))
            }
            _ => None,
        };
        // Attach the forwarder so a request landing on a non-owning pod routes a
        // pod-local session to its owner instead of erroring.
        if let (Some(manager), Some(comms)) = (terminal_manager.as_ref(), pod_comms.as_ref()) {
            manager.set_forwarder(Arc::new(crate::pod_forward::HttpPodForwarder::new(
                comms.clone(),
                bus.clone(),
            )));
        }
        // Same multi-pod posture for the workspace-sandbox manager: with pod
        // comms configured, attach peer discovery so `ensure` refuses to mint a
        // duplicate node-local sandbox while a live peer pod owns it (sandbox
        // ops do not forward across pods yet).
        if let (Some(manager), Some(_)) = (sandbox_manager.as_ref(), pod_comms.as_ref()) {
            manager.set_peers(bus.clone());
        }
        // `search_automation_node_types` (SOUL §11): the agent-facing wrapper over the
        // node-type-catalog index. Independent of the registry snapshot, but grouped
        // with the other discovery tools.
        crate::tools::register_search_automation_node_types(&mut registry, node_index.clone());
        // `search_articles` (SOUL §11): the agent-facing wrapper over the internal
        // how-to article corpus. Grouped with the other discovery tools.
        crate::tools::register_search_articles(&mut registry, article_index.clone());
        // `search_models` (SOUL §7): search the gateway model catalog by name/id so
        // an agent can resolve the exact model id for `delegate` / speech tools.
        // Grouped with the other discovery tools.
        crate::tools::register_search_models(&mut registry, llm.clone());
        // `trigger_link` (SOUL §11/§12): mint a signed, short-lived public URL an
        // external caller can POST to fire one named automation signal (the public
        // twin of `fire_trigger`), redeemed at `POST /triggers/fire/{token}`. Gated on
        // `automation:write`; no storage dependency, so registered unconditionally.
        crate::tools::register_trigger_link_tool(
            &mut registry,
            trigger_signer.clone(),
            download_base_url.clone(),
        );
        // `sql_query` + managed-schema tools (SOUL §11): capability-gated data
        // and schema operations against external Postgres connections. Registered
        // before `search_tools` so all of them are indexed for tool search.
        crate::external_db::register_external_database_tools(
            &mut registry,
            external_db.clone(),
            config.external_db.max_rows,
        );
        // Computer agents (SOUL §19/§20): the `computer_*` tools drive installed
        // server/desktop daemons over the live registry built here. Registered
        // before `search_tools` so they're indexed. The SAME `Arc` is stored on
        // AppState so the WS connect handler and the tools share one live map. It
        // holds `bus` + `store` for cross-pod ownership discovery (SOUL §11/§16 M7);
        // the sealed forwarding transport is bound below once `pod_comms` exists.
        let computer_registry = Arc::new(crate::computer_registry::ComputerRegistry::new(
            pod_id.clone(),
            Some(bus.clone()),
            Some(store.clone()),
        ));
        if let Some(comms) = pod_comms.as_ref() {
            computer_registry.set_pod_comms(comms.clone());
        }
        crate::tools::register_computer_tools(
            &mut registry,
            store.clone(),
            llm.clone(),
            computer_registry.clone(),
            config.llm.default_model.clone(),
            subagent_runs.clone(),
        );
        // `search_tools` last: it snapshots the registry to search over, so every
        // other tool (notes, MCP management, `notify`, terminals, …) must already
        // be registered.
        crate::tools::register_search_tools(&mut registry, tool_index.clone());
        Self {
            inner: Arc::new(AppStateInner {
                store,
                iam,
                llm,
                bus,
                config,
                fetcher,
                searcher,
                registry,
                subagent_runs,
                mcp_manager,
                tool_index,
                node_index,
                article_index,
                note_ingest,
                search,
                curation_enabled,
                storage,
                terminal_manager,
                sandbox_manager,
                pod_id,
                pod_comms,
                graph: graph_handle,
                inbound_channels,
                download_signer,
                endpoint_signer,
                trigger_signer,
                secrets,
                external_db,
                sso,
                sso_state,
                google_state,
                microsoft_state,
                google_channel,
                personalization: PersonalizationCache::new(DEFAULT_TTL),
                model_windows: tokio::sync::RwLock::new(None),
                model_input_modalities: tokio::sync::RwLock::new(None),
                active_turns: crate::active_turns::ActiveTurns::new(),
                computer_registry,
                ocr,
                previewer,
            }),
        }
    }

    /// The Postgres store (source of truth).
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The encrypted credential store (SOUL §13), if `[secrets].master_key` is
    /// configured. `None` disables features that need to store a secret.
    #[must_use]
    pub fn secret_store(&self) -> Option<&Arc<SecretStore>> {
        self.inner.secrets.as_ref()
    }

    /// The external-Postgres pool registry (SOUL §11/§19) — resolves + caches a
    /// pool per `ConnectionKind::Postgres` connection for the `sql_query` tool,
    /// the `SqlQuery` action, and the schema-migration routes.
    #[must_use]
    pub fn external_db(&self) -> &Arc<ExternalDbRegistry> {
        &self.inner.external_db
    }

    /// The config-defined storage backends (SOUL §9) — the registry the `/storage`
    /// routes resolve a `?store=` against (runtime backends layer on per-request).
    #[must_use]
    pub fn storage(&self) -> &StorageRegistry {
        &self.inner.storage
    }

    /// The default store as a handle, if a config default exists — the target for
    /// the terminal flush and the backup source (both single-store).
    #[must_use]
    pub fn storage_default_handle(&self) -> Option<StorageHandle> {
        self.inner.storage.default_handle()
    }

    /// The signer for `download_link` tokens (SOUL §9) — used by the public
    /// `GET /download/{token}` route to verify a link before serving its file.
    #[must_use]
    pub fn download_signer(&self) -> &DownloadSigner {
        &self.inner.download_signer
    }

    /// The signer for scoped MCP-endpoint tokens (SOUL §26) — used by the public
    /// `POST /mcp/s/{token}` route to verify a token before serving its endpoint.
    #[must_use]
    pub fn endpoint_signer(&self) -> &EndpointSigner {
        &self.inner.endpoint_signer
    }

    /// The signer for `trigger_link` tokens (SOUL §11/§12) — used by the public
    /// `POST /triggers/fire/{token}` route to verify a link before firing its signal.
    #[must_use]
    pub fn trigger_signer(&self) -> &TriggerSigner {
        &self.inner.trigger_signer
    }

    /// The interactive terminal session manager (SOUL §20), if `[exec]` is
    /// enabled — backs the terminal ws/REST routes.
    #[must_use]
    pub fn terminal_manager(&self) -> Option<&Arc<TerminalManager>> {
        self.inner.terminal_manager.as_ref()
    }

    /// The per-workspace sandbox manager (SOUL §20), if `[exec].per_workspace` is
    /// enabled for the container/kubernetes backend — drives the idle reaper.
    #[must_use]
    pub fn sandbox_manager(&self) -> Option<&Arc<WorkspaceSandboxManager>> {
        self.inner.sandbox_manager.as_ref()
    }

    /// This process's stable pod identity (multi-pod HA, SOUL §16 M7) — the value
    /// the terminal/sandbox managers stamp on rows they create. Boot reconcile
    /// (main.rs) passes it to the scoped `close_all_active_for_pod` /
    /// `mark_all_stopped_for_pod` so a restart reclaims only this pod's rows.
    #[must_use]
    pub fn pod_id(&self) -> &str {
        &self.inner.pod_id
    }

    /// The cross-pod comms handle (multi-pod HA, SOUL §16 M7), when a master key
    /// is configured — backs the `/internal/pod` route. `None` → that route `404`s.
    #[must_use]
    pub(crate) fn pod_comms(&self) -> Option<&Arc<crate::pod_forward::PodComms>> {
        self.inner.pod_comms.as_ref()
    }

    /// Announce this pod's reachable address on the bus registry (multi-pod HA,
    /// SOUL §16 M7). Called once at boot and then on the heartbeat clock
    /// (main.rs) so the TTL'd entry outlives only a live pod. A no-op when pod
    /// comms are disabled or no address could be determined.
    pub async fn announce_pod(&self) {
        if let Some(comms) = self.pod_comms() {
            if let Some(addr) = comms.advertised_addr.as_deref() {
                crate::pod_forward::announce_self(self.bus(), self.pod_id(), addr).await;
            }
        }
    }

    /// The IAM service (auth + workspace scoping).
    #[must_use]
    pub fn iam(&self) -> &Iam {
        &self.inner.iam
    }

    /// The OIDC single-sign-on provider (SOUL §18/§29), if `[sso]` is configured —
    /// backs `GET /auth/sso/login` + `/callback`. `None` → those routes `404`.
    #[must_use]
    pub fn sso(&self) -> Option<&Arc<catalerum_iam::OidcProvider>> {
        self.inner.sso.as_ref()
    }

    /// The signer for the short-lived SSO **state cookie** (SOUL §18) — the login
    /// route mints one, the callback verifies + consumes it.
    #[must_use]
    pub fn sso_state_signer(&self) -> &SsoStateSigner {
        &self.inner.sso_state
    }

    /// The signer for the short-lived **Google-OAuth state cookie** (SOUL §16 M7) —
    /// `/auth/google/connect` mints one, `/auth/google/callback` verifies + spends it.
    #[must_use]
    pub fn google_state_signer(&self) -> &GoogleStateSigner {
        &self.inner.google_state
    }

    /// The signer for the short-lived **Microsoft-OAuth state cookie** (SOUL §8) —
    /// `/auth/microsoft/connect` mints one, `/auth/microsoft/callback` verifies +
    /// spends it. Independent key from the Google signer.
    #[must_use]
    pub fn microsoft_state_signer(&self) -> &GoogleStateSigner {
        &self.inner.microsoft_state
    }

    /// The signer for per-channel **Google push tokens** (SOUL §8/§16 M7) — the
    /// watch worker mints one into each `events.watch` channel, the public
    /// `POST /webhooks/google/calendar` route verifies the `X-Goog-Channel-Token`.
    #[must_use]
    pub fn google_channel_signer(&self) -> &GoogleChannelSigner {
        &self.inner.google_channel
    }

    /// Whether the SSO state cookie should carry the `Secure` flag — true on an
    /// https deployment (derived from the effective API base URL), false for a
    /// plain-http dev origin so the cookie still round-trips.
    #[must_use]
    pub fn sso_cookie_secure(&self) -> bool {
        self.inner
            .config
            .server
            .effective_base_url()
            .starts_with("https")
    }

    /// The graph-query backend (SOUL §6.3), if `[neo4j]` is configured.
    #[must_use]
    pub(crate) fn graph(&self) -> Option<&GraphQuery> {
        self.inner.graph.as_ref()
    }

    #[must_use]
    pub fn graph_available(&self) -> bool {
        self.inner.graph.is_some()
    }

    /// The vector store (Qdrant) behind semantic search (SOUL §6.4), if
    /// `[qdrant]` is enabled — exposed for the `/status` health probe.
    #[must_use]
    pub fn vector(&self) -> Option<&VectorStore> {
        self.inner.search.as_ref().map(|s| &s.vector)
    }

    /// The llmleaf / OpenRouter chat client.
    #[must_use]
    pub fn llm(&self) -> &OpenRouterClient {
        &self.inner.llm
    }

    /// The `[ocr]` engine chain (SOUL §7/§10), when any engine is configured.
    /// Serves `POST /ocr`, the `ocr_document` tool's no-override path, and the
    /// ingest worker's `OcrContext`.
    #[must_use]
    pub fn ocr(&self) -> Option<&Arc<FallbackOcr>> {
        self.inner.ocr.as_ref()
    }

    /// The `[preview]` engine chain (SOUL §9/§10), when previews are enabled:
    /// the in-process image thumbnailer plus, when an exec sandbox backend is
    /// configured, the document (PDF/office/presentation) renderer. Serves the
    /// `/storage/preview` routes.
    #[must_use]
    pub fn previewer(&self) -> Option<&Arc<dyn Previewer>> {
        self.inner.previewer.as_ref()
    }

    /// The token relay / coordination bus.
    #[must_use]
    pub fn bus(&self) -> &Bus {
        &self.inner.bus
    }

    /// The process-level registry of detached chat turns (SOUL §7/§12).
    #[must_use]
    pub fn active_turns(&self) -> &crate::active_turns::ActiveTurns {
        &self.inner.active_turns
    }

    /// The pod-local registry of live computer-agent connections (SOUL §19/§20).
    #[must_use]
    pub fn computer_registry(&self) -> Arc<crate::computer_registry::ComputerRegistry> {
        self.inner.computer_registry.clone()
    }

    /// The parsed configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// The web-fetch backend (SOUL §27), if one is configured.
    #[must_use]
    pub fn fetcher(&self) -> Option<&Arc<dyn WebFetcher>> {
        self.inner.fetcher.as_ref()
    }

    /// The web-search backend (SOUL §27) — the `MultiSearcher` over enabled
    /// `[search]` providers — if any provider is configured.
    #[must_use]
    pub fn searcher(&self) -> Option<&Arc<dyn WebSearcher>> {
        self.inner.searcher.as_ref()
    }

    /// The LLM tool registry the chat agent loop dispatches against (SOUL §7).
    #[must_use]
    pub fn registry(&self) -> &ToolRegistry {
        &self.inner.registry
    }

    /// The shared pod-local lifecycle registry for background subagent runs.
    #[must_use]
    pub(crate) fn subagent_runs(&self) -> SubagentRunManager {
        self.inner.subagent_runs.clone()
    }

    /// The live external-MCP-server manager (SOUL §26) — used by the boot loader to
    /// connect DB-defined servers, and a clone backs the `*_mcp_server` tools.
    #[must_use]
    pub fn mcp_manager(&self) -> McpManager {
        self.inner.mcp_manager.clone()
    }

    /// The tool-search index (SOUL §7) — the boot loader pre-warms it; `search_tools`
    /// holds a clone.
    #[must_use]
    pub fn tool_index(&self) -> Arc<ToolIndex> {
        self.inner.tool_index.clone()
    }

    /// The automation node-type-catalog index (SOUL §11) — the boot loader pre-warms
    /// it; `search_automation_node_types` + the `/automations/node-types/search` route use
    /// it.
    #[must_use]
    pub fn node_index(&self) -> Arc<NodeDocIndex> {
        self.inner.node_index.clone()
    }

    /// The internal-articles index (SOUL §11) — the boot loader pre-warms it;
    /// `search_articles` + the `/articles/search` route use it.
    #[must_use]
    pub fn article_index(&self) -> Arc<ArticleIndex> {
        self.inner.article_index.clone()
    }

    /// Inbound-capable channels keyed by name (SOUL §25) — the Matrix/Telegram
    /// channels with `inbound = true`. The channel listener subscribes to each;
    /// empty when none are configured for inbound.
    #[must_use]
    pub fn inbound_channels(&self) -> &HashMap<String, Arc<dyn Channel>> {
        &self.inner.inbound_channels
    }

    /// Enqueue a best-effort note re-ingest after a write (SOUL §6.4/§10/§21).
    /// A no-op unless `[qdrant].enabled`; never fails the caller.
    pub async fn enqueue_note_ingest(&self, workspace_id: WorkspaceId, note_id: NoteId) {
        self.inner.note_ingest.enqueue(workspace_id, note_id).await;
    }

    /// Enqueue a best-effort event graph (re-)projection after a local-calendar
    /// write (SOUL §6.3/§8) — the calendar twin of [`enqueue_note_ingest`]. A
    /// no-op unless `[neo4j].enabled`; never fails the caller. On delete this
    /// reconciles to a purge (the worker finds the event gone).
    pub async fn enqueue_event_projection(&self, workspace_id: WorkspaceId, event_id: EventId) {
        self.inner
            .note_ingest
            .enqueue_event(workspace_id, event_id)
            .await;
    }

    /// Enqueue a best-effort link graph (re-)projection after a link write
    /// (SOUL §6.3) — the `RELATES_TO` twin of [`enqueue_event_projection`]. A
    /// no-op unless `[neo4j].enabled`; never fails the caller. On delete this
    /// reconciles to a purge (the worker finds the link gone).
    pub async fn enqueue_link_projection(&self, workspace_id: WorkspaceId, link_id: LinkId) {
        self.inner
            .note_ingest
            .enqueue_link(workspace_id, link_id)
            .await;
    }

    /// The user's [`Profile`](catalerum_core::model::Profile) for `workspace_id`,
    /// via the sole-user personalization cache (SOUL §18/§29) when
    /// `[server].mode = single_user`, else a direct store read.
    ///
    /// The profile is the one message-independent personalization input the chat
    /// turn injects (SOUL §22) — memory *recall* embeds the current message, so it
    /// is per-message and never cached. In `single_user` this returns a snapshot
    /// cached under the workspace's current generation (invalidated by
    /// [`bump_personalization`](Self::bump_personalization) on every profile write,
    /// with a TTL backstop); in `multi_user` the cache is never consulted (the
    /// mode-flip invalidation story: flipping is a restart, so the fresh process
    /// simply reads through). Byte-identical to the direct read either way.
    pub async fn cached_profile(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> catalerum_store::Result<catalerum_core::model::Profile> {
        // Mode gate (optimizations only, never correctness): multi_user reads through.
        if !self.inner.config.server.mode.caches_sole_user() {
            return self.inner.store.profiles().get(workspace_id, user_id).await;
        }
        let cache = &self.inner.personalization;
        if let Some(hit) = cache.get(workspace_id, user_id, std::time::Instant::now()) {
            return Ok(hit);
        }
        // Capture the generation *before* the read so a write racing the fill
        // stamps the entry stale (rebuilt next read) rather than serving stale.
        let generation = cache.generation(workspace_id);
        let profile = self
            .inner
            .store
            .profiles()
            .get(workspace_id, user_id)
            .await?;
        cache.put(
            workspace_id,
            user_id,
            profile.clone(),
            generation,
            std::time::Instant::now(),
        );
        Ok(profile)
    }

    /// Invalidate a workspace's cached personalization (SOUL §29) by bumping its
    /// generation — every write path that changes a user's profile calls this so
    /// [`cached_profile`](Self::cached_profile) rebuilds on the next read. Cheap
    /// (an in-memory counter increment); a no-op effect outside `single_user`
    /// mode, where the cache is never consulted anyway.
    pub fn bump_personalization(&self, workspace_id: WorkspaceId) {
        self.inner.personalization.bump(workspace_id);
    }

    /// Recall up to `limit` memory texts semantically relevant to `query` and
    /// visible to `user_id`, for auto-injection into the chat system prompt
    /// (SOUL §22). Empty when no vector backend is configured; best-effort
    /// (never fails the turn).
    pub async fn recall_memories(
        &self,
        workspace_id: WorkspaceId,
        user_id: Option<UserId>,
        query: &str,
        limit: usize,
    ) -> Vec<String> {
        match &self.inner.search {
            Some(search) => {
                crate::tools::recall_memory_texts(
                    &self.inner.store,
                    search,
                    workspace_id,
                    user_id,
                    query,
                    limit,
                )
                .await
            }
            None => Vec::new(),
        }
    }

    /// The context-window size (tokens) of `model` from the gateway catalog
    /// (SOUL §7), feeding the agent loop's auto-compaction trigger. The whole
    /// catalog is fetched once per process on first use and cached
    /// (`model_windows`); a fetch failure returns `None` (the compactor falls
    /// back to its built-in default) without poisoning the cache, so a later
    /// turn retries. Best-effort by design — this must never fail a turn.
    pub async fn model_context_window(&self, model: &str) -> Option<u32> {
        if let Some(map) = self.inner.model_windows.read().await.as_ref() {
            return map.get(model).copied();
        }
        let models = match self
            .inner
            .llm
            .list_models(catalerum_llm::ModelKind::All, None)
            .await
        {
            Ok(models) => models,
            Err(e) => {
                tracing::warn!(error = %e, "fetching model catalog for context windows failed");
                return None;
            }
        };
        let map: HashMap<String, u32> = models
            .into_iter()
            .filter_map(|m| m.context_length.map(|w| (m.id, w)))
            .collect();
        let window = map.get(model).copied();
        // Two concurrent misses both fetch; last write wins with equivalent data.
        *self.inner.model_windows.write().await = Some(map);
        window
    }

    /// Accepted input modalities for `model` according to the gateway catalog.
    /// Values are normalized to lowercase. Fetching is best-effort: an unknown
    /// model or catalog failure returns an empty set, making native binary input
    /// fail closed without failing the chat turn.
    pub async fn model_input_modalities(&self, model: &str) -> HashSet<String> {
        if let Some(map) = self.inner.model_input_modalities.read().await.as_ref() {
            return map.get(model).cloned().unwrap_or_default();
        }
        let models = match self
            .inner
            .llm
            .list_models(catalerum_llm::ModelKind::All, None)
            .await
        {
            Ok(models) => models,
            Err(e) => {
                tracing::warn!(error = %e, "fetching model catalog for input modalities failed");
                return HashSet::new();
            }
        };
        let map: HashMap<String, HashSet<String>> = models
            .into_iter()
            .map(|m| {
                let modalities = m
                    .input_modalities
                    .iter()
                    .map(|modality| modality.to_ascii_lowercase())
                    .collect();
                (m.id, modalities)
            })
            .collect();
        let modalities = map.get(model).cloned().unwrap_or_default();
        // Two concurrent misses both fetch; last write wins with equivalent data.
        *self.inner.model_input_modalities.write().await = Some(map);
        modalities
    }

    /// Whether `model` accepts image input.
    pub async fn model_supports_image_input(&self, model: &str) -> bool {
        self.model_input_modalities(model).await.contains("image")
    }

    /// Enqueue a best-effort memory-extraction job for a finished conversation
    /// turn (SOUL §22). A no-op unless `[curation].enabled`; never fails the turn.
    pub async fn enqueue_memory_extraction(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        user_id: UserId,
    ) {
        if !self.inner.curation_enabled {
            return;
        }
        if let Err(e) = catalerum_ingest::enqueue_extract_memories(
            &self.inner.store,
            workspace_id,
            conversation_id,
            user_id,
        )
        .await
        {
            tracing::warn!(error = %e, %conversation_id, "failed to enqueue memory extraction");
        }
    }

    /// Store a memory through the shared dedup seam (SOUL §22/§29) — the same
    /// heuristic + embedding-similarity path the `remember` tool uses, so the
    /// `POST /memories` route never stores a fact the workspace already knows. When
    /// a vector backend is configured (`[qdrant].enabled`) the similarity layer
    /// runs and a stored/refined memory is enqueued for (re-)embedding; otherwise
    /// dedup is heuristic-only. Returns whether the memory was `stored`,
    /// `deduplicated`, or `refined`.
    pub async fn store_memory_deduped(
        &self,
        workspace_id: WorkspaceId,
        scope: catalerum_core::model::MemoryScope,
        user_id: Option<UserId>,
        text: &str,
    ) -> Result<catalerum_ingest::MemoryStoreOutcome, catalerum_ingest::IngestError> {
        let index = self
            .inner
            .search
            .as_ref()
            .map(|s| catalerum_ingest::MemoryDedupIndex {
                embedder: &*s.embedder,
                vector: &s.vector,
                embed_model: s.embed_model.as_str(),
            });
        catalerum_ingest::store_memory_deduped(
            &self.inner.store,
            index.as_ref(),
            workspace_id,
            scope,
            user_id,
            text,
            None,
        )
        .await
    }
}

/// Resolve this process's stable pod identity (multi-pod HA, SOUL §16 M7).
///
/// Precedence: `CATALERUM_POD_ID` (explicit override) → `HOSTNAME` (the
/// k8s-native stable-per-pod name — a pod keeps it across restart-in-place, and a
/// replacement Pod under the same name in a StatefulSet re-claims it) → a random
/// UUID (single-process / non-k8s dev). Resolve ONCE at boot and thread the value
/// through: the random fallback differs on each call, so two calls would disagree
/// and a restart would fail to reclaim its own rows.
#[must_use]
pub(crate) fn resolve_pod_id() -> String {
    pod_id_from_env(
        std::env::var("CATALERUM_POD_ID").ok().as_deref(),
        std::env::var("HOSTNAME").ok().as_deref(),
    )
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// The env-driven half of [`resolve_pod_id`], split out to unit-test the
/// precedence without touching process env: the first non-blank of `pod_id_env`
/// then `hostname_env`; `None` when neither is set (caller uses a random UUID).
fn pod_id_from_env(pod_id_env: Option<&str>, hostname_env: Option<&str>) -> Option<String> {
    [pod_id_env, hostname_env]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_storage::LocalFsBackend;

    #[test]
    fn pod_id_resolution_order_prefers_override_then_hostname_then_random() {
        // CATALERUM_POD_ID wins over HOSTNAME.
        assert_eq!(
            pod_id_from_env(Some("pod-abc"), Some("host-xyz")).as_deref(),
            Some("pod-abc")
        );
        // Blank override falls through to HOSTNAME (whitespace is not an identity).
        assert_eq!(
            pod_id_from_env(Some("  "), Some("host-xyz")).as_deref(),
            Some("host-xyz")
        );
        // HOSTNAME used when the override is unset.
        assert_eq!(
            pod_id_from_env(None, Some("host-xyz")).as_deref(),
            Some("host-xyz")
        );
        // Trimmed on the way out.
        assert_eq!(
            pod_id_from_env(Some(" pod-abc "), None).as_deref(),
            Some("pod-abc")
        );
        // Neither set (or both blank) → None, so the caller mints a random UUID.
        assert_eq!(pod_id_from_env(None, None), None);
        assert_eq!(pod_id_from_env(Some(""), Some("   ")), None);
        // The full resolver never yields an empty id (random fallback engages).
        assert!(!resolve_pod_id().is_empty());
    }

    fn handle(namespaced: bool) -> StorageHandle {
        StorageHandle {
            backend: Arc::new(LocalFsBackend::new("/tmp/catalerum-test")),
            store: "s".into(),
            connection: "s".into(),
            bucket: "s".into(),
            namespaced,
        }
    }

    #[test]
    fn physical_and_user_key_honor_namespacing() {
        let ws = WorkspaceId::from_uuid(uuid::Uuid::nil());
        // An isolated store namespaces keys under `<ws>/…` (SOUL §18) and the
        // mapping round-trips back to the user-facing key.
        let iso = handle(true);
        let phys = iso.physical_key(ws, "docs/a.txt");
        assert_eq!(phys, format!("{ws}/docs/a.txt"));
        assert_eq!(iso.user_key(ws, &phys), "docs/a.txt");
        // A browse store uses the raw key verbatim — that's what makes an existing
        // directory's on-disk files visible — and a leading slash is trimmed (same
        // contract as `workspace_object_key`).
        let br = handle(false);
        assert_eq!(br.physical_key(ws, "docs/a.txt"), "docs/a.txt");
        assert_eq!(br.physical_key(ws, "/docs/a.txt"), "docs/a.txt");
        assert_eq!(br.user_key(ws, "docs/a.txt"), "docs/a.txt");
    }

    #[test]
    fn registry_visibility_honors_workspace_assignment() {
        use catalerum_core::model::Workspace;
        use catalerum_core::OrganisationId;

        fn config_store(workspaces: Vec<String>) -> ConfigStore {
            ConfigStore {
                backend: Arc::new(LocalFsBackend::new("/tmp/catalerum-test")),
                connection: "c".into(),
                bucket: "b".into(),
                kind: "local",
                namespaced: true,
                workspaces,
            }
        }
        fn workspace(slug: &str) -> Workspace {
            Workspace {
                id: WorkspaceId::new(),
                organisation_id: OrganisationId::new(),
                name: slug.to_string(),
                slug: slug.to_string(),
                archived_at: None,
            }
        }

        let team_a = workspace("team-a");
        let team_b = workspace("team-b");
        let mut stores = HashMap::new();
        stores.insert("shared".to_string(), config_store(Vec::new()));
        stores.insert(
            "a-only".to_string(),
            config_store(vec!["team-a".to_string()]),
        );
        let registry = StorageRegistry {
            stores,
            default: Some("a-only".to_string()),
        };

        assert!(registry.has_assignments());
        // The unassigned store is visible everywhere; the assigned one only in
        // its workspace — and fails closed with no workspace row (`None`).
        assert!(registry.visible("shared", Some(&team_a)));
        assert!(registry.visible("shared", Some(&team_b)));
        assert!(registry.visible("shared", None));
        assert!(registry.visible("a-only", Some(&team_a)));
        assert!(!registry.visible("a-only", Some(&team_b)));
        assert!(!registry.visible("a-only", None));
        assert!(!registry.visible("missing", Some(&team_a)));
        // Filtered listings drop the invisible store.
        let names = |ws: &Workspace| -> Vec<String> {
            registry
                .infos_for(Some(ws))
                .into_iter()
                .map(|(n, _)| n)
                .collect()
        };
        assert_eq!(names(&team_a), ["a-only", "shared"]);
        assert_eq!(names(&team_b), ["shared"]);
        // A registry with no assignments never needs the workspace row.
        let plain = StorageRegistry::single_for_test("shared", config_store(Vec::new()));
        assert!(!plain.has_assignments());
        assert!(plain.visible("shared", None));
    }
}
