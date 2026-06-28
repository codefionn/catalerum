//! A concrete automation [`ActionRunner`] backed by the LLM tool registry (SOUL
//! §11/§7/§19).
//!
//! [`ToolActionRunner`] binds the tool-backed automation actions (`CreateNote`,
//! `EditNote`, `CreateTask`, `MoveTask`, `CreateEvent`, `UpdateEvent`, `RunCommand`,
//! `Notify`, `IndexDocument`, `FetchUrl`, `HtmlToMarkdown`, `ExtractHtml`) to their
//! existing [`ToolRegistry`] tools: the action's
//! `params` become the tool args, dispatched
//! under a [`ToolContext`] carrying the automation's **authority** — the capability
//! set it runs with (SOUL §19). So an automation can only do what its authority
//! permits, enforced by the same deny-by-default dispatch gate as a chat turn.
//!
//! The authority is supplied to the runner. A caller may pin it explicitly
//! ([`as_user`](ToolActionRunner::as_user) / [`with_capabilities`](ToolActionRunner::with_capabilities)
//! — the path the tests and a future grant-aware dispatcher use). Otherwise, the
//! binary attaches a **store-backed** runner
//! ([`workspace_owner_authority`](ToolActionRunner::workspace_owner_authority)):
//! since the §19 grants table + policy engine don't exist yet (a soft `grant_id`,
//! always `None`), each automation runs **as its workspace's owner** under bounded
//! base-**Member** capabilities — ordinary read/write, never `*:delete`, `exec:run`,
//! or admin/MCP-expose. Protected scopes (§19) stay unreachable even before grant
//! resolution lands; the identity is resolved per call from the job's
//! `workspace_id`, so it is multi-tenant correct.
//!
//! An **`LlmAgent`** action (SOUL §11/§7) runs the shared agent loop
//! ([`catalerum_llm::run_agent`]) when an LLM client is attached
//! ([`with_llm`](ToolActionRunner::with_llm)): the action's `system`/`model`/`tools`
//! seed and confine the loop, which dispatches tools through the **same**
//! capability-gated [`ToolContext`] as every other action — so a triggered agent is
//! bounded both by its advertised tool subset and the automation's authority. The
//! agent is also **context- and skill-aware**: the firing `trigger` is described in
//! the seed user turn (so it knows *what* fired it), and each skill it names (§23)
//! has its runbook `instructions_md` injected into the system prompt. When the
//! firing trigger is a **`ChannelMessage`** (§25), the agent's final reply is
//! **delivered back to that channel** automatically (via the `notify` tool, under
//! the automation's authority) — so an inbound message → agent → on-channel reply
//! is a complete chatbot without the model having to call `notify` itself.
//!
//! A **`RunSkill`** action (SOUL §11/§23) is the named counterpart: it resolves a
//! saved [`Skill`](catalerum_core::model::Skill) and runs the **same** agent loop with
//! the skill's `instructions_md` as the system prompt and its `tools` as the advertised
//! set — capability-gated `skill:use@<name>` like the `use_skill` tool — so a reusable
//! skill becomes a one-field automation step (the channel-reply + JSON-output handling
//! is shared with `LlmAgent`).
//!
//! A **`Summarize`** action (SOUL §11) is a **one-shot** LLM completion — no tools, no
//! loop: the action's template-rendered `input` (or, unset, the firing trigger event)
//! is condensed into a `summary` downstream nodes can reference
//! (`{{ inputs.<node>.summary }}`). The object actions (`WriteObject`/`MoveObject`)
//! and `Webhook` dispatch their registry tools (`write_object`/`move_object`/
//! `send_webhook`) like any other tool-backed kind.
//!
//! A **`CreateChatThread`** action (SOUL §11/§12) is the in-product output sink:
//! it atomically creates an `automation`-origin conversation and writes the
//! template-rendered `message` as its first assistant turn. It is gated on
//! `conversation:write`, appears in the ordinary Chat list, and returns both ids
//! for downstream graph nodes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use catalerum_automation::{Action, ActionKind, ActionOutcome, ActionRunner, LlmAgent};
use catalerum_core::capability::{Action as CapAction, Capability, Resource};
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{
    Attachment, Calendar, Email, Event, ExtractedAttachment, Grant, MessageRole, Origin, Role,
};
use catalerum_core::tool::{ToolContext, ToolRegistry};
use catalerum_core::{AgentId, CalendarId, MailboxId, UserId, WorkspaceId};

use crate::state::StorageRegistry;
use catalerum_llm::{run_agent, AgentConfig, AgentOutcome, OpenRouterClient};
use catalerum_script::CodeToolHost;
use catalerum_store::Store;

/// The default system prompt for an `LlmAgent` action that doesn't supply its own.
const DEFAULT_AGENT_SYSTEM: &str = "You are an automation agent for catalerum. You were \
triggered to carry out a configured task. Use the tools available to you, stay within your \
authority, and stop when the task is done.";

/// The seed user turn that kicks an automation agent off (it has no human prompt —
/// a trigger fired it; its instructions live in the system prompt).
const AGENT_TRIGGER_PROMPT: &str =
    "This automation has been triggered. Carry out your instructions now.";

/// The LLM client + default model an [`LlmAgent`] action runs the §7 loop against.
#[derive(Clone)]
struct LlmRunner {
    client: OpenRouterClient,
    default_model: String,
}

/// An [`ActionRunner`] that executes tool-backed automation actions through the
/// [`ToolRegistry`], under a configured authority (acting identity + capabilities).
#[derive(Clone)]
pub struct ToolActionRunner {
    registry: ToolRegistry,
    user_id: Option<UserId>,
    agent_id: Option<AgentId>,
    capabilities: Option<Vec<Capability>>,
    /// When set (and no explicit identity is pinned), the acting identity +
    /// bounded authority are resolved **per workspace** from the store — the
    /// binary's interim authority until §19 grants land. `None` → the explicit
    /// or trusted-internal path.
    store: Option<Store>,
    /// When set, an `LlmAgent` action runs the §7 agent loop against this client.
    /// `None` → an `LlmAgent` action reports `Failed` ("no LLM client").
    llm: Option<LlmRunner>,
    /// The config storage backends (SOUL §9). When set (alongside `store`, which
    /// resolves runtime backends + the per-user default), the collect pipeline's
    /// [`archive_email`](ActionRunner::archive_email) writes a collected message's
    /// raw `.eml` + attachments to the workspace's files store and links them onto
    /// the row (SOUL §28/§29). `None` → archival is a clean no-op (opt-in by having
    /// a store configured, matching chat uploads).
    storage: Option<Arc<StorageRegistry>>,
}

impl ToolActionRunner {
    /// A runner over `registry` with no identity and **no capability enforcement**
    /// (`capabilities = None` — a trusted/internal caller). Attenuate with
    /// [`with_capabilities`](Self::with_capabilities) to run deny-by-default.
    #[must_use]
    pub fn new(registry: ToolRegistry) -> Self {
        Self {
            registry,
            user_id: None,
            agent_id: None,
            capabilities: None,
            store: None,
            llm: None,
            storage: None,
        }
    }

    /// A runner whose authority is resolved **per workspace** from `store`: each
    /// action runs **as the workspace's owner** (highest-privileged member,
    /// earliest-created on a tie) under bounded base-**Member** capabilities
    /// (SOUL §19) — ordinary read/write, never `*:delete`, `exec:run`, or
    /// admin/MCP-expose. This is the binary's interim authority for automations
    /// until the §19 grants table + policy engine land and supply a per-automation
    /// grant; resolving the identity per call from the `workspace_id` keeps it
    /// multi-tenant correct. An explicit [`as_user`](Self::as_user) /
    /// [`as_agent`](Self::as_agent) / [`with_capabilities`](Self::with_capabilities)
    /// still overrides what this would resolve.
    #[must_use]
    pub fn workspace_owner_authority(registry: ToolRegistry, store: Store) -> Self {
        Self {
            registry,
            user_id: None,
            agent_id: None,
            capabilities: None,
            store: Some(store),
            llm: None,
            storage: None,
        }
    }

    /// Attach the config storage backends so the collect pipeline archives a
    /// collected message's raw `.eml` + attachments to object storage (SOUL
    /// §9/§28/§29; [`archive_email`](ActionRunner::archive_email)). Runtime
    /// (user-added) backends + the per-user default files store resolve through the
    /// attached [`store`](Self::workspace_owner_authority). Without this, archival is
    /// a clean no-op (opt-in by having a store), matching how chat uploads land.
    #[must_use]
    pub fn with_storage(mut self, storage: Arc<StorageRegistry>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Attach the LLM client + default model an `LlmAgent` action runs the §7 loop
    /// against (SOUL §11/§7). Without it, an `LlmAgent` action reports `Failed`.
    /// `default_model` is used when the action doesn't pin its own `model`.
    #[must_use]
    pub fn with_llm(mut self, client: OpenRouterClient, default_model: impl Into<String>) -> Self {
        self.llm = Some(LlmRunner {
            client,
            default_model: default_model.into(),
        });
        self
    }

    /// Act on behalf of a user (tool side effects are authored as this user).
    #[must_use]
    pub fn as_user(mut self, user_id: UserId) -> Self {
        self.user_id = Some(user_id);
        self
    }

    /// Act as an agent (the automation's agent identity; side effects authored as
    /// the agent, SOUL §11/§21).
    #[must_use]
    pub fn as_agent(mut self, agent_id: AgentId) -> Self {
        self.agent_id = Some(agent_id);
        self
    }

    /// Run under `capabilities` — the automation's authority (SOUL §19). With this
    /// set, every action dispatch is capability-checked deny-by-default.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Vec<Capability>) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    fn context(&self, workspace_id: WorkspaceId) -> ToolContext {
        ToolContext {
            workspace_id: Some(workspace_id),
            user_id: self.user_id,
            agent_id: self.agent_id,
            grant_id: None,
            capabilities: self.capabilities.clone(),
            dry_run: false,
            gate: None,
            conversation_id: None,
            ui_id: None,
            registry: None,
        }
    }

    /// The [`ToolContext`] an action dispatches under for `workspace_id`.
    ///
    /// An explicitly pinned identity ([`as_user`](Self::as_user) /
    /// [`as_agent`](Self::as_agent)) wins. Otherwise, when a store is attached,
    /// the acting identity is the **workspace owner** and the authority is bounded
    /// to base **Member** capabilities (SOUL §19; see
    /// [`workspace_owner_authority`](Self::workspace_owner_authority)). With
    /// neither, the context carries no principal (the trusted internal-caller path,
    /// matching [`new`](Self::new)).
    /// Apply the automation's §19 `grant` to a resolved context: the grant **is**
    /// the authority, so it *replaces* the context's capabilities with its bundle
    /// (attenuation — an admin conferred exactly these) and records which grant
    /// authorized the run. `None` leaves the default authority untouched.
    fn apply_grant(mut ctx: ToolContext, grant: Option<&Grant>) -> ToolContext {
        if let Some(g) = grant {
            ctx.capabilities = Some(g.capabilities.clone());
            ctx.grant_id = Some(g.id);
            // A `dry_run` grant simulates every action at dispatch (no side effects).
            ctx.dry_run = g.constraints.dry_run;
        }
        ctx
    }

    async fn resolve_context(
        &self,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
    ) -> Result<ToolContext, String> {
        if self.user_id.is_some() || self.agent_id.is_some() {
            return Ok(Self::apply_grant(self.context(workspace_id), grant));
        }
        let Some(store) = &self.store else {
            return Ok(Self::apply_grant(self.context(workspace_id), grant));
        };
        let owner = workspace_owner(store, workspace_id).await.map_err(|e| {
            format!("resolving workspace {workspace_id} owner for automation authority: {e}")
        })?;
        let ctx = ToolContext {
            workspace_id: Some(workspace_id),
            user_id: Some(owner),
            agent_id: None,
            grant_id: None,
            // Default authority when no grant: an explicitly-set capability set
            // wins, else base Member. A §19 `grant` (applied below) replaces this.
            capabilities: self
                .capabilities
                .clone()
                .or_else(|| Some(catalerum_iam::base_capabilities(Role::Member))),
            dry_run: false,
            gate: None,
            conversation_id: None,
            ui_id: None,
            registry: None,
        };
        Ok(Self::apply_grant(ctx, grant))
    }
}

/// Lets an automation **code/condition node**'s JS reach the registry via
/// `catalerum.callTool` (SOUL §11/§19) through the *same* authority resolution +
/// dispatch gate as an `Action` node — so a code node can do no more than an action
/// node could. The authority (`workspace_id` + the run's §19 `grant`) is resolved
/// exactly as for an action ([`resolve_context`](ToolActionRunner::resolve_context):
/// the run's grant if present, else the workspace owner under bounded base-Member
/// capabilities), and [`ToolRegistry::dispatch`] enforces each tool's
/// `required_capability` deny-by-default against it. There is no separate allow-list
/// or confirm exclusion: an automation runs headless, and the capability cap is the
/// gate (delete/exec/egress stay unreachable unless a grant confers them — in which
/// case the automation was explicitly authorized for them, like any action).
///
/// `call_tool` is synchronous and runs on the script's `spawn_blocking` thread
/// (Boa's `Context` is `!Send`), never a runtime worker — so `block_on` is valid.
impl CodeToolHost for ToolActionRunner {
    fn call_tool(
        &self,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
        tool: &str,
        args: Value,
    ) -> Result<Value, String> {
        if !self.registry.contains(tool) {
            return Err(format!("unknown tool `{tool}`"));
        }
        let handle = tokio::runtime::Handle::current();
        let ctx = handle.block_on(self.resolve_context(workspace_id, grant))?;
        handle
            .block_on(self.registry.dispatch(tool, args, &ctx))
            .map_err(|e| e.to_string())
    }
}

/// The acting identity for a binary-run automation: the workspace's
/// highest-privileged member — Owner, else Admin, else Member, else Viewer —
/// earliest-created on a tie. Errors if the workspace has no members.
async fn workspace_owner(store: &Store, workspace_id: WorkspaceId) -> Result<UserId, String> {
    let members = store
        .memberships()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| e.to_string())?;
    // `list_by_workspace` is ordered created_at ASC; `min_by_key` keeps the first
    // element on a rank tie, so the earliest-created top-rank member wins.
    members
        .into_iter()
        .min_by_key(|m| role_rank(m.role))
        .map(|m| m.user_id)
        .ok_or_else(|| format!("workspace {workspace_id} has no members"))
}

/// Privilege rank (lower = more privileged) for picking the acting owner.
fn role_rank(role: Role) -> u8 {
    match role {
        Role::Owner => 0,
        Role::Admin => 1,
        Role::Member => 2,
        Role::Viewer => 3,
    }
}

/// The registry tool that backs an automation [`ActionKind`], if one exists yet.
fn tool_for(kind: ActionKind) -> Option<&'static str> {
    match kind {
        ActionKind::CreateNote => Some("create_note"),
        ActionKind::EditNote => Some("edit_note"),
        ActionKind::CreateTask => Some("kanban_create_task"),
        ActionKind::MoveTask => Some("kanban_move_task"),
        ActionKind::CreateEvent => Some("create_event"),
        ActionKind::UpdateEvent => Some("update_event"),
        ActionKind::RunCommand => Some("run_command"),
        // Interactive terminal (PTY) nodes (SOUL §20). The five `*_terminal` /
        // `terminal_*` tools are only registered when an executor backend is
        // configured (`[exec]`); without one these resolve to a tool the registry
        // lacks and the action reports "unknown tool". Chained across graph nodes by
        // templating the upstream `open_terminal` node's `session_id` into the
        // downstream node's params (see `render_params`).
        ActionKind::OpenTerminal => Some("open_terminal"),
        ActionKind::TerminalWrite => Some("terminal_write"),
        ActionKind::TerminalRead => Some("terminal_read"),
        ActionKind::PersistTerminal => Some("persist_terminal"),
        ActionKind::CloseTerminal => Some("close_terminal"),
        ActionKind::Notify => Some("notify"),
        // (Re-)index a document source into the vector index (SOUL §6.4/§10). Backed
        // by the `index_document` tool, which is only registered when a vector index
        // is configured (`[qdrant].enabled`); without one this resolves to a tool the
        // registry doesn't hold and the action reports "unknown tool".
        ActionKind::IndexDocument => Some("index_document"),
        // Bulk (re-)index every file under a bucket/prefix (SOUL §6.4/§10). Backed by
        // the `reindex_objects` tool, registered only with a vector index configured.
        ActionKind::ReindexObjects => Some("reindex_objects"),
        // Web fetch (SOUL §27). Backed by the `fetch_url` tool, registered only when
        // a fetch backend is configured (`fetcher.is_some()`, like `POST /fetch`);
        // without one this resolves to a tool the registry lacks → "unknown tool".
        // Gated `web:read`. Chain it into the pure HTML transforms below by
        // templating its `content` into their `html`.
        ActionKind::FetchUrl => Some("fetch_url"),
        // Web search (SOUL §27). Backed by the `web_search` tool, registered only
        // when a search provider is configured (`searcher.is_some()`); without one
        // this resolves to a tool the registry lacks → "unknown tool". Gated
        // `web:search`. Chain a result `url` into a downstream `fetch_url` node.
        ActionKind::WebSearch => Some("web_search"),
        // Pure HTML transforms (SOUL §27): no network, no capability — the
        // `html_to_markdown` / `extract_html` tools are always registered, so these
        // never fail with "unknown tool".
        ActionKind::HtmlToMarkdown => Some("html_to_markdown"),
        ActionKind::ExtractHtml => Some("extract_html"),
        // SQL against an external Postgres connection (SOUL §11). Backed by the
        // always-registered `sql_query` tool; its own invoke enforces
        // `db:read@<conn>` / `db:write@<conn>` against the automation's grant.
        ActionKind::SqlQuery => Some("sql_query"),
        // Object writes (SOUL §9/§11). Backed by the `write_object`/`move_object`
        // tools, registered whenever object storage is configured (alongside
        // `copy_object`); without a store these report "unknown tool". Both gate
        // `storage:write`. `write_object` is a keyed upsert (redelivery-safe);
        // `move_object` deletes its source, so it auto-skips on a redelivery.
        ActionKind::WriteObject => Some("write_object"),
        ActionKind::MoveObject => Some("move_object"),
        // Outbound webhook delivery (SOUL §11/§27). Backed by the `send_webhook`
        // tool, registered when the binary built the guarded sender (whenever it
        // builds the fetcher); gated `web:write` — the egress-write counterpart
        // to `fetch_url`'s `web:read`. A non-2xx receiver status fails the step.
        ActionKind::Webhook => Some("send_webhook"),
        // `LlmAgent`/`Summarize` are handled before this (they run the LLM, not a
        // single tool dispatch).
        // `notify` is only in the registry when a channel is configured
        // (`[channels]`), so without one the action reports "unknown tool".
        _ => None,
    }
}

