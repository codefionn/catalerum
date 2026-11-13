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
//! An `LlmDecision` action makes one native `/v1/decisions` call with structured
//! state and named questions. Its model comes from the action, acting user's
//! decision setting, or [`with_decision_model`](ToolActionRunner::with_decision_model).
//! It requires `conversation:write`, keeps raw `answers`, and projects scalar
//! results into `data` for branch expressions. Dry runs spend nothing. A grant
//! with `cost_limit` is refused because this endpoint has no hard spend ceiling.
//!
//! A **`CreateChatThread`** action (SOUL §11/§12) is the in-product output sink:
//! it atomically creates an `automation`-origin conversation and writes the
//! template-rendered `message` as its first assistant turn. It is gated on
//! `conversation:write`, appears in the ordinary Chat list, and returns both ids
//! for downstream graph nodes.

mod collect;
mod prompt;
mod templating;

#[cfg(test)]
mod decision_tests;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use async_trait::async_trait;
use catalerum_automation::{Action, ActionKind, ActionOutcome, ActionRunner};
use catalerum_core::capability::{Action as CapAction, Capability, Resource};
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{Attachment, ExtractedAttachment, Grant, MessageRole, Origin, Role};
use catalerum_core::tool::{ToolContext, ToolRegistry};
use catalerum_core::{AgentId, MailboxId, UserId, WorkspaceId};
use catalerum_llm::{run_agent, AgentConfig, AgentOutcome, OpenRouterClient};
use catalerum_script::CodeToolHost;
use catalerum_store::Store;
use serde_json::{json, Value};

use crate::state::StorageRegistry;

use prompt::{
    agent_outcome_to_action, agent_request, channel_from_trigger, channel_text_from_trigger,
    extract_json, summarize_request, system_with_skills, trigger_prompt, JSON_OUTPUT_INSTRUCTION,
};
use templating::render_params;

pub(crate) use collect::get_or_create_local_calendar_by_name;

/// The default system prompt for an `LlmAgent` action that doesn't supply its own.
const DEFAULT_AGENT_SYSTEM: &str = "You are an automation agent for catalerum. You were \
triggered to carry out a configured task. Use the tools available to you, stay within your \
authority, and stop when the task is done.";

/// The seed user turn that kicks an automation agent off (it has no human prompt —
/// a trigger fired it; its instructions live in the system prompt).
const AGENT_TRIGGER_PROMPT: &str =
    "This automation has been triggered. Carry out your instructions now.";

/// The LLM client + default model an [`LlmAgent`](catalerum_automation::LlmAgent) action
/// runs the §7 loop against.
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
    /// Decision model fallback. Kept separate from the chat model.
    decision_model: Option<String>,
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
            decision_model: None,
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
            decision_model: None,
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

    /// Set the server decision model fallback. An explicit action model or the
    /// acting user's decision setting takes precedence.
    #[must_use]
    pub fn with_decision_model(mut self, model: Option<String>) -> Self {
        self.decision_model = model;
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
                .or_else(|| Some(default_automation_capabilities())),
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

/// The authority an automation runs under when it has **no** §19 grant: the
/// workspace owner as principal, bounded to base **Member** capabilities (ordinary
/// read/write — never delete / exec / admin). Shared by [`ToolActionRunner`]'s
/// context resolution and the authoring-time authority checks
/// (`tools::automation_debug`), so the diagnosis can never drift from the runtime.
pub(crate) fn default_automation_capabilities() -> Vec<Capability> {
    catalerum_iam::base_capabilities(Role::Member)
}

/// The registry tool that backs an automation [`ActionKind`], if one exists yet.
pub(crate) fn tool_for(kind: ActionKind) -> Option<&'static str> {
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

    /// Execute one native typed decision under the automation's authority.
    async fn run_llm_decision(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let Some(llm) = &self.llm else {
            return ActionOutcome::failed("llm_decision needs an LLM client; this runner has none");
        };
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(err) => return ActionOutcome::failed(err),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("conversation")),
        ) {
            return ActionOutcome::failed(
                "llm_decision denied: requires conversation:write authority",
            );
        }
        let rendered = Action {
            kind: action.kind,
            params: render_params(&action.params, trigger),
        };
        let Some(spec) = rendered.as_llm_decision() else {
            return ActionOutcome::failed("malformed llm_decision parameters");
        };
        if let Err(err) = rendered.validate_payload() {
            return ActionOutcome::failed(err);
        }
        let user_model = match (&self.store, ctx.user_id, spec.model.as_ref()) {
            (Some(store), Some(user), None) => store
                .llm_settings()
                .get(workspace_id, user)
                .await
                .ok()
                .and_then(|settings| settings.decision_model),
            _ => None,
        };
        let model = match crate::tools::decision_model(
            spec.model.as_deref(),
            user_model.as_deref(),
            self.decision_model.as_deref(),
        ) {
            Ok(model) => model,
            Err(err) => return ActionOutcome::failed(err.to_string()),
        };
        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({ "dry_run": true, "model": model })));
        }
        if let Err(err) = crate::tools::check_decision_cost_limit(
            grant.and_then(|grant| grant.constraints.cost_limit),
        ) {
            return ActionOutcome::failed(err.to_string());
        }
        if let Err(err) = crate::tools::validate_decision_model(&llm.client, &model).await {
            return ActionOutcome::failed(err.to_string());
        }
        let request = catalerum_llm::DecisionRequest {
            model,
            state: spec
                .state
                .unwrap_or_else(|| trigger.cloned().unwrap_or_else(|| json!({}))),
            questions: spec.questions,
        };
        match llm
            .client
            .decide(request)
            .await
            .and_then(crate::tools::decision_output)
        {
            Ok(output) => ActionOutcome::succeeded(Some(output)),
            Err(err) => ActionOutcome::failed(format!("llm_decision: {err}")),
        }
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
        if action.kind == ActionKind::LlmDecision {
            return self
                .run_llm_decision(workspace_id, action, trigger, grant)
                .await;
        }
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
            match crate::object_service::write_object_bytes(
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
            match crate::object_service::write_object_bytes(
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
        let handle = match crate::object_service::resolve_store(
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
                crate::object_service::delete_object_at(&store, &handle, workspace_id, &key).await
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