impl ToolActionRunner {
    /// Publish an automation result into a new chat thread. `title` is optional
    /// (default: "Automation output") and `message` is required; both support the
    /// same `{{ inputs.<node>... }}` templates as tool-backed actions. The initial
    /// row is an assistant message so the thread reads as generated output and can
    /// immediately be continued in the normal chat UI.
    async fn run_create_chat_thread(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("conversation")),
        ) {
            return ActionOutcome::failed(
                "create_chat_thread denied: requires conversation:write authority",
            );
        }

        let params = render_params(&action.params, trigger);
        let title = match params.get("title") {
            Some(Value::String(value)) if !value.trim().is_empty() => value.trim(),
            Some(Value::String(_)) | None => "Automation output",
            Some(_) => {
                return ActionOutcome::failed("create_chat_thread: `title` must be a string")
            }
        };
        let message = match params.get("message") {
            Some(Value::String(value)) if !value.trim().is_empty() => value.trim(),
            Some(Value::String(_)) | None => {
                return ActionOutcome::failed(
                    "create_chat_thread: `message` is required and must not be empty",
                )
            }
            Some(_) => {
                return ActionOutcome::failed("create_chat_thread: `message` must be a string")
            }
        };

        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({
                "dry_run": true,
                "title": title,
                "message": message,
            })));
        }
        let Some(store) = &self.store else {
            return ActionOutcome::failed(
                "automation action `CreateChatThread` needs an attached store",
            );
        };
        match store
            .conversations()
            .create_with_initial_message(
                workspace_id,
                Some(title),
                Origin::Automation,
                MessageRole::Assistant,
                message,
            )
            .await
        {
            Ok((conversation, stored_message)) => ActionOutcome::succeeded(Some(json!({
                "conversation_id": conversation.id,
                "message_id": stored_message.id,
                "title": conversation.title,
                "message": stored_message.content,
            }))),
            Err(e) => ActionOutcome::failed(format!("create_chat_thread: {e}")),
        }
    }

    /// Run an `LlmAgent` action: drive the §7 agent loop ([`run_agent`]) seeded +
    /// confined by the action's `system`/`model`/`tools`, dispatching tools through
    /// the resolved (capability-gated) [`ToolContext`]. The outcome carries the
    /// agent's final text + a tool-call count. `Failed` if no LLM client is attached
    /// or the params don't parse as an [`LlmAgent`].
    async fn run_llm_agent(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(llm) = &self.llm else {
            return ActionOutcome::failed(
                "automation action `LlmAgent` needs an LLM client; this runner has none",
            );
        };
        let Some(agent) = action.as_llm_agent() else {
            return ActionOutcome::failed("malformed `llm_agent` action parameters");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(e) => return ActionOutcome::failed(e),
        };
        // System prompt = the agent's own (or a default) + the runbook instructions
        // of any skills it names (SOUL §23). Skills are best-effort: a missing one is
        // logged and skipped, never fails the run.
        let mut base = agent
            .system
            .clone()
            .unwrap_or_else(|| DEFAULT_AGENT_SYSTEM.to_string());
        // JSON-output steering: tell the model to emit only a JSON value so the
        // step's `data` can feed downstream nodes (SOUL §11).
        if agent.wants_json() {
            base.push_str(JSON_OUTPUT_INSTRUCTION);
        }
        let skills = self
            .load_skill_instructions(workspace_id, &agent.skills)
            .await;
        let system = system_with_skills(&base, &skills);
        let (request, allowed) = agent_request(&agent, &llm.default_model, system, trigger);
        // Enforce the grant's `cost_limit` (§19) as a per-run spend ceiling: the loop
        // halts before another paid turn once cumulative `usage.cost_usd` reaches it.
        // `None` (no grant / no cost cap) leaves the run uncapped. This is why a
        // `cost_limit` grant is no longer rejected as "unenforced" by the executor.
        let config = AgentConfig {
            cost_limit: grant.and_then(|g| g.constraints.cost_limit),
            // Deferred advertising (SOUL §7): an unconfined agent action starts
            // from the discovery tools and loads the rest on demand; an explicit
            // `tools` list keeps full advertising of that set.
            discovery_tools: if allowed.is_none() {
                crate::tools::discovery_seed()
            } else {
                Vec::new()
            },
            ..AgentConfig::default()
        };
        match run_agent(
            &llm.client,
            request,
            &self.registry,
            &ctx,
            &config,
            allowed.as_deref(),
        )
        .await
        {
            Ok(outcome) => {
                self.finish_agent_run(workspace_id, &ctx, trigger, agent.wants_json(), outcome)
                    .await
            }
            Err(e) => ActionOutcome::failed(format!("llm agent loop failed: {e}")),
        }
    }

    /// Shared post-run handling for an agent-loop action (`LlmAgent` / `RunSkill`):
    /// warn if the loop hit its iteration, repeated-tool, or grant `cost_limit` (§19)
    /// cap — the reply is then a best-effort partial, surfaced rather than presented as a clean finish
    /// — deliver the reply back to a channel-message trigger (§25), and map the outcome
    /// to an [`ActionOutcome`], parsing the reply into `data` when JSON-steered.
    async fn finish_agent_run(
        &self,
        workspace_id: WorkspaceId,
        ctx: &ToolContext,
        trigger: Option<&Value>,
        wants_json: bool,
        outcome: AgentOutcome,
    ) -> ActionOutcome {
        if outcome.hit_iteration_cap {
            tracing::warn!(
                workspace = %workspace_id,
                iterations = outcome.iterations,
                "agent action hit its iteration cap; the reply may be incomplete"
            );
        }
        if outcome.hit_tool_loop_cap {
            tracing::warn!(
                workspace = %workspace_id,
                iterations = outcome.iterations,
                "agent action stopped a repeated/unsuccessful tool-call loop; the reply may be incomplete"
            );
        }
        if outcome.hit_cost_limit {
            tracing::warn!(
                workspace = %workspace_id,
                cost_usd = ?outcome.usage.as_ref().and_then(|u| u.cost_usd),
                "agent action hit its grant cost_limit; the reply may be incomplete"
            );
        }
        // Channel chatbot: deliver the agent's reply back to the channel that triggered
        // it (SOUL §25). Best-effort — reuses the `notify` tool under the automation's
        // authority (base Member holds `channel:write`); a no-op unless the trigger was
        // a channel message and `[channels]` is on.
        self.deliver_channel_reply(ctx, trigger, &outcome.content)
            .await;
        // For a JSON-steered agent, parse the reply so downstream nodes get structured
        // `data` (best-effort: unparseable → no `data`, the raw `content` is still there).
        let data = if wants_json {
            extract_json(&outcome.content)
        } else {
            None
        };
        agent_outcome_to_action(&outcome, data)
    }

    /// Run a `Summarize` action (SOUL §11): a **one-shot** LLM completion — no
    /// tools, no agent loop — condensing the action's (template-rendered) `input`
    /// (or, unset, the firing trigger event) into a `summary` downstream nodes can
    /// template (`{{ inputs.<node>.summary }}`). Params: `input`, `instructions`,
    /// `max_words`, `model` (defaults to the runner's). A `dry_run` grant is
    /// checked here (the direct-action manual check, like `run_write_email` —
    /// registry simulation can't cover a non-tool action), so a simulated run
    /// spends no tokens. A grant `cost_limit` can't halt a single completion
    /// mid-request (there is no "next turn" to refuse, unlike the §7 loop); the
    /// spend of the one call is reported as `cost_usd`.
    async fn run_summarize(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(llm) = &self.llm else {
            return ActionOutcome::failed(
                "automation action `Summarize` needs an LLM client; this runner has none",
            );
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        // `input` commonly references upstream output (`{{ inputs.fetch.content }}`),
        // so render against the run context like a tool-backed action's params.
        let params = render_params(&action.params, trigger);
        let request = match summarize_request(&params, trigger, &llm.default_model) {
            Ok(r) => r,
            Err(e) => return ActionOutcome::failed(e),
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({
                "dry_run": true,
                "model": request.model,
            })));
        }
        match llm.client.chat(request.clone()).await {
            Ok(turn) => {
                let summary = turn.content.trim().to_string();
                if summary.is_empty() {
                    return ActionOutcome::failed("summarize: the model returned no content");
                }
                let mut out = json!({ "summary": summary, "model": request.model });
                if let Some(cost) = turn.usage.as_ref().and_then(|u| u.cost_usd) {
                    out["cost_usd"] = json!(cost);
                }
                ActionOutcome::succeeded(Some(out))
            }
            Err(e) => ActionOutcome::failed(format!("summarize: llm request failed: {e}")),
        }
    }

    /// Run a `RunProfile` action: resolve the named §19 agent profile and drive its
    /// loop ([`crate::profile_agent::run_profile`]) under **its own** grant — which
    /// must be **⊆ this automation's grant** (attenuation; the ceiling is `grant`).
    /// The user turn is the action's explicit `input`, else the firing channel
    /// message's text, else a description of the trigger; a channel-message trigger
    /// also routes the reply back to that channel. `Failed` if no LLM client / store
    /// is attached or the params don't parse.
    async fn run_profile_action(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(llm) = &self.llm else {
            return ActionOutcome::failed(
                "automation action `RunProfile` needs an LLM client; this runner has none",
            );
        };
        let Some(store) = &self.store else {
            return ActionOutcome::failed(
                "automation action `RunProfile` needs a store to resolve the profile",
            );
        };
        let Some(spec) = action.as_run_profile() else {
            return ActionOutcome::failed("malformed `run_profile` action parameters");
        };
        let profile = match store
            .agent_profiles()
            .get_by_name(workspace_id, spec.profile.trim())
            .await
        {
            Ok(Some(p)) => p,
            Ok(None) => {
                return ActionOutcome::failed(format!(
                    "run_profile: profile `{}` not found in this workspace",
                    spec.profile
                ))
            }
            Err(e) => return ActionOutcome::failed(format!("run_profile: resolving profile: {e}")),
        };
        // User turn: explicit input → else the firing channel message's text → else a
        // description of the trigger.
        let user_text = spec
            .input
            .clone()
            .or_else(|| channel_text_from_trigger(trigger))
            .unwrap_or_else(|| trigger_prompt(trigger));
        let reply_channel = channel_from_trigger(trigger);
        // Attenuation ceiling: the automation's grant (when run under one). `None`
        // lets the profile run under its own grant with no further ceiling.
        let ceiling = grant.map(|g| g.capabilities.clone());
        match crate::profile_agent::run_profile(
            store,
            &llm.client,
            &self.registry,
            &llm.default_model,
            &profile,
            &user_text,
            reply_channel.as_deref(),
            ceiling.as_deref(),
        )
        .await
        {
            Ok(outcome) => agent_outcome_to_action(&outcome, None),
            Err(e) => ActionOutcome::failed(format!("run_profile failed: {e}")),
        }
    }

    /// Run a `RunSkill` action: resolve the named workspace skill (§23) and drive the
    /// §7 agent loop ([`run_agent`]) with the skill's `instructions_md` as the system
    /// prompt and its `tools` as the advertised set, under the automation's authority.
    /// Capability-gated `skill:use@<name>` (the same per-skill check the `use_skill`
    /// tool enforces, §19); the skill's tools are *still* each gated by the automation's
    /// grant on dispatch, so the skill can do no more than the automation could. The
    /// user turn is the action's explicit `input`, else the firing channel message's
    /// text, else a description of the trigger; a channel-message trigger routes the
    /// reply back to that channel. `Failed` if no LLM client / store is attached, the
    /// params don't parse, the skill is unknown, or the authority lacks `skill:use`.
    async fn run_skill_action(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(llm) = &self.llm else {
            return ActionOutcome::failed(
                "automation action `RunSkill` needs an LLM client; this runner has none",
            );
        };
        let Some(store) = &self.store else {
            return ActionOutcome::failed(
                "automation action `RunSkill` needs a store to resolve the skill",
            );
        };
        let Some(spec) = action.as_run_skill() else {
            return ActionOutcome::failed("malformed `run_skill` action parameters");
        };
        let skill = match store
            .skills()
            .get_by_name(workspace_id, spec.skill.trim())
            .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                return ActionOutcome::failed(format!(
                    "run_skill: skill `{}` not found in this workspace",
                    spec.skill
                ))
            }
            Err(e) => return ActionOutcome::failed(format!("run_skill: resolving skill: {e}")),
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(e) => return ActionOutcome::failed(e),
        };
        // Per-skill capability gate (`skill:use@<name>`, §19/§23) — the same check the
        // `use_skill` tool makes. Base Member holds whole-domain `skill:use`, so the
        // default authority passes; a narrowed grant must cover this skill by name.
        let required = Capability::new(CapAction::Use, Resource::new("skill", &skill.name));
        if !cap_allows(&ctx, &required) {
            return ActionOutcome::failed(format!(
                "run_skill: the automation's authority does not permit skill:use@{}",
                skill.name
            ));
        }
        // System prompt = the skill's runbook (or the default if it carries none), plus
        // the JSON-output instruction when steered.
        let mut system = if skill.instructions_md.trim().is_empty() {
            DEFAULT_AGENT_SYSTEM.to_string()
        } else {
            skill.instructions_md.clone()
        };
        if spec.wants_json() {
            system.push_str(JSON_OUTPUT_INSTRUCTION);
        }
        // User turn: explicit input → firing channel message text → trigger description.
        let user_text = spec
            .input
            .clone()
            .or_else(|| channel_text_from_trigger(trigger))
            .unwrap_or_else(|| trigger_prompt(trigger));
        let model = spec
            .model
            .clone()
            .unwrap_or_else(|| llm.default_model.clone());
        let request = ChatRequest::new(
            model,
            vec![ChatMessage::system(system), ChatMessage::user(user_text)],
        );
        // Confine the loop to the skill's tool set (empty = advertise the whole
        // registry, still grant-bounded) and cap spend at the grant's `cost_limit` (§19).
        let allowed = if skill.tools.is_empty() {
            None
        } else {
            Some(skill.tools.clone())
        };
        let config = AgentConfig {
            cost_limit: grant.and_then(|g| g.constraints.cost_limit),
            // Deferred advertising (SOUL §7), same as `LlmAgent` above.
            discovery_tools: if allowed.is_none() {
                crate::tools::discovery_seed()
            } else {
                Vec::new()
            },
            ..AgentConfig::default()
        };
        match run_agent(
            &llm.client,
            request,
            &self.registry,
            &ctx,
            &config,
            allowed.as_deref(),
        )
        .await
        {
            Ok(outcome) => {
                self.finish_agent_run(workspace_id, &ctx, trigger, spec.wants_json(), outcome)
                    .await
            }
            Err(e) => ActionOutcome::failed(format!("run_skill failed: {e}")),
        }
    }

    /// Deliver an agent's reply back to the channel that triggered it (SOUL §25):
    /// when `trigger` is a `ChannelMessage`, dispatch the `notify` tool with the
    /// agent's `content` to that channel. Best-effort — a non-channel trigger, an
    /// empty reply, no `notify` tool (`[channels]` off), or a delivery failure is a
    /// logged no-op that never fails the run. This makes "an inbound channel message
    /// triggers an agent that replies on the channel" work without the model having
    /// to remember to call `notify` itself.
    async fn deliver_channel_reply(
        &self,
        ctx: &ToolContext,
        trigger: Option<&Value>,
        content: &str,
    ) {
        let Some(channel) = channel_from_trigger(trigger) else {
            return;
        };
        let reply = content.trim();
        if reply.is_empty() {
            return;
        }
        let args = json!({ "channel": &channel, "message": reply });
        if let Err(e) = self.registry.dispatch("notify", args, ctx).await {
            tracing::warn!(error = %e, %channel, "agent channel reply failed (run still ok)");
        }
    }

    /// Load the `instructions_md` of each named skill in `workspace_id` (SOUL §23),
    /// in order, skipping (with a warning) any that are missing or unreadable. Empty
    /// when no store is attached or no skills are named.
    async fn load_skill_instructions(
        &self,
        workspace_id: WorkspaceId,
        skills: &[String],
    ) -> Vec<String> {
        let Some(store) = &self.store else {
            return Vec::new();
        };
        // One query for all named skills (not an N+1 `get_by_name` each), then look
        // them up by name preserving the agent's declared order.
        let by_name: std::collections::HashMap<String, _> =
            match store.skills().get_many_by_name(workspace_id, skills).await {
                Ok(found) => found.into_iter().map(|s| (s.name.clone(), s)).collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to load skills; skipping all");
                    return Vec::new();
                }
            };
        let mut out = Vec::new();
        for name in skills {
            match by_name.get(name) {
                Some(skill) => out.push(skill.instructions_md.clone()),
                None => {
                    tracing::warn!(skill = %name, "llm_agent names an unknown skill; skipping")
                }
            }
        }
        out
    }
}

/// Map an [`AgentOutcome`] to an `LlmAgent` step's [`ActionOutcome`]. An agent that
/// produced **neither text nor any tool call** accomplished nothing — that is a
/// `Failed` step, **surfaced in the run history**, not a silent empty success (e.g.
/// an LLM backend that returned an empty stream). Otherwise it succeeds, carrying
/// the final text + iteration / tool-call counts. Pure, so the rule is unit-testable
/// without a live LLM.
fn agent_outcome_to_action(outcome: &AgentOutcome, data: Option<Value>) -> ActionOutcome {
    if outcome.content.trim().is_empty() && outcome.tool_invocations.is_empty() {
        return ActionOutcome::failed(
            "llm agent produced no output (empty response and no tool calls) — the model or \
             LLM backend may have returned nothing",
        );
    }
    let mut out = json!({
        "content": outcome.content,
        "iterations": outcome.iterations,
        "tool_calls": outcome.tool_invocations.len(),
    });
    // Surface the run's cost + truncation status so the run detail can show a cost
    // chip / "budget reached" badge (§19) rather than presenting a budget- or
    // iteration-capped partial reply as a clean finish.
    if let Some(cost) = outcome.usage.as_ref().and_then(|u| u.cost_usd) {
        out["cost_usd"] = json!(cost);
    }
    if outcome.hit_cost_limit {
        out["cost_capped"] = json!(true);
    }
    if outcome.hit_iteration_cap {
        out["iteration_capped"] = json!(true);
    }
    if outcome.hit_tool_loop_cap {
        out["tool_loop_capped"] = json!(true);
    }
    // A JSON-steered agent attaches its parsed reply as `data` for downstream nodes.
    if let Some(data) = data {
        out["data"] = data;
    }
    ActionOutcome::succeeded(Some(out))
}

/// Appended to a JSON-steered agent's system prompt so its final reply is a single
/// parseable JSON value (the executor extracts it into the step's `data`).
const JSON_OUTPUT_INSTRUCTION: &str = "\n\nIMPORTANT: Your final reply MUST be a single \
valid JSON value and NOTHING else — no prose, no explanation, no markdown code fences.";

/// Extract a JSON value from an LLM reply: strip a surrounding ```json … ``` (or
/// bare ``` … ```) markdown fence if present, then parse. `None` if it doesn't
/// parse. Pure + testable.
fn extract_json(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    // Strip a leading fence (``` or ```json) and a trailing ``` if present.
    let body = trimmed
        .strip_prefix("```")
        .map(|rest| {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            let rest = rest.trim_start_matches(['\n', '\r']);
            rest.strip_suffix("```").unwrap_or(rest).trim()
        })
        .unwrap_or(trimmed);
    serde_json::from_str(body).ok()
}

/// Compose the agent's system prompt: its base instructions followed by each
/// named skill's runbook (SOUL §23), under a clear header. Pure so the composition
/// is unit-testable; an empty `skill_instructions` returns `base` unchanged.
fn system_with_skills(base: &str, skill_instructions: &[String]) -> String {
    if skill_instructions.is_empty() {
        return base.to_string();
    }
    let mut system = base.to_string();
    system.push_str("\n\n# Skills\n\nYou have been given these skills (runbooks); follow them:\n");
    for instructions in skill_instructions {
        system.push_str("\n---\n");
        system.push_str(instructions);
    }
    system
}

/// The seed user turn for an automation agent: it has no human prompt, so describe
/// the firing trigger (if any) then the standing nudge. Pure + testable.
fn trigger_prompt(trigger: Option<&Value>) -> String {
    match trigger {
        Some(t) => format!(
            "This automation was triggered by the following event:\n\n```json\n{}\n```\n\n{}",
            serde_json::to_string_pretty(t).unwrap_or_else(|_| t.to_string()),
            AGENT_TRIGGER_PROMPT,
        ),
        None => AGENT_TRIGGER_PROMPT.to_string(),
    }
}

/// The firing event for an action, resolving **both** runtime-context shapes
/// (mirrors [`collect_item`]): in a **graph** run the executor wraps the context as
/// `{ "trigger": <event>, "inputs": … }`, so the event is at `trigger`; in the
/// **linear** loop the context *is* the event. A serialized `TriggerEvent` never
/// carries a top-level `trigger` key, so its presence unambiguously marks the graph
/// shape. `None` for a manual/no-trigger run. Without this, the channel helpers read
/// `kind`/`channel`/`text` off the *wrapper* in graph runs and find nothing — silently
/// breaking the SOUL §25 in-channel reply for graph automations.
fn firing_event(trigger: Option<&Value>) -> Option<&Value> {
    let t = trigger?;
    match t.get("trigger") {
        Some(event) => Some(event),
        None => Some(t),
    }
}

/// The channel a `ChannelMessage` trigger fired on, if this run was triggered by an
/// inbound channel message — so a chatbot agent's reply can be delivered there.
/// `None` for any other trigger (or a malformed one). Pure + testable.
fn channel_from_trigger(trigger: Option<&Value>) -> Option<String> {
    let t = firing_event(trigger)?;
    if t.get("kind").and_then(Value::as_str) != Some("channel_message") {
        return None;
    }
    t.get("channel").and_then(Value::as_str).map(str::to_string)
}

/// The message text of a `ChannelMessage` trigger, if this run was fired by an
/// inbound channel message — so a `RunProfile` action responds to what was actually
/// said. `None` for any other trigger (or a textless one). Pure + testable.
fn channel_text_from_trigger(trigger: Option<&Value>) -> Option<String> {
    let t = firing_event(trigger)?;
    if t.get("kind").and_then(Value::as_str) != Some("channel_message") {
        return None;
    }
    t.get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Build the seed [`ChatRequest`] + advertised tool subset for an [`LlmAgent`]
/// action. Pure (no I/O) so the seeding/model/tool-restriction rules are unit
/// testable: the model defaults to `default_model` unless the action pins one; the
/// seed is `[system, user-turn]` (the prebuilt `system`; the user turn describes the
/// `trigger`); and an empty `tools` list means "advertise the whole registry"
/// (`None`), while a non-empty list confines the loop to that subset.
/// The default system prompt for a `Summarize` action.
const SUMMARIZE_SYSTEM: &str = "You are a summarizer. Condense the material the user provides \
into a clear, faithful summary. Reply with the summary only — no preamble, no meta-commentary.";

/// Cap on the bytes of input a `Summarize` action feeds the model — a defensive
/// bound so a huge upstream output (a whole fetched page, a big file's text)
/// degrades to a truncated-input summary instead of a hard model-context error.
const MAX_SUMMARIZE_INPUT_BYTES: usize = 200 * 1024;

/// Build the one-shot [`ChatRequest`] for a `Summarize` action (SOUL §11). Pure
/// (no I/O) so the input/instruction/model rules are unit-testable. `params`
/// must already be template-rendered: the summarized `input` is the param when
/// set (a non-string JSON — e.g. a whole-object `{{ inputs.x }}` reference — is
/// pretty-printed), else the firing trigger event; over-long input is truncated
/// on a char boundary with an explicit marker. `instructions` and a `max_words`
/// bound extend the system prompt; `model` falls back to `default_model`.
fn summarize_request(
    params: &serde_json::Map<String, Value>,
    trigger: Option<&Value>,
    default_model: &str,
) -> Result<ChatRequest, String> {
    let mut input = match params.get("input") {
        None | Some(Value::Null) => match firing_event(trigger) {
            Some(event) => {
                serde_json::to_string_pretty(event).unwrap_or_else(|_| event.to_string())
            }
            None => {
                return Err(
                    "summarize: no `input` param and no firing trigger to summarize".to_string(),
                )
            }
        },
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(
                    "summarize: `input` resolved to an empty string — check the \
                     {{ path }} reference"
                        .to_string(),
                );
            }
            s.to_string()
        }
        // A whole-value template (`"{{ inputs.fetch }}"`) resolves to non-string
        // JSON — summarize its pretty form rather than erroring.
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    if input.len() > MAX_SUMMARIZE_INPUT_BYTES {
        let mut end = MAX_SUMMARIZE_INPUT_BYTES;
        while end > 0 && !input.is_char_boundary(end) {
            end -= 1;
        }
        input.truncate(end);
        input.push_str("\n\n[input truncated]");
    }

    let mut system = SUMMARIZE_SYSTEM.to_string();
    if let Some(instructions) = params
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        system.push_str("\n\n");
        system.push_str(instructions);
    }
    if let Some(words) = params
        .get("max_words")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)
    {
        system.push_str(&format!("\n\nKeep the summary under {words} words."));
    }

    let model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model)
        .to_string();
    Ok(ChatRequest::new(
        model,
        vec![ChatMessage::system(system), ChatMessage::user(input)],
    ))
}

fn agent_request(
    agent: &LlmAgent,
    default_model: &str,
    system: String,
    trigger: Option<&Value>,
) -> (ChatRequest, Option<Vec<String>>) {
    let seed = vec![
        ChatMessage::system(system),
        ChatMessage::user(trigger_prompt(trigger)),
    ];
    let model = agent
        .model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    let allowed = if agent.tools.is_empty() {
        None
    } else {
        Some(agent.tools.clone())
    };
    let mut request = ChatRequest::new(model, seed);
    // The node's "thinking" picker (SOUL §7/§11): a non-empty effort is requested
    // for the loop; empty/absent leaves it to the provider default.
    request.reasoning_effort = agent
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (request, allowed)
}

/// The collect-write actions (SOUL §10/§28).
impl ToolActionRunner {
    /// `WriteEmail` — persist a collected message (idempotent upsert by
    /// `(mailbox_id, uid)`) and enqueue the §10 chunk→embed→project pipeline. The
    /// message rides on the firing `CollectEmail` trigger. Gated `email:write`; the
    /// output carries the stored email's id/uid so the collect finalizer can key its
    /// cursor commit on this node.
    async fn run_write_email(
        &self,
        workspace_id: WorkspaceId,
        _action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("write_email needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("write_email denied: requires email:write authority");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "write_email: no email on the trigger — place it downstream of a CollectEmail trigger",
            );
        };
        let mut email: Email = match serde_json::from_value(item.clone()) {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "write_email: trigger item is not an email: {e}"
                ))
            }
        };
        // The write target is governed by the **action's** authority scope, never the
        // (deserialized) item's — force the workspace so a crafted item can't land in
        // another tenant (defense-in-depth; the collect job already scopes correctly).
        email.workspace_id = workspace_id;
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "uid": email.uid })));
        }
        // Idempotent-redelivery signal (SOUL §11/§29): probe whether this
        // `(mailbox_id, uid)` is **already stored** before the upsert. `newly_written`
        // is `false` when the message was already persisted — an at-least-once
        // REDELIVERY (the crash window between an earlier run's write and its ledger
        // commit, or a snapshot provider re-emitting). The DAG executor latches this
        // into a per-run `redelivery` flag, so the non-idempotent nodes downstream of
        // this write (a classifying `LlmAgent`, a `LabelEmail`) auto-**skip** instead of
        // double-firing (no double-spent tokens, no re-label) — while this *idempotent*
        // write still runs so its success advances the collect cursor (`commit_on`).
        // Read the store, not the ledger: the row survives a crash the ledger doesn't.
        let newly_written = match store
            .emails()
            .get_by_uid(workspace_id, email.mailbox_id, &email.uid)
            .await
        {
            Ok(_) => false,
            Err(catalerum_store::StoreError::NotFound) => true,
            // A probe error is not fatal — treat it as "assume new" and let the upsert
            // (which hits the same store) surface any real failure, rather than
            // wrongly suppressing a first-delivery classify.
            Err(e) => {
                tracing::warn!(error = %e, uid = %email.uid, "write_email: newly-written probe failed; assuming new");
                true
            }
        };
        let stored = match store.emails().upsert_by_uid(&email).await {
            Ok(e) => e,
            Err(e) => return ActionOutcome::failed(format!("write_email upsert: {e}")),
        };
        // The §10 projection is derived/rebuildable, so a failed enqueue is logged,
        // not fatal — the email is already in Postgres truth and re-projects on a
        // later write of the same uid.
        if let Err(e) =
            catalerum_ingest::enqueue_ingest_email(&store, workspace_id, stored.id).await
        {
            tracing::warn!(error = %e, email = %stored.id, "write_email: failed to enqueue ingest_email projection");
        }
        ActionOutcome::succeeded(Some(json!({
            "email_id": stored.id,
            "mailbox_id": stored.mailbox_id,
            "uid": stored.uid,
            "newly_written": newly_written,
        })))
    }

    /// `WriteEvent` — the calendar twin of [`run_write_email`](Self::run_write_email):
    /// idempotent upsert of a collected event by `(calendar_id, uid)` + enqueue the
    /// event graph projection. An optional `calendar_id` param redirects the write
    /// into a specific **local** calendar instead of the collect source's mirror.
    /// Gated `calendar:write`.
    async fn run_write_event(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("write_event needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("calendar")),
        ) {
            return ActionOutcome::failed("write_event denied: requires calendar:write authority");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "write_event: no event on the trigger — place it downstream of a CollectCalendar trigger",
            );
        };
        let mut event: Event = match serde_json::from_value(item.clone()) {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "write_event: trigger item is not an event: {e}"
                ))
            }
        };
        // Scope the write to the action's workspace, not the item's (defense-in-depth).
        event.workspace_id = workspace_id;
        // A dry run must not touch the store — return before the redirect below
        // resolves (and may create) a calendar or the probe reads for redelivery.
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "uid": event.uid })));
        }
        // Optional (templatable) redirect into a specific **local** calendar,
        // overriding the collect source's mirror. Two param spellings, both
        // accepting a name-or-id: `calendar_id` (historically a UUID) and
        // `calendar` (a calendar name). A name is get-or-created idempotently, so
        // an automation authored against a not-yet-existing calendar — e.g.
        // "Feiertage Bayern" for a Bavaria-holidays ICS collect — writes into a
        // freshly-made local calendar of that name instead of silently landing in
        // the collect source's mirror. Local calendars only: WriteEvent is a
        // store-mirror write that never loops back to a provider (SOUL §8), so a
        // row planted in a provider calendar's mirror would silently vanish on its
        // next snapshot sync.
        let rendered = render_params(&action.params, trigger);
        let target = rendered
            .get("calendar_id")
            .or_else(|| rendered.get("calendar"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(arg) = target {
            match resolve_write_calendar(&store, workspace_id, arg).await {
                Ok(calendar) => event.calendar_id = calendar.id,
                Err(e) => return ActionOutcome::failed(format!("write_event: {e}")),
            }
        }
        // Idempotent-redelivery signal (SOUL §11/§29) — the calendar twin of the
        // `WriteEmail` probe: read the store for this `(calendar_id, uid)` **before** the
        // upsert. `newly_written` is `false` when the event was already persisted (an
        // at-least-once redelivery), which the DAG executor latches into a per-run
        // `redelivery` flag so any downstream non-idempotent node (a `CollectCalendar →
        // LlmAgent` classify) auto-skips instead of double-firing — while this idempotent
        // write still runs so its success advances the collect cursor (`commit_on`). Read
        // the store, not the ledger: the row survives a crash the ledger doesn't.
        let newly_written = match store
            .events()
            .get_by_uid(workspace_id, event.calendar_id, &event.uid)
            .await
        {
            Ok(_) => false,
            Err(catalerum_store::StoreError::NotFound) => true,
            // A probe error is not fatal — assume new and let the upsert (same store)
            // surface any real failure, rather than wrongly suppressing a first classify.
            Err(e) => {
                tracing::warn!(error = %e, uid = %event.uid, "write_event: newly-written probe failed; assuming new");
                true
            }
        };
        let upsert = catalerum_ingest::event_to_upsert(&event);
        let stored = match store.events().upsert_by_uid(&upsert).await {
            Ok(e) => e,
            Err(e) => return ActionOutcome::failed(format!("write_event upsert: {e}")),
        };
        if let Err(e) =
            catalerum_ingest::enqueue_project_event(&store, workspace_id, stored.id).await
        {
            tracing::warn!(error = %e, event = %stored.id, "write_event: failed to enqueue project_event");
        }
        ActionOutcome::succeeded(Some(json!({
            "event_id": stored.id,
            "calendar_id": stored.calendar_id,
            "uid": stored.uid,
            "newly_written": newly_written,
        })))
    }

    /// `LabelEmail` — record a classifier verdict on a **stored** email (SOUL
    /// §11/§28): a full replace of its free-text `labels`. The target is resolved
    /// from the firing trigger's `(mailbox_id, uid)` (so it labels the message an
    /// upstream `WriteEmail` persisted); `labels` come from the action `params`
    /// (templatable, e.g. `{ "labels": "{{ inputs.classify.labels }}" }`). Idempotent;
    /// gated `email:write`.
    async fn run_label_email(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("label_email needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("label_email denied: requires email:write authority");
        }
        // `labels` may be templated from an upstream classifier's output.
        let rendered = render_params(&action.params, trigger);
        let mut seen = std::collections::HashSet::new();
        let labels: Vec<String> = rendered
            .get("labels")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            // Order-preserving de-dup of the whole set (Vec::dedup only drops
            // consecutive repeats — a classifier may emit ["a","b","a"]).
            .filter(|s| seen.insert(s.clone()))
            .collect();
        if labels.is_empty() {
            return ActionOutcome::failed("label_email: `labels` must be a non-empty string array");
        }
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "label_email: no email on the trigger — it labels the message an upstream WriteEmail wrote",
            );
        };
        let Some(mailbox_id) = item
            .get("mailbox_id")
            .and_then(|v| serde_json::from_value::<MailboxId>(v.clone()).ok())
        else {
            return ActionOutcome::failed("label_email: trigger item is missing a mailbox_id");
        };
        let Some(uid) = item.get("uid").and_then(Value::as_str) else {
            return ActionOutcome::failed("label_email: trigger item is missing a uid");
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "labels": labels })));
        }
        let email = match store
            .emails()
            .get_by_uid(workspace_id, mailbox_id, uid)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "label_email: target email not found (write it first with WriteEmail): {e}"
                ))
            }
        };
        match store
            .emails()
            .set_labels(workspace_id, email.id, &labels)
            .await
        {
            Ok(updated) => ActionOutcome::succeeded(Some(json!({
                "email_id": updated.id,
                "labels": updated.labels,
            }))),
            Err(e) => ActionOutcome::failed(format!("label_email set_labels: {e}")),
        }
    }

    /// `MarkEmailRead` — set a **stored** email's local `seen` flag (SOUL
    /// §11/§28), or clear it with `"unread": true`. The target is resolved from
    /// the firing trigger's `(mailbox_id, uid)` like `LabelEmail`. **Local
    /// only** (§14): the provider's mailbox is never written, so a provider
    /// re-sync may overwrite it. Idempotent; gated `email:write`.
    async fn run_mark_email_read(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(store) = self.store.clone() else {
            return ActionOutcome::failed("mark_email_read needs a store-backed runner");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("email")),
        ) {
            return ActionOutcome::failed("mark_email_read denied: requires email:write authority");
        }
        // `unread` may be templated (e.g. from an upstream classifier's verdict).
        let rendered = render_params(&action.params, trigger);
        let unread = rendered
            .get("unread")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let Some(item) = collect_item(trigger) else {
            return ActionOutcome::failed(
                "mark_email_read: no email on the trigger — it marks the message an upstream WriteEmail wrote",
            );
        };
        let Some(mailbox_id) = item
            .get("mailbox_id")
            .and_then(|v| serde_json::from_value::<MailboxId>(v.clone()).ok())
        else {
            return ActionOutcome::failed("mark_email_read: trigger item is missing a mailbox_id");
        };
        let Some(uid) = item.get("uid").and_then(Value::as_str) else {
            return ActionOutcome::failed("mark_email_read: trigger item is missing a uid");
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "unread": unread })));
        }
        let email = match store
            .emails()
            .get_by_uid(workspace_id, mailbox_id, uid)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                return ActionOutcome::failed(format!(
                    "mark_email_read: target email not found (write it first with WriteEmail): {e}"
                ))
            }
        };
        match store
            .emails()
            .set_seen(workspace_id, email.id, !unread)
            .await
        {
            Ok(updated) => ActionOutcome::succeeded(Some(json!({
                "email_id": updated.id,
                "unread": unread,
            }))),
            Err(e) => ActionOutcome::failed(format!("mark_email_read set_seen: {e}")),
        }
    }
}

/// The collected item carried on a collect trigger's firing event: in a graph run
/// the runtime ctx is `{ "trigger": <event>, "inputs": … }`, so the item is at
/// `trigger.item`; in the linear loop the ctx *is* the event, so it is at `item`.
fn collect_item(trigger: Option<&Value>) -> Option<&Value> {
    let t = trigger?;
    if let Some(item) = t.get("trigger").and_then(|e| e.get("item")) {
        return Some(item);
    }
    t.get("item")
}

/// A stable, workspace-unique `external_id` for a *named* local calendar (SOUL
/// §8) — a slug of the name so [`CalendarRepo::upsert_local`] get-or-creates the
/// same row on every `WriteEvent` run for the same name (its local unique index
/// keys on `external_id`). The `named-` prefix keeps it out of the reserved
/// `default` key (the auto-provisioned default local calendar), so asking to
/// write to a calendar literally named "default" never renames that one.
fn local_calendar_external_id(name: &str) -> String {
    let slug: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    format!("named-{}", if slug.is_empty() { "cal" } else { slug })
}

/// Get-or-create a writable **local** calendar by name (case-insensitive, SOUL
/// §8). An existing local calendar of that name is reused; otherwise a new one
/// is created, keyed on [`local_calendar_external_id`]. This is the single
/// get-or-create path shared by the `create_calendar` tool, `POST /calendars`,
/// and a `WriteEvent` name redirect — so asking for the same calendar name more
/// than once (a re-run, a double-submit, an agent retry) converges on one row
/// instead of minting differently-keyed duplicate calendars.
pub(crate) async fn get_or_create_local_calendar_by_name(
    store: &Store,
    workspace_id: WorkspaceId,
    name: &str,
) -> std::result::Result<Calendar, String> {
    let name = name.trim();
    let existing = store
        .calendars()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| format!("listing calendars: {e}"))?;
    if let Some(cal) = existing
        .into_iter()
        .find(|c| c.is_local() && c.name.trim().eq_ignore_ascii_case(name))
    {
        return Ok(cal);
    }
    store
        .calendars()
        .upsert_local(workspace_id, &local_calendar_external_id(name), name)
        .await
        .map_err(|e| format!("creating local calendar `{name}`: {e}"))
}

/// Resolve a `WriteEvent` redirect target to a writable **local** calendar,
/// accepting either a calendar id (UUID) or a calendar **name**. A bare UUID is
/// looked up and must be local (a provider calendar is refused — its mirror is
/// overwritten by the next sync, SOUL §8). Any other string is a name: an
/// existing local calendar of that name (case-insensitive) is reused, else one
/// is created (get-or-create via [`local_calendar_external_id`]), so an
/// automation can target a calendar that does not exist yet.
async fn resolve_write_calendar(
    store: &Store,
    workspace_id: WorkspaceId,
    arg: &str,
) -> std::result::Result<Calendar, String> {
    // A well-formed UUID is always an id, never a name.
    if let Ok(id) = arg.parse::<CalendarId>() {
        let calendar = store
            .calendars()
            .get(workspace_id, id)
            .await
            .map_err(|e| format!("calendar {id} not found in this workspace: {e}"))?;
        if !calendar.is_local() {
            return Err(
                "`calendar_id` must name a local calendar — a provider calendar's mirror is \
                 overwritten by its next sync"
                    .to_string(),
            );
        }
        return Ok(calendar);
    }
    // A name: get-or-create a local calendar of that name, so a calendar the
    // user made via `create_calendar` (or a prior WriteEvent) isn't duplicated
    // by a differently-keyed twin.
    get_or_create_local_calendar_by_name(store, workspace_id, arg).await
}

/// Enforce a capability for a special-cased action that bypasses the registry's own
/// gate. `None` capabilities = a trusted/internal caller (no enforcement), matching
/// [`ToolRegistry::dispatch`].
fn cap_allows(ctx: &ToolContext, required: &Capability) -> bool {
    ctx.capabilities
        .as_ref()
        .is_none_or(|caps| caps.iter().any(|h| h.covers(required)))
}

#[async_trait]
impl ActionRunner for ToolActionRunner {
    async fn run(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        // The LlmAgent action runs the §7 agent loop, not a single tool dispatch.
        if action.kind == ActionKind::LlmAgent {
            return self
                .run_llm_agent(workspace_id, action, trigger, grant)
                .await;
        }
        // RunProfile runs a durable §19 agent profile (its own grant, ⊆ this
        // automation's), not a single tool dispatch.
        if action.kind == ActionKind::RunProfile {
            return self
                .run_profile_action(workspace_id, action, trigger, grant)
                .await;
        }
        // RunSkill runs a named workspace skill (§23) through the §7 agent loop — its
        // runbook seeds the prompt and its tools confine the loop — not a single tool
        // dispatch.
        if action.kind == ActionKind::RunSkill {
            return self
                .run_skill_action(workspace_id, action, trigger, grant)
                .await;
        }
        // Summarize is a one-shot LLM completion (no tools, no loop), so like
        // LlmAgent it needs the client, not a registry dispatch.
        if action.kind == ActionKind::Summarize {
            return self
                .run_summarize(workspace_id, action, trigger, grant)
                .await;
        }
        // CreateChatThread is a direct, atomic store write (conversation + first
        // assistant message), not a registry tool dispatch.
        if action.kind == ActionKind::CreateChatThread {
            return self
                .run_create_chat_thread(workspace_id, action, trigger, grant)
                .await;
        }
        // The collect-write actions (SOUL §10/§28) persist an item carried on the
        // firing trigger (an upstream CollectEmail/CollectCalendar) — a whole-item
        // store upsert + projection enqueue, not a templated tool dispatch — so they
        // read `trigger` directly rather than going through `tool_for`/`render_params`.
        match action.kind {
            ActionKind::WriteEmail => {
                return self
                    .run_write_email(workspace_id, action, trigger, grant)
                    .await
            }
            ActionKind::WriteEvent => {
                return self
                    .run_write_event(workspace_id, action, trigger, grant)
                    .await
            }
            ActionKind::LabelEmail => {
                return self
                    .run_label_email(workspace_id, action, trigger, grant)
                    .await
            }
            ActionKind::MarkEmailRead => {
                return self
                    .run_mark_email_read(workspace_id, action, trigger, grant)
                    .await
            }
            _ => {}
        }
        let Some(tool) = tool_for(action.kind) else {
            return ActionOutcome::failed(format!(
                "automation action `{:?}` has no tool runner yet",
                action.kind
            ));
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(e) => return ActionOutcome::failed(e),
        };
        // Resolve `{{ path }}` references in the action's params against the node's
        // runtime context (the firing trigger + upstream node outputs), so e.g. a
        // `terminal_write` node can use the `session_id` an upstream `open_terminal`
        // node produced: `{ "session_id": "{{ inputs.open.session_id }}" }`.
        let args = Value::Object(render_params(&action.params, trigger));
        match self.registry.dispatch(tool, args, &ctx).await {
            Ok(output) => ActionOutcome::succeeded(Some(output)),
            Err(e) => ActionOutcome::failed(e.to_string()),
        }
    }

    /// Archive a collected message's raw `.eml` + attachments as **objects** in the
    /// workspace's files store and link them onto the stored row (SOUL §9/§28/§29,
    /// the resolution of the §29 "attachments — bucket + link vs. inline" question).
    /// Bucket + link: bytes land in the store under `emails/<mailbox>/<uid>/…`, ride
    /// the §10 object-ingest pipeline for free, and the row keeps only references.
    /// Opt-in (a no-op with no store), best-effort (a store failure warns, never
    /// fails the already-committed write), and idempotent (a redelivery whose row is
    /// already archived is skipped). See [`ActionRunner::archive_email`].
    async fn archive_email(
        &self,
        workspace_id: WorkspaceId,
        mailbox_id: MailboxId,
        uid: &str,
        raw: Option<Vec<u8>>,
        attachments: Vec<ExtractedAttachment>,
    ) {
        // Opt-in: no store configured → nothing to archive (matches chat uploads).
        let (Some(storage), Some(store)) = (self.storage.clone(), self.store.clone()) else {
            return;
        };
        // JMAP (and any provider that doesn't surface raw bytes) reaches here with
        // nothing to write — a clean no-op (deferred: JMAP blob download).
        if raw.is_none() && attachments.is_empty() {
            return;
        }
        // The write must already have landed the row (the collect pipeline calls this
        // only after a committed WriteEmail). An absent row means the write was
        // skipped/failed — nothing to archive; a row that is **already** archived is a
        // redelivery/re-collect → skip (idempotent; the objects are content-addressed
        // by key anyway, so a re-write would be harmless — this just avoids the work).
        let email = match store
            .emails()
            .get_by_uid(workspace_id, mailbox_id, uid)
            .await
        {
            Ok(e) => e,
            Err(catalerum_store::StoreError::NotFound) => return,
            Err(e) => {
                tracing::warn!(error = %e, uid, "archive_email: failed to load stored email");
                return;
            }
        };
        if email.raw_ref.is_some() || !email.attachments.is_empty() {
            return;
        }
        // Archival writes objects under the runner's default authority (the workspace
        // owner, base-Member — which holds `storage:write`). Skip-with-warn rather
        // than fail if a restricted authority somehow doesn't cover it: the email row
        // is the source of truth, archival is derived.
        let ctx = match self.resolve_context(workspace_id, None).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, uid, "archive_email: could not resolve authority");
                return;
            }
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("storage")),
        ) {
            tracing::warn!(
                uid,
                "archive_email: authority lacks storage:write; skipping archival"
            );
            return;
        }
        // The workspace owner's per-user default files store is the sink (unnamed →
        // resolved like a chat upload). Objects land under a collision-free,
        // path-traversal-safe key prefix keyed by the stable `(mailbox, uid)`.
        let owner = ctx.user_id;
        let prefix = format!("emails/{}/{}", mailbox_id, safe_segment(uid));

        // Raw `.eml` first: a NotFound here means no files store is configured for
        // this workspace at all → skip cleanly and leave the refs unset so a later
        // re-collect can heal (never fail the committed write).
        let mut raw_ref: Option<String> = None;
        if let Some(bytes) = raw {
            let key = format!("{prefix}/raw.eml");
            match crate::routes::storage::write_object_bytes(
                &storage,
                &store,
                workspace_id,
                owner,
                (None, &key),
                bytes,
                Some("message/rfc822".to_string()),
            )
            .await
            {
                Ok(_) => raw_ref = Some(key),
                Err(crate::error::ApiError::NotFound) => {
                    tracing::debug!(
                        uid,
                        "archive_email: no default files store; skipping archival"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, uid, "archive_email: failed to archive raw .eml");
                }
            }
        }

        // Each attachment → its own object (so it is separately downloadable via the
        // §9 `download_link` tool and separately §10-indexed by content type).
        let mut refs: Vec<Attachment> = Vec::new();
        for (n, att) in attachments.into_iter().enumerate() {
            let name = att
                .filename
                .as_deref()
                .map(safe_segment)
                .filter(|s| s != "store")
                .unwrap_or_else(|| format!("attachment-{n}"));
            let key = format!("{prefix}/attachments/{n}-{name}");
            let size = att.data.len() as u64;
            match crate::routes::storage::write_object_bytes(
                &storage,
                &store,
                workspace_id,
                owner,
                (None, &key),
                att.data,
                att.content_type.clone(),
            )
            .await
            {
                Ok(_) => refs.push(Attachment {
                    url: format!("/storage/objects/{key}"),
                    filename: att.filename,
                    content_type: att.content_type,
                    size: Some(size),
                }),
                Err(e) => {
                    tracing::warn!(error = %e, uid, %key, "archive_email: failed to archive attachment");
                }
            }
        }

        // Link the archived objects onto the row (kept out of the provider upsert so a
        // flag-only re-sync never clobbers them — like `set_raw_ref`/`set_labels`).
        if let Some(rref) = &raw_ref {
            if let Err(e) = store
                .emails()
                .set_raw_ref(workspace_id, email.id, rref)
                .await
            {
                tracing::warn!(error = %e, uid, "archive_email: failed to record raw_ref");
            }
        }
        if !refs.is_empty() {
            if let Err(e) = store
                .emails()
                .set_attachments(workspace_id, email.id, &refs)
                .await
            {
                tracing::warn!(error = %e, uid, "archive_email: failed to record attachment refs");
            }
        }
        tracing::debug!(
            uid,
            archived_raw = raw_ref.is_some(),
            attachments = refs.len(),
            "archive_email: archived collected message to object storage"
        );
    }

    /// Delete the archived objects a prior [`archive_email`] wrote, keyed by their
    /// object `keys` (SOUL §9/§28) — the deletion twin, invoked by the collect
    /// deletion reconcile. Resolves the same default files store and runs each key
    /// through the storage route's object-delete core (blob delete + catalogue
    /// removal + §10 de-index + `deleted` trigger). Opt-in + best-effort + idempotent.
    async fn cleanup_email_archive(&self, workspace_id: WorkspaceId, keys: Vec<String>) {
        if keys.is_empty() {
            return;
        }
        let (Some(storage), Some(store)) = (self.storage.clone(), self.store.clone()) else {
            return;
        };
        // Resolve the workspace owner's default files store (the sink archival used).
        let owner = workspace_owner(&store, workspace_id).await.ok();
        let handle = match crate::routes::storage::resolve_store(
            &storage,
            &store,
            workspace_id,
            owner,
            None,
        )
        .await
        {
            Ok(h) => h,
            Err(crate::error::ApiError::NotFound) => return,
            Err(e) => {
                tracing::warn!(error = %e, "cleanup_email_archive: could not resolve store");
                return;
            }
        };
        for key in keys {
            if let Err(e) =
                crate::routes::storage::delete_object_at(&store, &handle, workspace_id, &key).await
            {
                tracing::warn!(error = %e, %key, "cleanup_email_archive: failed to delete archived object");
            }
        }
    }
}

/// Sanitize one path segment (a uid or attachment filename) into a collision-free,
/// path-traversal-safe object-key component (SOUL §9/§18): keep `[A-Za-z0-9._-]`,
/// replace anything else with `_`, and collapse an empty / all-dots result (`.`,
/// `..`) to a safe literal so a crafted uid/filename can never escape the
/// `emails/<mailbox>/<uid>/…` prefix. Mirrors the backup crate's `safe_segment`.
fn safe_segment(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.bytes().all(|b| b == b'.') {
        "store".to_string()
    } else {
        s
    }
}

/// Substitute `{{ path }}` references in an action's `params` with values from the
/// node's runtime `ctx` (in a graph run, the merged `{ "trigger": …, "inputs":
/// { <upstream_id>: <output> } }`; in the linear loop, the firing trigger itself).
///
/// A param string that is **exactly** a single `{{ path }}` is replaced with the
/// resolved JSON value, *preserving its type* (so a number stays a number); a
/// `{{ path }}` embedded in surrounding text is stringified and interpolated. An
/// **unresolved** path is left verbatim — so an automation that uses no templating,
/// or contains a literal `{{`, is unchanged, and a typo surfaces as a clear
/// downstream error (e.g. "unknown terminal session") rather than silent mangling.
/// Recurses into nested objects/arrays. Pure (no I/O) so it is unit-tested below.
fn render_params(
    params: &serde_json::Map<String, Value>,
    ctx: Option<&Value>,
) -> serde_json::Map<String, Value> {
    let Some(ctx) = ctx else {
        return params.clone();
    };
    params
        .iter()
        .map(|(k, v)| (k.clone(), render_value(v, ctx)))
        .collect()
}

/// Recursively render template references in one JSON value (see [`render_params`]).
fn render_value(v: &Value, ctx: &Value) -> Value {
    match v {
        Value::String(s) => render_string(s, ctx),
        Value::Array(a) => Value::Array(a.iter().map(|x| render_value(x, ctx)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), render_value(x, ctx)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Render template references in a single string (see [`render_params`]). A
/// whole-string `{{ path }}` yields the resolved value verbatim (type preserved);
/// otherwise every `{{ path }}` token is stringified and interpolated in place.
fn render_string(s: &str, ctx: &Value) -> Value {
    let trimmed = s.trim();
    // Whole-string template → return the resolved value with its JSON type intact.
    if let Some(path) = trimmed
        .strip_prefix("{{")
        .and_then(|r| r.strip_suffix("}}"))
    {
        if let Some(found) = resolve_path(ctx, path.trim()) {
            return found.clone();
        }
        return Value::String(s.to_string());
    }
    // Embedded templates → string interpolation (resolved values stringified).
    if !s.contains("{{") {
        return Value::String(s.to_string());
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let path = after[..end].trim();
                match resolve_path(ctx, path) {
                    Some(found) => out.push_str(&stringify(found)),
                    // Unresolved → leave the token verbatim.
                    None => {
                        out.push_str("{{");
                        out.push_str(&after[..end]);
                        out.push_str("}}");
                    }
                }
                rest = &after[end + 2..];
            }
            // Unterminated `{{` → emit the rest literally and stop.
            None => {
                out.push_str("{{");
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// Resolve a dotted `path` (e.g. `inputs.open.session_id`, `trigger.kind`,
/// `inputs.list.0`) against `ctx`: object keys traverse maps, all-digit segments
/// index arrays. `None` if any segment is missing or the type doesn't match.
fn resolve_path<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = ctx;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(map) => map.get(seg)?,
            Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Stringify a resolved value for **interpolation** into surrounding text: a JSON
/// string contributes its raw text (no quotes); anything else its compact JSON.
fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{build_registry, NoteIngest};
    use catalerum_automation::{execute, FailCodeRunner};
    use catalerum_core::model::{Author, Role, RunStatus, StepStatus};
    use catalerum_store::{NewAutomation, Store};
    use serde_json::json;

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    fn automation(name: &str, actions: Vec<Value>) -> NewAutomation {
        NewAutomation {
            name: name.to_string(),
            enabled: true,
            triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
            condition: None,
            actions,
            spec: None,
            grant_id: None,
        }
    }

    /// A trivial ungated tool that echoes its args — for the code-node host test.
    struct EchoTool;

    #[async_trait]
    impl catalerum_core::tool::Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn invoke(&self, args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
            Ok(args)
        }
    }

    /// A tool requiring `notes:write` — exercises the capability gate.
    struct GatedTool;

    #[async_trait]
    impl catalerum_core::tool::Tool for GatedTool {
        fn name(&self) -> &str {
            "gated"
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn required_capability(&self) -> Option<Capability> {
            Some(Capability::new(CapAction::Write, Resource::domain("notes")))
        }
        async fn invoke(&self, _args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
            Ok(json!({ "ok": true }))
        }
    }

    /// `ToolActionRunner` as a [`CodeToolHost`] dispatches an allowed tool, rejects
    /// an unknown one, and enforces the run's capability cap — the SAME deny-by-default
    /// gate an action node uses, so a code node's `callTool` can do no more than the
    /// automation's authority permits. DB-free: with no store the authority is the
    /// explicitly-set cap set. Each call runs on a blocking thread (as a real code
    /// node does) so `call_tool`'s internal `block_on` is valid.
    #[tokio::test]
    async fn code_tool_host_dispatches_under_authority() {
        use std::sync::Arc;

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(EchoTool));
        registry.register(Arc::new(GatedTool));
        // No store + a bounded cap set (here: empty, so only ungated tools dispatch).
        let runner = ToolActionRunner::new(registry).with_capabilities(Vec::new());
        let ws = WorkspaceId::new();

        // An ungated tool dispatches and its result flows back to the script.
        let echoed = {
            let runner = runner.clone();
            tokio::task::spawn_blocking(move || {
                runner.call_tool(ws, None, "echo", json!({ "x": 1 }))
            })
            .await
            .unwrap()
        };
        assert_eq!(echoed.unwrap(), json!({ "x": 1 }));

        // An unknown tool is rejected before any dispatch.
        let unknown = {
            let runner = runner.clone();
            tokio::task::spawn_blocking(move || runner.call_tool(ws, None, "nope", json!({})))
                .await
                .unwrap()
        };
        assert!(unknown.unwrap_err().contains("unknown tool"));

        // A tool needing a capability beyond the run's authority is denied.
        let denied = {
            let runner = runner.clone();
            tokio::task::spawn_blocking(move || runner.call_tool(ws, None, "gated", json!({})))
                .await
                .unwrap()
        };
        assert!(
            denied.is_err(),
            "gated tool must be denied under empty caps"
        );
    }

    #[test]
    fn tool_mapping_covers_the_tool_backed_kinds_only() {
        assert_eq!(tool_for(ActionKind::CreateNote), Some("create_note"));
        assert_eq!(tool_for(ActionKind::MoveTask), Some("kanban_move_task"));
        assert_eq!(tool_for(ActionKind::CreateEvent), Some("create_event"));
        assert_eq!(tool_for(ActionKind::UpdateEvent), Some("update_event"));
        assert_eq!(tool_for(ActionKind::RunCommand), Some("run_command"));
        assert_eq!(tool_for(ActionKind::OpenTerminal), Some("open_terminal"));
        assert_eq!(tool_for(ActionKind::TerminalWrite), Some("terminal_write"));
        assert_eq!(tool_for(ActionKind::TerminalRead), Some("terminal_read"));
        assert_eq!(
            tool_for(ActionKind::PersistTerminal),
            Some("persist_terminal")
        );
        assert_eq!(tool_for(ActionKind::CloseTerminal), Some("close_terminal"));
        assert_eq!(tool_for(ActionKind::Notify), Some("notify"));
        assert_eq!(tool_for(ActionKind::IndexDocument), Some("index_document"));
        // The object + webhook actions map to their registry tools (SOUL §9/§11/§27).
        assert_eq!(tool_for(ActionKind::WriteObject), Some("write_object"));
        assert_eq!(tool_for(ActionKind::MoveObject), Some("move_object"));
        assert_eq!(tool_for(ActionKind::Webhook), Some("send_webhook"));
        // The LLM kinds are special-cased before `tool_for` (they run the client,
        // not a single tool dispatch), so they have no tool mapping.
        assert_eq!(tool_for(ActionKind::LlmAgent), None);
        assert_eq!(tool_for(ActionKind::Summarize), None);
        assert_eq!(tool_for(ActionKind::CreateChatThread), None);
        // The collect-write actions are special-cased before `tool_for` (they read
        // the whole trigger item + do a store upsert), so they have no tool mapping.
        assert_eq!(tool_for(ActionKind::WriteEmail), None);
        assert_eq!(tool_for(ActionKind::WriteEvent), None);
        assert_eq!(tool_for(ActionKind::LabelEmail), None);
        assert_eq!(tool_for(ActionKind::MarkEmailRead), None);
    }

    /// A CreateChatThread output node templates an upstream result, atomically
    /// creates a visible automation-origin conversation with one assistant
    /// message, and returns ids for downstream nodes. The direct store path still
    /// enforces the automation's `conversation:write` authority.
    #[tokio::test]
    async fn create_chat_thread_publishes_automation_output_and_checks_authority() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping create_chat_thread test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create(
                "chat-output",
                &format!("chat-output-{}", uuid::Uuid::new_v4()),
            )
            .await
            .expect("workspace");
        let owner = store
            .users()
            .create(
                &format!("chat-output-{}@t.test", uuid::Uuid::new_v4()),
                "Owner",
                None,
            )
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");

        let action: Action = serde_json::from_value(json!({
            "kind": "create_chat_thread",
            "title": "Daily {{ inputs.report.day }}",
            "message": "{{ inputs.report.text }}"
        }))
        .unwrap();
        let run_context = json!({
            "trigger": null,
            "inputs": {
                "report": { "day": "Tuesday", "text": "Everything is green." }
            }
        });
        let runner =
            ToolActionRunner::workspace_owner_authority(ToolRegistry::new(), store.clone());
        let outcome = runner.run(ws.id, &action, Some(&run_context), None).await;
        assert_eq!(outcome.status, StepStatus::Succeeded);
        let output = outcome.output.expect("output ids");

        let conversations = store
            .conversations()
            .list_by_workspace(ws.id)
            .await
            .unwrap();
        assert_eq!(conversations.len(), 1);
        let conversation = &conversations[0];
        assert_eq!(conversation.origin, Origin::Automation);
        assert_eq!(conversation.title.as_deref(), Some("Daily Tuesday"));
        assert_eq!(output["conversation_id"], json!(conversation.id));

        let messages = store
            .messages()
            .list_by_conversation(conversation.id)
            .await
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::Assistant);
        assert_eq!(messages[0].content, "Everything is green.");
        assert_eq!(output["message_id"], json!(messages[0].id));

        let denied = runner
            .clone()
            .with_capabilities(Vec::new())
            .run(ws.id, &action, Some(&run_context), None)
            .await;
        assert_eq!(denied.status, StepStatus::Failed);
        assert!(denied
            .error
            .as_deref()
            .is_some_and(|error| error.contains("conversation:write")));
        assert_eq!(
            store
                .conversations()
                .list_by_workspace(ws.id)
                .await
                .unwrap()
                .len(),
            1,
            "denied output must not create another thread"
        );
    }

    /// End-to-end (SOUL §10/§11/§28): a Maildir with two messages, a graph
    /// automation `CollectEmail(conn) → WriteEmail` with `commit_on` on the write,
    /// run through [`catalerum_ingest::run_collect_email`] — one run fires per
    /// message, each `WriteEmail` upserts the message, the `commit_on` write
    /// `Succeeded` so both commit, and a second poll of the unchanged mailbox
    /// collects nothing (the cursor advanced over the committed prefix).
    #[tokio::test]
    async fn collect_email_fires_per_message_writes_them_and_commits_then_is_idempotent() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping collect_email e2e: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        use catalerum_core::model::ConnectionKind;
        use catalerum_ingest::{run_collect_email, AutomationContext, CollectPayload};
        use std::sync::Arc;

        // Use an ISOLATED throwaway db (own automations table), NOT the shared dev DB:
        // this test creates an *enabled* CollectEmail automation, and a dev server's
        // ScheduleWorker scans every workspace each tick — a leaked one would make it
        // enqueue a `collect_email` job forever (the "why is email still running" bug).
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("collect", &format!("collect-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // workspace_owner_authority resolves the owner's base-Member caps (which
        // include email:write), so the workspace needs an Owner.
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");

        // A temp Maildir with two unseen messages in new/.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("new")).unwrap();
        std::fs::create_dir_all(dir.path().join("cur")).unwrap();
        let msg = |from: &str, subject: &str, body: &str| {
            format!(
                "From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\nDate: Wed, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <{subject}@x>\r\n\r\n{body}\r\n"
            )
        };
        std::fs::write(
            dir.path().join("new/msg-1"),
            msg("Ada <ada@bank.com>", "stmt-one", "your statement"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("new/msg-2"),
            msg("Bob <bob@shop.com>", "receipt-two", "a refund"),
        )
        .unwrap();

        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Email,
                "maildir",
                None,
                Some(json!({ "provider": "maildir", "root": dir.path().to_str().unwrap(), "name": "INBOX" })),
            )
            .await
            .expect("conn");

        // Graph: CollectEmail(conn) → WriteEmail, cursor committed on the write. A
        // wide `backfill_window` so the fixture's dated messages aren't dropped by
        // the default first-poll grace (which collects only very-recent mail).
        let collect_trigger = json!({
            "kind": "collect_email",
            "connection": conn.id.to_string(),
            "commit_on": "w",
            "backfill_window": { "days": 3650 }
        });
        let spec = json!({ "graph": {
            "nodes": [
                { "id": "c", "kind": "trigger", "trigger": collect_trigger },
                { "id": "w", "kind": "action", "action": { "kind": "write_email" } }
            ],
            "edges": [ { "from": "c", "to": "w" } ]
        }});
        let automation = store
            .automations()
            .create(
                ws.id,
                &NewAutomation {
                    name: "collect-mail".into(),
                    enabled: true,
                    triggers: vec![collect_trigger.clone()],
                    condition: None,
                    actions: vec![],
                    spec: Some(spec),
                    grant_id: None,
                },
            )
            .await
            .expect("automation");

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner: Arc<dyn ActionRunner> = Arc::new(ToolActionRunner::workspace_owner_authority(
            registry,
            store.clone(),
        ));
        let ctx = AutomationContext::new(runner);
        let payload = CollectPayload::new(ws.id, automation.id, collect_trigger.clone());

        // First poll: one run per message, both written, both committed.
        let report = run_collect_email(&store, &ctx, ws.id, &payload)
            .await
            .expect("collect");
        assert_eq!(report.runs_fired, 2, "one run per new message");
        assert_eq!(
            report.committed, 2,
            "the commit_on WriteEmail succeeded for both"
        );
        assert_eq!(report.sources, 1);

        let stored = store.emails().list_by_workspace(ws.id, 50).await.unwrap();
        assert_eq!(stored.len(), 2, "WriteEmail persisted both messages");
        let subjects: std::collections::HashSet<&str> =
            stored.iter().map(|e| e.subject.as_str()).collect();
        assert!(subjects.contains("stmt-one") && subjects.contains("receipt-two"));

        // Second poll of the unchanged Maildir: nothing new (cursor advanced over the
        // committed prefix; the content-hash cursor short-circuits an unchanged snapshot).
        let again = run_collect_email(&store, &ctx, ws.id, &payload)
            .await
            .expect("collect again");
        assert_eq!(
            again.runs_fired, 0,
            "an unchanged mailbox collects nothing on re-poll"
        );
        assert_eq!(
            store
                .emails()
                .list_by_workspace(ws.id, 50)
                .await
                .unwrap()
                .len(),
            2,
            "no duplicate emails on re-poll"
        );

        // A THIRD message arrives → the snapshot changes and is re-emitted in FULL
        // (all 3 uids). Only the genuinely-new message fires a run — the two already
        // committed are skipped (the ledger survived the no-change poll; this is the
        // snapshot double-run regression the review caught).
        std::fs::write(
            dir.path().join("new/msg-3"),
            msg("Carol <carol@x.com>", "third-msg", "hi"),
        )
        .unwrap();
        let third = run_collect_email(&store, &ctx, ws.id, &payload)
            .await
            .expect("collect third");
        assert_eq!(
            third.runs_fired, 1,
            "only the new message re-fires, not the whole snapshot"
        );
        assert_eq!(third.committed, 1);
        assert_eq!(
            store
                .emails()
                .list_by_workspace(ws.id, 50)
                .await
                .unwrap()
                .len(),
            3,
            "the third message is now stored; the first two were not re-written"
        );
    }

    /// `WriteEmail` reports the `newly_written` redelivery signal (SOUL §11/§29): the
    /// first write of a `(mailbox_id, uid)` is `true`, and a **redelivery** (a second
    /// write of the same key — the crash-window replay) is `false`. That `false` is
    /// what the DAG executor latches to auto-skip a downstream `LlmAgent`/`LabelEmail`,
    /// so this test pins the durable signal at its source.
    #[tokio::test]
    async fn write_email_reports_newly_written_true_then_false_on_redelivery() {
        use catalerum_core::model::ConnectionKind;

        let Some(url) = db_url() else {
            eprintln!(
                "skipping newly_written test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("nw", &format!("nw-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");
        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Email,
                "maildir",
                None,
                Some(json!({ "provider": "maildir", "name": "INBOX" })),
            )
            .await
            .expect("conn");
        // A real mailbox row so the WriteEmail upsert's FK is satisfied.
        let mb = store
            .mailboxes()
            .upsert(ws.id, conn.id, "INBOX", "INBOX", false)
            .await
            .expect("mailbox");

        // A collected email carried on the firing trigger (graph form: trigger.item).
        let item = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "workspace_id": ws.id.to_string(),
            "mailbox_id": mb.id.to_string(),
            "uid": "u-redeliv-1",
            "subject": "your statement",
            "has_attachments": false
        });
        let trigger = json!({ "trigger": { "item": item } });
        let action: Action = serde_json::from_value(json!({ "kind": "write_email" })).unwrap();

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

        // First delivery → the row is inserted → newly_written = true.
        let first = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(
            first.status,
            StepStatus::Succeeded,
            "first write succeeds: {:?}",
            first.error
        );
        assert_eq!(
            first.output.as_ref().unwrap()["newly_written"],
            json!(true),
            "the first write of a (mailbox_id, uid) is newly written"
        );

        // Redelivery of the same item → the upsert finds the existing row → false.
        let again = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(again.status, StepStatus::Succeeded);
        assert_eq!(
            again.output.as_ref().unwrap()["newly_written"],
            json!(false),
            "a redelivery of an already-stored message reports newly_written=false"
        );
        // Idempotent: still exactly one stored email for that uid.
        assert_eq!(
            store
                .emails()
                .list_by_workspace(ws.id, 50)
                .await
                .unwrap()
                .len(),
            1,
            "the redelivery upsert did not duplicate the message"
        );
    }

    /// Archival (SOUL §9/§28/§29 — the §29 "attachments: bucket + link" resolution +
    /// the raw-`.eml` deferral): `archive_email` writes the raw message + each
    /// attachment as **objects** in the workspace's files store and links them onto
    /// the row; a redelivery re-archives nothing; and `cleanup_email_archive` (the
    /// deletion reconcile) removes the objects. Uses a local-FS store + live Postgres.
    #[tokio::test]
    async fn archive_email_writes_objects_links_refs_is_idempotent_and_cleans_up() {
        use catalerum_core::model::ConnectionKind;
        use catalerum_storage::LocalFsBackend;

        let Some(url) = db_url() else {
            eprintln!(
                "skipping archive_email test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("arch", &format!("arch-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");
        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Email,
                "maildir",
                None,
                Some(json!({ "provider": "maildir", "name": "INBOX" })),
            )
            .await
            .expect("conn");
        let mb = store
            .mailboxes()
            .upsert(ws.id, conn.id, "INBOX", "INBOX", false)
            .await
            .expect("mailbox");
        // A stored email row for archive_email to find + link onto.
        let mut email: Email = serde_json::from_value(json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "workspace_id": ws.id.to_string(),
            "mailbox_id": mb.id.to_string(),
            "uid": "u-arch-1",
            "subject": "with attachment",
            "has_attachments": true
        }))
        .unwrap();
        email.workspace_id = ws.id;
        let stored = store
            .emails()
            .upsert_by_uid(&email)
            .await
            .expect("upsert email");
        assert!(stored.raw_ref.is_none() && stored.attachments.is_empty());

        // A local-FS files store as the default, threaded onto the runner.
        let tmp = tempfile::tempdir().expect("tmp");
        let backend: Arc<dyn catalerum_core::provider::StorageBackend> =
            Arc::new(LocalFsBackend::new(tmp.path().to_path_buf()));
        let config_store = crate::state::ConfigStore {
            backend: backend.clone(),
            connection: "files".to_string(),
            bucket: "files".to_string(),
            kind: "local",
            namespaced: true,
            workspaces: Vec::new(),
        };
        let registry = crate::state::StorageRegistry::single_for_test("files", config_store);
        let tool_registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(tool_registry, store.clone())
            .with_storage(Arc::new(registry.clone()));

        // Archive: a raw .eml + one attachment (bytes passed directly, as the collect
        // pipeline would after MIME-extracting them).
        let raw = b"From: a@x.com\r\nSubject: with attachment\r\n\r\nbody\r\n".to_vec();
        let atts = vec![ExtractedAttachment {
            filename: Some("invoice.pdf".to_string()),
            content_type: Some("application/pdf".to_string()),
            data: b"%PDF-1.4 fake".to_vec(),
        }];
        runner
            .archive_email(ws.id, mb.id, "u-arch-1", Some(raw.clone()), atts)
            .await;

        // The row now links the archived objects, under the stable key prefix.
        let after = store.emails().get(ws.id, stored.id).await.expect("reload");
        assert_eq!(
            after.raw_ref.as_deref(),
            Some(&*format!("emails/{}/u-arch-1/raw.eml", mb.id)),
            "raw_ref points at the archived .eml key"
        );
        assert_eq!(after.attachments.len(), 1, "one attachment ref");
        let att_ref = &after.attachments[0];
        assert_eq!(
            att_ref.url,
            format!(
                "/storage/objects/emails/{}/u-arch-1/attachments/0-invoice.pdf",
                mb.id
            )
        );
        assert_eq!(att_ref.filename.as_deref(), Some("invoice.pdf"));
        assert_eq!(att_ref.content_type.as_deref(), Some("application/pdf"));

        // The blobs physically exist on the backend (namespaced under <ws>/).
        let handle = registry.get("files").unwrap().handle("files".to_string());
        let raw_phys = handle.physical_key(ws.id, after.raw_ref.as_deref().unwrap());
        let att_key = att_ref.url.strip_prefix("/storage/objects/").unwrap();
        let att_phys = handle.physical_key(ws.id, att_key);
        assert!(
            backend.stat(&raw_phys).await.is_ok(),
            "raw .eml blob exists"
        );
        assert!(
            backend.stat(&att_phys).await.is_ok(),
            "attachment blob exists"
        );

        // Redelivery: archiving the same message again is a no-op (row already
        // archived) — refs unchanged, no duplicate objects.
        runner
            .archive_email(ws.id, mb.id, "u-arch-1", Some(raw), vec![])
            .await;
        let after2 = store.emails().get(ws.id, stored.id).await.expect("reload2");
        assert_eq!(
            after2.raw_ref, after.raw_ref,
            "redelivery leaves raw_ref unchanged"
        );
        assert_eq!(
            after2.attachments.len(),
            1,
            "redelivery adds no attachment refs"
        );

        // Deletion reconcile: cleanup removes the archived blobs.
        let keys = vec![after.raw_ref.clone().unwrap(), att_key.to_string()];
        runner.cleanup_email_archive(ws.id, keys).await;
        assert!(
            backend.stat(&raw_phys).await.is_err(),
            "raw .eml blob deleted on cleanup"
        );
        assert!(
            backend.stat(&att_phys).await.is_err(),
            "attachment blob deleted on cleanup"
        );
    }

    /// `WriteEvent` reports the `newly_written` redelivery signal (SOUL §11/§29) — the
    /// calendar twin of [`write_email_reports_newly_written_true_then_false_on_redelivery`]:
    /// the first write of a `(calendar_id, uid)` is `true`, a redelivery (a second write
    /// of the same key) is `false`. That `false` is what the DAG executor latches to
    /// auto-skip a downstream `LlmAgent` in a `CollectCalendar → WriteEvent → LlmAgent`
    /// flow, so this pins the durable signal at its source (via `EventRepo::get_by_uid`).
    #[tokio::test]
    async fn write_event_reports_newly_written_true_then_false_on_redelivery() {
        use catalerum_core::model::ConnectionKind;

        let Some(url) = db_url() else {
            eprintln!("skipping write_event newly_written test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("nwe", &format!("nwe-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");
        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Calendar,
                "caldav",
                None,
                Some(json!({ "provider": "caldav" })),
            )
            .await
            .expect("conn");
        // A real calendar row so the WriteEvent upsert's FK is satisfied.
        let cal = store
            .calendars()
            .upsert(ws.id, conn.id, "ext-cal", "Work", false)
            .await
            .expect("calendar");

        // A collected event carried on the firing trigger (graph form: trigger.item).
        let now = chrono::Utc::now();
        let item = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "workspace_id": ws.id.to_string(),
            "calendar_id": cal.id.to_string(),
            "uid": "u-ev-redeliv-1",
            "start": now.to_rfc3339(),
            "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
            "summary": "Standup",
            "sequence": 0
        });
        let trigger = json!({ "trigger": { "item": item } });
        let action: Action = serde_json::from_value(json!({ "kind": "write_event" })).unwrap();

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

        // First delivery → the row is inserted → newly_written = true.
        let first = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(
            first.status,
            StepStatus::Succeeded,
            "first write succeeds: {:?}",
            first.error
        );
        assert_eq!(
            first.output.as_ref().unwrap()["newly_written"],
            json!(true),
            "the first write of a (calendar_id, uid) is newly written"
        );

        // Redelivery of the same item → the upsert finds the existing row → false.
        let again = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(again.status, StepStatus::Succeeded);
        assert_eq!(
            again.output.as_ref().unwrap()["newly_written"],
            json!(false),
            "a redelivery of an already-stored event reports newly_written=false"
        );
        // Idempotent: still exactly one stored event for that uid.
        let events = store
            .events()
            .list_by_workspace(
                ws.id,
                None,
                catalerum_store::DateRange {
                    from: None,
                    to: None,
                },
                50,
            )
            .await
            .unwrap();
        assert_eq!(
            events.len(),
            1,
            "the redelivery upsert did not duplicate the event"
        );
    }

    /// `WriteEvent`'s optional `calendar_id` param redirects the write into a
    /// specific **local** calendar (overriding the collected item's source
    /// calendar), and refuses a provider calendar — a row planted in a provider
    /// mirror would silently vanish on its next snapshot sync (SOUL §8).
    #[tokio::test]
    async fn write_event_calendar_id_param_redirects_into_a_local_calendar_only() {
        use catalerum_core::model::ConnectionKind;

        let Some(url) = db_url() else {
            eprintln!("skipping write_event calendar_id test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("wecal", &format!("wecal-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");
        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Calendar,
                "caldav",
                None,
                Some(json!({ "provider": "caldav" })),
            )
            .await
            .expect("conn");
        // The collect source's mirror (provider) calendar and the redirect target.
        let source = store
            .calendars()
            .upsert(ws.id, conn.id, "ext-cal", "Work", false)
            .await
            .expect("source calendar");
        let local = store
            .calendars()
            .upsert_local(ws.id, "redirect", "Planning")
            .await
            .expect("local calendar");

        let now = chrono::Utc::now();
        let item = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "workspace_id": ws.id.to_string(),
            "calendar_id": source.id.to_string(),
            "uid": "u-ev-redirect-1",
            "start": now.to_rfc3339(),
            "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
            "summary": "Planning sync",
            "sequence": 0
        });
        let trigger = json!({ "trigger": { "item": item } });

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

        // Redirect into the local calendar: the stored row lands there, not in
        // the source mirror.
        let action: Action = serde_json::from_value(json!({
            "kind": "write_event",
            "calendar_id": local.id.to_string()
        }))
        .unwrap();
        let out = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(
            out.status,
            StepStatus::Succeeded,
            "redirected write succeeds: {:?}",
            out.error
        );
        assert_eq!(
            out.output.as_ref().unwrap()["calendar_id"],
            json!(local.id.to_string()),
            "the event was written into the redirect target, not the source calendar"
        );

        // A provider calendar as the target is refused.
        let action: Action = serde_json::from_value(json!({
            "kind": "write_event",
            "calendar_id": source.id.to_string()
        }))
        .unwrap();
        let out = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(out.status, StepStatus::Failed);
        assert!(
            out.error
                .as_deref()
                .unwrap_or("")
                .contains("local calendar"),
            "the failure names the local-calendar restriction: {:?}",
            out.error
        );

        // A calendar id from another workspace (or a bogus one) is refused too.
        let action: Action = serde_json::from_value(json!({
            "kind": "write_event",
            "calendar_id": uuid::Uuid::new_v4().to_string()
        }))
        .unwrap();
        let out = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(out.status, StepStatus::Failed);
        assert!(
            out.error.as_deref().unwrap_or("").contains("not found"),
            "an unknown calendar id fails with not-found: {:?}",
            out.error
        );
    }

    /// `WriteEvent`'s `calendar` param names a **local** calendar to write into:
    /// an existing one of that name is reused, and a not-yet-existing name is
    /// created on the fly — so an automation can target e.g. "Feiertage Bayern"
    /// without a separate create_calendar step. Idempotent across runs.
    #[tokio::test]
    async fn write_event_calendar_name_reuses_or_creates_a_local_calendar() {
        use catalerum_core::model::ConnectionKind;

        let Some(url) = db_url() else {
            eprintln!("skipping write_event calendar-name test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("wecaln", &format!("wecaln-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");
        // The collect source's mirror (a provider calendar named "bavaria", like
        // the reported ICS-holidays case).
        let conn = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Calendar,
                "webcal",
                None,
                Some(json!({ "provider": "caldav" })),
            )
            .await
            .expect("conn");
        let source = store
            .calendars()
            .upsert(ws.id, conn.id, "ext-cal", "bavaria", true)
            .await
            .expect("source calendar");

        let now = chrono::Utc::now();
        let mk_item = |uid: &str| {
            json!({
                "id": uuid::Uuid::new_v4().to_string(),
                "workspace_id": ws.id.to_string(),
                "calendar_id": source.id.to_string(),
                "uid": uid,
                "start": now.to_rfc3339(),
                "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
                "summary": "Neujahr",
                "sequence": 0
            })
        };

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

        // First write to a name that does not exist yet → a local calendar of
        // that name is created and the event lands there, NOT in the source.
        let action: Action = serde_json::from_value(json!({
            "kind": "write_event",
            "calendar": "Feiertage Bayern"
        }))
        .unwrap();
        let trigger = json!({ "trigger": { "item": mk_item("u-fb-1") } });
        let out = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(
            out.status,
            StepStatus::Succeeded,
            "named write succeeds: {:?}",
            out.error
        );
        let created_id = out.output.as_ref().unwrap()["calendar_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(
            created_id,
            source.id.to_string(),
            "the event did not land in the collect source (\"bavaria\")"
        );
        let created: catalerum_core::CalendarId = created_id.parse().unwrap();
        let cal = store.calendars().get(ws.id, created).await.expect("cal");
        assert_eq!(cal.name, "Feiertage Bayern");
        assert!(cal.is_local(), "target is a local calendar");

        // A second write of a *different* event to the SAME name reuses that same
        // calendar (idempotent — no per-event duplicate calendars).
        let trigger = json!({ "trigger": { "item": mk_item("u-fb-2") } });
        let out = runner.run(ws.id, &action, Some(&trigger), None).await;
        assert_eq!(out.status, StepStatus::Succeeded);
        assert_eq!(
            out.output.as_ref().unwrap()["calendar_id"],
            json!(created_id),
            "the same name resolves to the same local calendar on re-run"
        );
        let locals = store
            .calendars()
            .list_by_workspace(ws.id)
            .await
            .expect("list")
            .into_iter()
            .filter(|c| c.is_local() && c.name == "Feiertage Bayern")
            .count();
        assert_eq!(locals, 1, "no duplicate \"Feiertage Bayern\" calendars");
    }

    /// FULL end-to-end (the reported bug): a `CollectCalendar` over a local
    /// `.ics` of **all-day** holidays (like the Bavaria feed) → a `WriteEvent`
    /// that names its destination calendar ("Feiertage Bayern") actually PERSISTS
    /// the events into a freshly-created local calendar — proving the whole
    /// collect→write chain writes (the earlier failure: WriteEvent redirected to a
    /// calendar id that wasn't a writable local one, so every write failed and
    /// nothing landed anywhere).
    #[tokio::test]
    async fn collect_calendar_all_day_writes_events_into_a_named_local_calendar() {
        use std::sync::Arc;

        use catalerum_automation::ActionRunner;
        use catalerum_core::model::ConnectionKind;
        use catalerum_ingest::{run_collect_calendar, AutomationContext, CollectPayload};

        // All-day (`VALUE=DATE`) events, far-future so they always clear the
        // first-poll 1-day backfill cutoff regardless of the test clock — the
        // exact shape of the officeholidays Bavaria feed.
        const ICS: &str = "BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//bavaria//EN\r
BEGIN:VEVENT\r
UID:assumption@bayern\r
DTSTART;VALUE=DATE:20990815\r
DTEND;VALUE=DATE:20990816\r
SUMMARY:Bavaria: Assumption Day\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:unity@bayern\r
DTSTART;VALUE=DATE:20991003\r
DTEND;VALUE=DATE:20991004\r
SUMMARY:Bavaria: German Unity Day\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:christmas@bayern\r
DTSTART;VALUE=DATE:20991225\r
DTEND;VALUE=DATE:20991226\r
SUMMARY:Bavaria: Christmas Day\r
END:VEVENT\r
END:VCALENDAR\r
";

        let Some(url) = db_url() else {
            eprintln!(
                "skipping collect->write e2e test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("bayern", &format!("bayern-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // The runner resolves the workspace owner's authority (base-Member caps
        // cover calendar:read/write), so an Owner membership is required.
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("membership");

        // A local `.ics` calendar connection over a temp dir holding the feed.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("bavaria.ics"), ICS).unwrap();
        let connection = store
            .connections()
            .create(
                ws.id,
                ConnectionKind::Calendar,
                "ics",
                None,
                Some(json!({ "provider": "local", "path": dir.path().to_string_lossy() })),
            )
            .await
            .expect("connection");
        let conn = connection.id.to_string();

        // The user's automation: collect the feed → write each event into a
        // calendar named "Feiertage Bayern" (which does NOT exist yet).
        let automation = store
            .automations()
            .create(
                ws.id,
                &NewAutomation {
                    name: "bayern-holidays".into(),
                    enabled: true,
                    triggers: vec![json!({ "kind": "collect_calendar", "connection": conn })],
                    condition: None,
                    actions: vec![json!({
                        "kind": "write_event",
                        "calendar": "Feiertage Bayern"
                    })],
                    spec: None,
                    grant_id: None,
                },
            )
            .await
            .expect("automation");

        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner: Arc<dyn ActionRunner> = Arc::new(ToolActionRunner::workspace_owner_authority(
            registry,
            store.clone(),
        ));
        let ctx = AutomationContext::new(runner);

        let payload = CollectPayload::new(ws.id, automation.id, automation.triggers[0].clone());
        let report = run_collect_calendar(&store, &ctx, ws.id, &payload)
            .await
            .expect("collect poll");
        assert_eq!(report.sources, 1, "the one .ics file is the one source");
        assert_eq!(report.runs_fired, 3, "one run fired per all-day event");

        // The destination calendar was auto-created, local, and holds all three
        // events — none of which leaked into the collect source ("bavaria.ics").
        let calendars = store.calendars().list_by_workspace(ws.id).await.unwrap();
        let target = calendars
            .iter()
            .find(|c| c.is_local() && c.name == "Feiertage Bayern")
            .expect("the named destination calendar was created");
        let source = calendars
            .iter()
            .find(|c| !c.is_local())
            .expect("the collect source calendar exists");
        for uid in ["assumption@bayern", "unity@bayern", "christmas@bayern"] {
            let ev = store
                .events()
                .get_by_uid(ws.id, target.id, uid)
                .await
                .unwrap_or_else(|e| panic!("event {uid} written to Feiertage Bayern: {e}"));
            assert!(ev.all_day, "the all-day flag survives the write");
            assert!(
                matches!(
                    store.events().get_by_uid(ws.id, source.id, uid).await,
                    Err(catalerum_store::StoreError::NotFound)
                ),
                "event {uid} must NOT have landed in the collect source calendar"
            );
        }
    }

    #[test]
    fn local_calendar_external_id_is_a_stable_prefixed_slug() {
        assert_eq!(
            local_calendar_external_id("Feiertage Bayern"),
            "named-feiertage-bayern"
        );
        // Same name (case/space-insensitive at the edges) → same key.
        assert_eq!(
            local_calendar_external_id("  Feiertage   Bayern  "),
            local_calendar_external_id("Feiertage   Bayern")
        );
        // Never collides with the reserved auto-default calendar key ("default").
        assert_ne!(local_calendar_external_id("default"), "default");
        // Punctuation-only / empty names still yield a valid non-empty key.
        assert_eq!(local_calendar_external_id("!!!"), "named-cal");
    }

    #[test]
    fn render_params_threads_upstream_outputs_into_action_params() {
        // The graph-run context: the firing trigger + an upstream `open` node's
        // output (the shape a `terminal_write` node sees).
        let ctx = json!({
            "trigger": { "kind": "webhook", "path": "/convert" },
            "inputs": {
                "open": { "session_id": "abc-123", "kind": "ephemeral" },
                "gen":  { "data": { "lines": 7 } }
            }
        });
        let params: serde_json::Map<String, Value> = json!({
            "session_id": "{{ inputs.open.session_id }}",
            "data": "python convert.py # {{ inputs.gen.data.lines }} lines\n",
            "count": "{{ inputs.gen.data.lines }}",
            "literal": "no templates here",
            "kept": "{{ inputs.missing.field }}"
        })
        .as_object()
        .unwrap()
        .clone();

        let out = render_params(&params, Some(&ctx));
        // Whole-string template → the resolved string, verbatim (no surrounding text).
        assert_eq!(out["session_id"], json!("abc-123"));
        // Whole-string template over a number → the JSON number, type preserved.
        assert_eq!(out["count"], json!(7));
        // Embedded template → stringified + interpolated into the surrounding text.
        assert_eq!(out["data"], json!("python convert.py # 7 lines\n"));
        // No template → unchanged.
        assert_eq!(out["literal"], json!("no templates here"));
        // Unresolved path → left verbatim (a clear downstream error, not silent loss).
        assert_eq!(out["kept"], json!("{{ inputs.missing.field }}"));
    }

    #[test]
    fn render_params_is_a_noop_without_context_and_resolves_arrays() {
        let params: serde_json::Map<String, Value> =
            json!({ "x": "{{ a }}" }).as_object().unwrap().clone();
        // No context → params returned unchanged (the linear/manual path).
        assert_eq!(render_params(&params, None)["x"], json!("{{ a }}"));

        // Array indexing + nested rendering inside an array param.
        let ctx = json!({ "inputs": { "n": { "items": ["alpha", "beta"] } } });
        let p: serde_json::Map<String, Value> =
            json!({ "argv": ["echo", "{{ inputs.n.items.1 }}"] })
                .as_object()
                .unwrap()
                .clone();
        let out = render_params(&p, Some(&ctx));
        assert_eq!(out["argv"], json!(["echo", "beta"]));
    }

    #[tokio::test]
    async fn automation_creates_a_note_through_the_registry_capability_enforced() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping tool-action-runner test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("autorun", &format!("autorun-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // embed/graph off → only Postgres needed for create_note.
        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let user = UserId::new();

        // 1. A Member-authority automation that creates a note → run Succeeded, and
        //    the note really lands in the workspace.
        let make_note = store
            .automations()
            .create(ws.id, &automation("note-bot", vec![json!({
                "kind": "create_note", "title": "From automation", "markdown": "hi", "tags": ["auto"]
            })]))
            .await
            .unwrap();
        let runner = ToolActionRunner::new(registry.clone())
            .as_user(user)
            .with_capabilities(catalerum_iam::base_capabilities(Role::Member));
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &make_note, None)
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        let notes = store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap();
        assert!(
            notes.iter().any(|n| n.title == "From automation"),
            "the automation created the note"
        );

        // 2. Viewer authority lacks Write@notes → the action is denied → run Failed.
        let blocked = store
            .automations()
            .create(
                ws.id,
                &automation(
                    "note-bot-viewer",
                    vec![json!({
                        "kind": "create_note", "title": "blocked"
                    })],
                ),
            )
            .await
            .unwrap();
        let viewer = ToolActionRunner::new(registry.clone())
            .as_user(user)
            .with_capabilities(catalerum_iam::base_capabilities(Role::Viewer));
        let run = execute(&store, &viewer, &FailCodeRunner, ws.id, &blocked, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "deny-by-default applies to automations too"
        );
        let steps = store
            .automation_runs()
            .list_steps(ws.id, run.id)
            .await
            .unwrap();
        assert_eq!(steps[0].status, StepStatus::Failed);
        // The note was NOT created.
        assert!(!store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap()
            .iter()
            .any(|n| n.title == "blocked"));

        // 3. An LLM-backed action (`summarize`) on a runner with no LLM client →
        //    run Failed with a clear message. (`llm_agent`/`run_skill` behave the
        //    same; every tool-less kind now has a real runner, so "no tool runner
        //    yet" is gone — a missing dependency is what fails.)
        let unsupported = store
            .automations()
            .create(
                ws.id,
                &automation(
                    "summary-bot",
                    vec![json!({ "kind": "summarize", "input": "text" })],
                ),
            )
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &unsupported, None)
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        let steps = store
            .automation_runs()
            .list_steps(ws.id, run.id)
            .await
            .unwrap();
        assert!(steps[0]
            .error
            .as_deref()
            .unwrap()
            .contains("needs an LLM client"));
    }

    /// A canned executor so `run_command` is registered (it needs `exec:run`).
    struct FakeExec;
    #[async_trait]
    impl catalerum_core::provider::Executor for FakeExec {
        async fn run(
            &self,
            cmd: catalerum_core::provider::CommandSpec,
        ) -> catalerum_core::error::Result<catalerum_core::provider::CommandResult> {
            Ok(catalerum_core::provider::CommandResult {
                exit_code: 0,
                stdout: cmd.argv.join(" "),
                stderr: String::new(),
                timed_out: false,
            })
        }
        async fn open_session(
            &self,
            _spec: catalerum_core::provider::SessionSpec,
        ) -> catalerum_core::error::Result<catalerum_core::provider::Session> {
            Err(catalerum_core::error::Error::Unsupported(
                "no sessions".into(),
            ))
        }
    }

    /// §19 enforcement: an automation **without** a grant runs under base-Member and
    /// is denied a protected action (`run_command` needs `exec:run`, which no base
    /// role holds); **with** a grant conferring `exec:run`, the same action runs.
    #[tokio::test]
    async fn grant_elevates_an_automation_above_its_base_authority() {
        use catalerum_core::capability::{Action, Capability, Constraints, Resource};
        use std::sync::Arc;

        let Some(url) = db_url() else {
            eprintln!(
                "skipping grant-enforcement test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("grantrun", &format!("grantrun-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // Registry WITH an executor → `run_command` is registered.
        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            Some(Arc::new(FakeExec)),
            Vec::new(),
            None,
            None,
        );
        let user = UserId::new();
        // Default authority is base **Member**, which does NOT hold `exec:run`.
        let runner = ToolActionRunner::new(registry)
            .as_user(user)
            .with_capabilities(catalerum_iam::base_capabilities(Role::Member));

        let cmd = json!({ "kind": "run_command", "command": ["echo", "granted"] });

        // 1. No grant → run_command denied (deny-by-default) → run Failed.
        let ungranted = store
            .automations()
            .create(ws.id, &automation("no-grant", vec![cmd.clone()]))
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &ungranted, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "base Member lacks exec:run → denied"
        );
        assert_eq!(
            run.grant_id, None,
            "an ungranted run records no audit grant"
        );

        // 2. A grant conferring exec:run, attached to the automation → the SAME action
        //    now runs → run Succeeded. The grant elevated the automation's authority.
        let grant = store
            .grants()
            .upsert(
                ws.id,
                "exec-powers",
                &[Capability::new(Action::Run, Resource::domain("exec"))],
                &Default::default(),
            )
            .await
            .unwrap();
        let granted = store
            .automations()
            .create(
                ws.id,
                &catalerum_store::NewAutomation {
                    name: "with-grant".into(),
                    enabled: true,
                    triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                    condition: None,
                    actions: vec![cmd],
                    spec: None,
                    grant_id: Some(grant.id),
                },
            )
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &granted, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Succeeded,
            "the grant conferred exec:run → the action ran"
        );
        assert_eq!(
            run.grant_id,
            Some(grant.id),
            "the production run path snapshots the authorizing grant for audit (§19)"
        );

        // 3. A `dry_run` grant: the action is AUTHORIZED then SIMULATED at dispatch —
        //    the run Succeeds but the side effect is NOT committed (no note created).
        let dry = store
            .grants()
            .upsert(
                ws.id,
                "dry-run-powers",
                &[Capability::new(Action::Write, Resource::domain("notes"))],
                &Constraints {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let auto3 = store
            .automations()
            .create(
                ws.id,
                &catalerum_store::NewAutomation {
                    name: "dry".into(),
                    enabled: true,
                    triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                    condition: None,
                    actions: vec![json!({
                        "kind": "create_note", "title": "DRYRUN-MARKER", "markdown": "x"
                    })],
                    spec: None,
                    grant_id: Some(dry.id),
                },
            )
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto3, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Succeeded,
            "dry-run authorizes + simulates → run succeeds"
        );
        let notes = store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap();
        assert!(
            !notes.iter().any(|n| n.title == "DRYRUN-MARKER"),
            "dry-run must NOT commit the side effect — no note created"
        );

        // 4. A grant with a constraint the runtime *still* can't enforce (`rate_limit`)
        //    → fails closed, never running with that guardrail silently dropped.
        let capped = store
            .grants()
            .upsert(
                ws.id,
                "rate-capped",
                &[Capability::new(Action::Write, Resource::domain("notes"))],
                &Constraints {
                    rate_limit: Some(5),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let auto4 = store
            .automations()
            .create(
                ws.id,
                &catalerum_store::NewAutomation {
                    name: "rate-limited".into(),
                    enabled: true,
                    triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                    condition: None,
                    actions: vec![json!({ "kind": "create_note", "title": "rate-limited" })],
                    spec: None,
                    grant_id: Some(capped.id),
                },
            )
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto4, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Failed,
            "an unenforced constraint (rate_limit) fails closed"
        );

        // 5. A `cost_limit` grant is now ENFORCED (the agent loop caps per-run spend,
        //    §7/§19), so it no longer fails closed: a non-LLM action under it has zero
        //    spend → the cap is trivially satisfied → the run succeeds and commits.
        let cost_capped = store
            .grants()
            .upsert(
                ws.id,
                "cost-capped",
                &[Capability::new(Action::Write, Resource::domain("notes"))],
                &Constraints {
                    cost_limit: Some(1.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let auto5 = store
            .automations()
            .create(
                ws.id,
                &catalerum_store::NewAutomation {
                    name: "cost-capped".into(),
                    enabled: true,
                    triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                    condition: None,
                    actions: vec![json!({
                        "kind": "create_note", "title": "COST-OK-MARKER", "markdown": "x"
                    })],
                    spec: None,
                    grant_id: Some(cost_capped.id),
                },
            )
            .await
            .unwrap();
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto5, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Succeeded,
            "a cost_limit grant is enforced, not failed-closed → a zero-spend action commits"
        );
        let notes = store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap();
        assert!(
            notes.iter().any(|n| n.title == "COST-OK-MARKER"),
            "the cost-capped run must actually commit its side effect (note created)"
        );
    }

    #[test]
    fn role_rank_orders_by_privilege() {
        assert!(role_rank(Role::Owner) < role_rank(Role::Admin));
        assert!(role_rank(Role::Admin) < role_rank(Role::Member));
        assert!(role_rank(Role::Member) < role_rank(Role::Viewer));
    }

    #[tokio::test]
    async fn workspace_owner_authority_acts_as_the_workspace_owner() {
        let Some(url) = db_url() else {
            eprintln!("skipping workspace-owner-authority test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let ws = store
            .workspaces()
            .create("ownerauth", &format!("ownerauth-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // Two members: a Member (created first) and the Owner (created later).
        // The runner must pick the *Owner* as the acting identity regardless of
        // creation order — proving rank, not just "first member".
        let member = store
            .users()
            .create(
                &format!("m-{}@t.test", uuid::Uuid::new_v4()),
                "Member",
                None,
            )
            .await
            .expect("member user");
        store
            .memberships()
            .upsert(ws.id, member.id, Role::Member)
            .await
            .expect("member");
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner user");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("owner");

        // embed/graph off → only Postgres needed for create_note.
        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

        // An automation that creates a note — no explicit identity/authority is
        // pinned, so the runner resolves the workspace owner + base-Member caps.
        let auto = store
            .automations()
            .create(
                ws.id,
                &automation(
                    "owner-bot",
                    vec![json!({
                        "kind": "create_note", "title": "by the owner", "markdown": "hi"
                    })],
                ),
            )
            .await
            .expect("automation");
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto, None)
            .await
            .unwrap();
        assert_eq!(
            run.status,
            RunStatus::Succeeded,
            "Member caps cover create_note"
        );
        let notes = store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap();
        let note = notes
            .iter()
            .find(|n| n.title == "by the owner")
            .expect("note created");
        assert_eq!(
            note.author,
            Author::User { id: owner.id },
            "the automation acted as the resolved workspace owner, not the lower-ranked member"
        );
    }

    #[tokio::test]
    async fn workspace_owner_authority_fails_when_the_workspace_has_no_members() {
        let Some(url) = db_url() else {
            eprintln!("skipping no-members test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        // A workspace with no memberships → no identity to act as → the action
        // fails cleanly (a `Failed` run), never an unauthenticated side effect.
        let ws = store
            .workspaces()
            .create("nomembers", &format!("nomembers-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());
        let auto = store
            .automations()
            .create(
                ws.id,
                &automation(
                    "orphan-bot",
                    vec![json!({
                        "kind": "create_note", "title": "should not exist"
                    })],
                ),
            )
            .await
            .expect("automation");
        let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto, None)
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        let steps = store
            .automation_runs()
            .list_steps(ws.id, run.id)
            .await
            .unwrap();
        assert!(steps[0].error.as_deref().unwrap().contains("no members"));
        assert!(store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn agent_request_defaults_model_and_seeds_system_plus_user_nudge() {
        use catalerum_core::model::MessageRole;
        // No model / tools pinned, no trigger → default model, the given system,
        // the bare nudge as the user turn, all tools advertised (None).
        let (req, allowed) = agent_request(
            &LlmAgent::default(),
            "echo",
            DEFAULT_AGENT_SYSTEM.to_string(),
            None,
        );
        assert_eq!(req.model, "echo");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, MessageRole::System);
        assert_eq!(req.messages[0].content, DEFAULT_AGENT_SYSTEM);
        assert_eq!(req.messages[1].role, MessageRole::User);
        assert_eq!(req.messages[1].content, AGENT_TRIGGER_PROMPT);
        assert!(
            allowed.is_none(),
            "empty tools → advertise the whole registry"
        );
    }

    #[test]
    fn agent_request_honors_pinned_model_and_tool_subset() {
        let agent = LlmAgent {
            system: None,
            model: Some("gpt-x".into()),
            tools: vec!["create_note".into(), "recall".into()],
            skills: vec![],
            output: None,
            reasoning_effort: None,
        };
        let (req, allowed) = agent_request(&agent, "echo", "be terse".to_string(), None);
        assert_eq!(
            req.model, "gpt-x",
            "the action's model wins over the default"
        );
        assert_eq!(req.messages[0].content, "be terse");
        assert_eq!(
            allowed.as_deref(),
            Some(&["create_note".to_string(), "recall".to_string()][..])
        );
    }

    #[test]
    fn trigger_prompt_describes_the_firing_event() {
        // No trigger → the bare nudge.
        assert_eq!(trigger_prompt(None), AGENT_TRIGGER_PROMPT);
        // A trigger → its JSON is embedded so the agent knows what fired it.
        let p = trigger_prompt(Some(&json!({ "kind": "webhook", "path": "/deploy-done" })));
        assert!(p.contains("webhook") && p.contains("/deploy-done"));
        assert!(p.contains(AGENT_TRIGGER_PROMPT));
    }

    #[test]
    fn agent_outcome_empty_output_is_a_failed_step() {
        use catalerum_core::model::{StepStatus, ToolCall};
        use catalerum_llm::ToolInvocation;

        // Neither text nor tool calls → Failed (surfaces a no-op / broken-LLM turn,
        // rather than silently succeeding with empty output).
        let empty = AgentOutcome {
            content: "  ".into(),
            iterations: 1,
            ..Default::default()
        };
        let out = agent_outcome_to_action(&empty, None);
        assert_eq!(out.status, StepStatus::Failed);
        assert!(out.error.as_deref().unwrap().contains("no output"));

        // Non-empty text → Succeeded, carrying the content; no `data` without it.
        let answered = AgentOutcome {
            content: "here you go".into(),
            iterations: 1,
            ..Default::default()
        };
        let out = agent_outcome_to_action(&answered, None);
        assert_eq!(out.status, StepStatus::Succeeded);
        assert_eq!(
            out.output.as_ref().unwrap()["content"],
            json!("here you go")
        );
        assert!(out.output.as_ref().unwrap().get("data").is_none());

        // A JSON-steered agent attaches its parsed reply as `data` for downstream use.
        let out = agent_outcome_to_action(&answered, Some(json!({ "yes": 0.8, "no": 0.2 })));
        assert_eq!(
            out.output.as_ref().unwrap()["data"],
            json!({ "yes": 0.8, "no": 0.2 })
        );

        // No text but a tool ran → Succeeded (the agent did real work via tools).
        let acted = AgentOutcome {
            content: String::new(),
            iterations: 2,
            tool_invocations: vec![ToolInvocation {
                call: ToolCall {
                    id: "1".into(),
                    name: "create_note".into(),
                    arguments: "{}".into(),
                },
                result: "{}".into(),
                is_error: false,
                duration_ms: 0,
                media: Vec::new(),
            }],
            ..Default::default()
        };
        let out = agent_outcome_to_action(&acted, None);
        assert_eq!(out.status, StepStatus::Succeeded);
        assert_eq!(out.output.as_ref().unwrap()["tool_calls"], json!(1));

        // Cost + truncation status surface into the step output (for the run-detail
        // cost chip / "budget reached" badge, §19). A clean run omits the flags;
        // a cost-capped run carries `cost_usd` + `cost_capped`.
        let clean = agent_outcome_to_action(&answered, None);
        let clean_out = clean.output.as_ref().unwrap();
        assert!(clean_out.get("cost_usd").is_none());
        assert!(clean_out.get("cost_capped").is_none());
        assert!(clean_out.get("iteration_capped").is_none());
        assert!(clean_out.get("tool_loop_capped").is_none());

        let capped = AgentOutcome {
            content: "partial".into(),
            iterations: 3,
            usage: Some(catalerum_core::stream::Usage {
                cost_usd: Some(1.25),
                ..Default::default()
            }),
            hit_cost_limit: true,
            ..Default::default()
        };
        let out = agent_outcome_to_action(&capped, None);
        let o = out.output.as_ref().unwrap();
        assert_eq!(o["cost_usd"], json!(1.25));
        assert_eq!(o["cost_capped"], json!(true));
        assert!(o.get("iteration_capped").is_none());

        let loop_capped = AgentOutcome {
            content: "partial".into(),
            iterations: 3,
            hit_tool_loop_cap: true,
            ..Default::default()
        };
        let out = agent_outcome_to_action(&loop_capped, None);
        assert_eq!(
            out.output.as_ref().unwrap()["tool_loop_capped"],
            json!(true)
        );
    }

    #[test]
    fn extract_json_handles_bare_and_fenced_replies() {
        // Bare JSON.
        assert_eq!(
            extract_json(r#"{"yes":0.7,"no":0.3}"#),
            Some(json!({ "yes": 0.7, "no": 0.3 }))
        );
        // A ```json fenced block (the common LLM habit).
        assert_eq!(
            extract_json("```json\n{\"a\": 1}\n```"),
            Some(json!({ "a": 1 }))
        );
        // A bare ``` fence + surrounding whitespace.
        assert_eq!(extract_json("  ```\n[1, 2]\n```  "), Some(json!([1, 2])));
        // Prose around JSON doesn't parse → None (the agent step keeps raw content).
        assert_eq!(extract_json("Sure! Here you go: {\"a\":1}"), None);
        assert_eq!(extract_json("not json at all"), None);
    }

    #[test]
    fn system_with_skills_appends_runbooks_or_leaves_base_unchanged() {
        // No skills → base unchanged.
        assert_eq!(system_with_skills("base prompt", &[]), "base prompt");
        // Skills → base + each runbook, in order.
        let s = system_with_skills("base prompt", &["do X first".into(), "then do Y".into()]);
        assert!(s.starts_with("base prompt"));
        assert!(s.contains("# Skills"));
        let x = s.find("do X first").expect("skill 1 present");
        let y = s.find("then do Y").expect("skill 2 present");
        assert!(x < y, "skills are appended in order");
    }

    #[tokio::test]
    async fn llm_agent_without_a_client_fails_cleanly() {
        // A runner with no `with_llm` reports a clear Failed outcome for an
        // LlmAgent action (rather than panicking or silently succeeding).
        let runner = ToolActionRunner::new(ToolRegistry::default());
        let action = Action {
            kind: ActionKind::LlmAgent,
            params: serde_json::Map::new(),
        };
        let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome
            .error
            .as_deref()
            .unwrap()
            .contains("needs an LLM client"));
    }

    #[tokio::test]
    async fn summarize_without_a_client_fails_cleanly() {
        // Summarize, like LlmAgent, needs the client — without one it reports a
        // clear Failed outcome rather than panicking or silently succeeding.
        let runner = ToolActionRunner::new(ToolRegistry::default());
        let action = Action {
            kind: ActionKind::Summarize,
            params: serde_json::Map::from_iter([("input".to_string(), json!("some text"))]),
        };
        let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome
            .error
            .as_deref()
            .unwrap()
            .contains("needs an LLM client"));
    }

    #[test]
    fn summarize_request_seeds_input_instructions_and_model() {
        // Explicit input + instructions + max_words + pinned model.
        let params = serde_json::Map::from_iter([
            ("input".to_string(), json!("  the text to condense  ")),
            ("instructions".to_string(), json!("Focus on action items.")),
            ("max_words".to_string(), json!(50)),
            ("model".to_string(), json!("some/model")),
        ]);
        let req = summarize_request(&params, None, "default/model").unwrap();
        assert_eq!(req.model, "some/model");
        assert_eq!(req.messages.len(), 2);
        let system = &req.messages[0].content;
        assert!(system.contains("Focus on action items."));
        assert!(system.contains("under 50 words"));
        assert_eq!(req.messages[1].content, "the text to condense");

        // No input → the firing trigger event is summarized (graph-context shape:
        // the event lives under `trigger`, mirroring `firing_event`).
        let trigger = json!({ "trigger": { "kind": "webhook", "path": "/x" }, "inputs": {} });
        let req =
            summarize_request(&serde_json::Map::new(), Some(&trigger), "default/model").unwrap();
        assert_eq!(req.model, "default/model");
        assert!(req.messages[1].content.contains("\"kind\": \"webhook\""));
        // …and the wrapper's `inputs` is NOT what gets summarized.
        assert!(!req.messages[1].content.contains("inputs"));

        // A whole-value template resolves to non-string JSON → pretty-printed.
        let params = serde_json::Map::from_iter([("input".to_string(), json!({ "a": [1, 2] }))]);
        let req = summarize_request(&params, None, "default/model").unwrap();
        assert!(req.messages[1].content.contains("\"a\""));
    }

    #[test]
    fn summarize_request_rejects_nothing_to_summarize_and_truncates() {
        // No input and no trigger → a clear error.
        let err = summarize_request(&serde_json::Map::new(), None, "m").unwrap_err();
        assert!(err.contains("no `input`"), "got: {err}");
        // An input that template-resolved to empty → a clear error (a silent
        // empty summary would hide a broken {{ path }}).
        let params = serde_json::Map::from_iter([("input".to_string(), json!("   "))]);
        let err = summarize_request(&params, None, "m").unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
        // Oversized input truncates on a char boundary with an explicit marker.
        let big = "é".repeat(MAX_SUMMARIZE_INPUT_BYTES); // 2 bytes/char → over cap
        let params = serde_json::Map::from_iter([("input".to_string(), json!(big))]);
        let req = summarize_request(&params, None, "m").unwrap();
        let user = &req.messages[1].content;
        assert!(user.ends_with("[input truncated]"));
        assert!(user.len() <= MAX_SUMMARIZE_INPUT_BYTES + 32);
    }

    #[tokio::test]
    async fn run_skill_without_a_client_fails_cleanly() {
        // RunSkill, like LlmAgent, runs the §7 loop — without an LLM client it reports
        // a clear Failed outcome rather than panicking or silently succeeding.
        let runner = ToolActionRunner::new(ToolRegistry::default());
        let action = Action {
            kind: ActionKind::RunSkill,
            params: serde_json::Map::from_iter([("skill".to_string(), json!("triage-inbox"))]),
        };
        let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
        assert_eq!(outcome.status, StepStatus::Failed);
        assert!(outcome
            .error
            .as_deref()
            .unwrap()
            .contains("needs an LLM client"));
    }

    #[test]
    fn channel_from_trigger_extracts_channel_message_only() {
        assert_eq!(
            channel_from_trigger(Some(
                &json!({ "kind": "channel_message", "channel": "ops", "text": "hi" })
            )),
            Some("ops".to_string())
        );
        assert_eq!(
            channel_from_trigger(Some(&json!({ "kind": "webhook", "path": "/x" }))),
            None
        );
        assert_eq!(channel_from_trigger(None), None);
        // A channel message missing its channel name → nothing to reply to.
        assert_eq!(
            channel_from_trigger(Some(&json!({ "kind": "channel_message" }))),
            None
        );
    }

    #[test]
    fn channel_helpers_unwrap_graph_wrapped_context() {
        // In a graph run the executor wraps the context as `{ "trigger": <event>,
        // "inputs": … }`; the channel helpers must still find the channel/text, else
        // the SOUL §25 in-channel reply silently no-ops for graph automations (works
        // in the linear path, which passes the raw event).
        let graph = json!({
            "trigger": { "kind": "channel_message", "channel": "tg:42", "text": "hi there" },
            "inputs": { "some_node": { "data": 1 } }
        });
        assert_eq!(
            channel_from_trigger(Some(&graph)),
            Some("tg:42".to_string())
        );
        assert_eq!(
            channel_text_from_trigger(Some(&graph)),
            Some("hi there".to_string())
        );

        // The linear shape (the event itself) still works.
        let linear = json!({ "kind": "channel_message", "channel": "ops", "text": "yo" });
        assert_eq!(channel_from_trigger(Some(&linear)), Some("ops".to_string()));
        assert_eq!(
            channel_text_from_trigger(Some(&linear)),
            Some("yo".to_string())
        );

        // A graph run of a non-channel trigger → no channel reply.
        let graph_webhook = json!({ "trigger": { "kind": "webhook", "path": "/x" }, "inputs": {} });
        assert_eq!(channel_from_trigger(Some(&graph_webhook)), None);

        // firing_event unwraps the graph shape and passes the linear one through.
        assert_eq!(
            firing_event(Some(&graph))
                .and_then(|e| e.get("kind"))
                .and_then(Value::as_str),
            Some("channel_message")
        );
        assert_eq!(
            firing_event(Some(&linear))
                .and_then(|e| e.get("kind"))
                .and_then(Value::as_str),
            Some("channel_message")
        );
    }

    /// A [`catalerum_channels::Channel`] that records the text it's sent — proves an
    /// agent's reply is delivered to the triggering channel (no LLM needed).
    struct RecordingChannel(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    #[async_trait]
    impl catalerum_channels::Channel for RecordingChannel {
        fn kind(&self) -> &str {
            "test"
        }
        async fn send(
            &self,
            msg: &catalerum_channels::OutMessage,
        ) -> catalerum_channels::Result<()> {
            self.0.lock().unwrap().push(msg.text.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn channel_triggered_agent_reply_is_delivered_to_the_channel() {
        use std::sync::{Arc, Mutex};
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut channels = std::collections::HashMap::new();
        channels.insert(
            "ops".to_string(),
            Arc::new(RecordingChannel(sent.clone())) as Arc<dyn catalerum_channels::Channel>,
        );
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(crate::tools::NotifyTool::new(channels)));
        let runner = ToolActionRunner::new(registry);
        // base Member holds `channel:write`, so the reply dispatch is authorized.
        let ctx = ToolContext {
            workspace_id: Some(WorkspaceId::new()),
            capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
            ..Default::default()
        };

        // A channel-message trigger → the (trimmed) reply lands on that channel.
        let trigger = json!({ "kind": "channel_message", "channel": "ops", "text": "hi" });
        runner
            .deliver_channel_reply(&ctx, Some(&trigger), "  hello from the bot  ")
            .await;
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &["hello from the bot".to_string()]
        );

        // A non-channel trigger and an empty reply both deliver nothing.
        runner
            .deliver_channel_reply(&ctx, Some(&json!({ "kind": "webhook", "path": "/x" })), "x")
            .await;
        runner
            .deliver_channel_reply(&ctx, Some(&trigger), "   ")
            .await;
        assert_eq!(
            sent.lock().unwrap().len(),
            1,
            "only the channel-triggered, non-empty reply was sent"
        );
    }
}
