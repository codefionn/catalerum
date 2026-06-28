//! The LLM tool registry (SOUL §7, §19, §21).
//!
//! These are the typed, scoped tools the chat agent loop ([`catalerum_llm::run_agent_streaming`])
//! dispatches against. Each is a thin, in-process client of a repository (the
//! same source of truth the REST routes use), and every call is **workspace-scoped
//! from the [`ToolContext`]** — the model never names a workspace, so cross-workspace
//! reach is impossible by construction (SOUL §18). **Per-action capabilities are
//! enforced** at [`ToolRegistry::dispatch`](catalerum_core::tool::ToolRegistry):
//! each tool declares a [`required_capability`](catalerum_core::tool::Tool::required_capability)
//! (e.g. `notes:write`, `memory:read`, `vector:search`) and a call is denied
//! unless the caller's grant (the chat runs under the user's role capabilities)
//! covers it (SOUL §19, deny-by-default). `fetch_url`'s `web:read` egress scope is
//! still ungated pending host-glob policy (§27).
//!
//! Registered here:
//! - `create_note` / `edit_note` / `read_note` / `list_notes` — markdown notes
//!   (SOUL §21), thin clients of [`NoteRepo`].
//! - `fetch_url` — fetch a web page as clean Markdown (SOUL §27), wrapping the
//!   configured [`WebFetcher`]; only registered when a fetch backend exists.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_channels::{Channel, OutMessage};
use catalerum_core::ask::Question;
use catalerum_core::audio::{SpeechRequest, TranscriptionRequest};
use catalerum_core::capability::{attenuate, Action, Capability, Resource};
use catalerum_core::embed::EmbeddingRequest;
use catalerum_core::error::{Error, Result};
use catalerum_core::model::{
    Attachment, Author, Board, Code, ConnectionKind, LlmSettings, Map, McpAuthSpec, McpServerDef,
    MemoryScope, Skill, TaskStatus,
};
use catalerum_core::model_ui::{
    apply_ui_patch, validate_ui_spec, NodeKind, UiNode, UiPatchOp, UiSpec, UiView,
};
use catalerum_core::provider::{
    CommandSpec, Embedder, Executor, SpeechSynthesizer, Transcriber, WebFetcher, WebSearcher,
};
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry, MODEL_MEDIA_RESULT_FIELD};
use catalerum_core::{
    BoardId, BucketId, CalendarId, ColumnId, ConversationId, EmailId, EventId, GrantId, LinkId,
    MailboxId, MediaInput, MemoryId, NoteId, ObjectId, SourceRef, TaskId, UiDefinitionId, UserId,
    WorkspaceId,
};
use catalerum_fetch::{ExtractHtmlTool, FetchUrlTool, HtmlToMarkdownTool};
use catalerum_graph::{GraphStore, NoteHit, RelatedNote, WorkspaceFacts};
use catalerum_llm::{ModelInfo, ModelKind, OpenRouterClient};
use catalerum_script::{JsLimits, ScriptCodeRunner};
use catalerum_search::{SearchDefaults, WebSearchTool};
use catalerum_store::{
    source_from_parts, DateRange, EventPatch, LinkRepo, MemoryRepo, NewAgentProfile,
    NewMcpServerDef, NewSkill, NoteRepo, PendingQuestionRepo, ProfileRepo, SearchSettingsRepo,
    Store, StoreError, UiDefinitionInput, UiDefinitionRepo, UpsertEvent,
};

use crate::download_link::{DownloadClaims, DownloadSigner};
use crate::mcp_manager::McpManager;
use crate::sandbox::WorkspaceSandboxManager;
use crate::state::StorageRegistry;
use crate::tool_index::{tool_allowed, ToolIndex, LIST_TOOLS_NAME, SEARCH_TOOLS_NAME};
use crate::trigger_link::{TriggerClaims, TriggerSigner};
use catalerum_vector::{ScoredPoint, SearchFilter, SearchQuery, VectorStore};

/// Best-effort note (re-)projection: after a note is written, enqueue the
/// derived-store jobs that reconcile its projections — `ingest_note` (chunks →
/// Qdrant, SOUL §6.4) and `project_note` (→ Neo4j graph, SOUL §6.3). Each is
/// gated on whether a worker can serve it (`[qdrant].enabled` / `[neo4j].enabled`)
/// — no point enqueuing a job nothing will run — and **never** allowed to fail a
/// write: a queue hiccup logs a warning and the note still saves; the next edit
/// re-enqueues. Shared by the note REST handlers and the `create_note`/`edit_note`
/// LLM tools so both write paths project identically.
#[derive(Clone)]
pub(crate) struct NoteIngest {
    store: Store,
    /// Whether an embed-capable worker is running (`[qdrant].enabled`).
    embed: bool,
    /// Whether a graph-capable worker is running (`[neo4j].enabled`).
    graph: bool,
}

impl NoteIngest {
    /// Build the hook from the two derived-store toggles.
    pub(crate) fn new(store: Store, embed: bool, graph: bool) -> Self {
        Self {
            store,
            embed,
            graph,
        }
    }

    /// Enqueue the reconcile jobs for `note_id`, best-effort. Each kind is a
    /// no-op when its store is disabled.
    pub(crate) async fn enqueue(&self, workspace_id: WorkspaceId, note_id: NoteId) {
        if self.embed {
            if let Err(e) =
                catalerum_ingest::enqueue_ingest_note(&self.store, workspace_id, note_id).await
            {
                tracing::warn!(error = %e, %note_id, "failed to enqueue note embed (note still saved)");
            }
        }
        if self.graph {
            if let Err(e) =
                catalerum_ingest::enqueue_project_note(&self.store, workspace_id, note_id).await
            {
                tracing::warn!(error = %e, %note_id, "failed to enqueue note graph projection (note still saved)");
            }
        }
    }

    /// Enqueue an event's graph (re-)projection for `event_id`, best-effort (a
    /// no-op when the graph is disabled). Used after a local-calendar event
    /// create/edit/delete so the `:Event` node + `SCHEDULED_IN`/`ABOUT` edges
    /// reconcile (SOUL §6.3/§8); a deleted event reconciles to a purge. There is
    /// no event embed path yet, so only the graph job is enqueued.
    pub(crate) async fn enqueue_event(&self, workspace_id: WorkspaceId, event_id: EventId) {
        if self.graph {
            if let Err(e) =
                catalerum_ingest::enqueue_project_event(&self.store, workspace_id, event_id).await
            {
                tracing::warn!(error = %e, %event_id, "failed to enqueue event graph projection (event still saved)");
            }
        }
    }

    /// Enqueue a link's graph (re-)projection for `link_id`, best-effort (a no-op
    /// when the graph is disabled). Used after a link create/delete so the
    /// `RELATES_TO` edge reconciles (SOUL §6.3); a deleted link reconciles to a
    /// purge (the worker finds the link gone). Links have no embed path.
    pub(crate) async fn enqueue_link(&self, workspace_id: WorkspaceId, link_id: LinkId) {
        if self.graph {
            if let Err(e) =
                catalerum_ingest::enqueue_project_link(&self.store, workspace_id, link_id).await
            {
                tracing::warn!(error = %e, %link_id, "failed to enqueue link graph projection (link still saved)");
            }
        }
    }

    /// Enqueue a memory (re-)embed for `memory_id`, best-effort (a no-op when
    /// embedding is disabled). Used after a `remember`/`forget` write so the
    /// memory's vectors reconcile (SOUL §22).
    pub(crate) async fn enqueue_memory(&self, workspace_id: WorkspaceId, memory_id: MemoryId) {
        if self.embed {
            if let Err(e) =
                catalerum_ingest::enqueue_ingest_memory(&self.store, workspace_id, memory_id).await
            {
                tracing::warn!(error = %e, %memory_id, "failed to enqueue memory embed (memory still saved)");
            }
        }
    }
}

/// `notify` — deliver a message to the workspace's configured channel (SOUL
/// §25/§11/§7). Registered only when a channel is configured (`[channels]`); the
/// destination is the channel itself (e.g. a Discord webhook). Gated on
/// `channel:write` (a Viewer can't notify).
pub(crate) struct NotifyTool {
    /// Configured channels by name; `default` is used when none is named.
    channels: std::collections::HashMap<String, Arc<dyn Channel>>,
}

impl NotifyTool {
    pub(crate) fn new(channels: std::collections::HashMap<String, Arc<dyn Channel>>) -> Self {
        Self { channels }
    }

    /// Sorted channel names, for the unknown-channel error message.
    fn available(&self) -> String {
        let mut names: Vec<&str> = self.channels.keys().map(String::as_str).collect();
        names.sort_unstable();
        names.join(", ")
    }
}

#[async_trait]
impl Tool for NotifyTool {
    fn name(&self) -> &str {
        "notify"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "channel")
    }

    fn description(&self) -> &str {
        "Send a short notification message to a channel (e.g. a Discord webhook). \
         The optional `channel` names which configured channel to use (default: \"default\")."
    }

    fn parameters_schema(&self) -> Json {
        // Surface the *configured* channel names as an enum so the model picks a
        // valid one instead of guessing and learning the set only from an error.
        let mut names: Vec<&str> = self.channels.keys().map(String::as_str).collect();
        names.sort_unstable();
        json!({
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The message text to deliver." },
                "channel": {
                    "type": "string",
                    "enum": names,
                    "description": "Which configured channel to deliver to (default: \"default\")."
                }
            },
            "required": ["message"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let message = required_str(&args, "message")?;
        let name = args
            .get("channel")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("default");
        let channel = self.channels.get(name).ok_or_else(|| {
            Error::invalid(format!(
                "unknown channel `{name}`; configured: {}",
                self.available()
            ))
        })?;
        channel
            .send(&OutMessage::text(message))
            .await
            .map_err(|e| Error::provider(format!("channel delivery failed: {e}")))?;
        Ok(json!({ "delivered": true, "channel": name, "kind": channel.kind() }))
    }
}

/// Build the chat tool registry from the application's services.
///
/// The four note tools are always registered (notes are a core surface), as are
/// the pure HTML transforms `html_to_markdown` / `extract_html` (no backend or
/// capability needed); the
/// `fetch_url` tool is registered only when a web-fetch backend is configured
/// (`fetcher.is_some()`), mirroring the `POST /fetch` route's availability; the
/// `search_semantic` tool only when a vector index is configured
/// (`search.is_some()`, i.e. `[qdrant].enabled`); the `query_graph` tool only
/// when a graph is configured (`graph.is_some()`, i.e. `[neo4j].enabled`). The
/// `create_note`/`edit_note` tools carry the [`NoteIngest`] hook so an
/// LLM-authored note is projected the same as a UI edit.
#[must_use]
// Each arg is a distinct already-built service/registry input; bundling them into
// a struct would only move the long list one call up.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_registry(
    store: &Store,
    fetcher: Option<&Arc<dyn WebFetcher>>,
    ingest: NoteIngest,
    search: Option<SemanticSearch>,
    graph: Option<GraphQuery>,
    executor: Option<Arc<dyn Executor>>,
    ui_handler_tools: Vec<String>,
    sandbox: Option<Arc<WorkspaceSandboxManager>>,
    secrets: Option<Arc<catalerum_store::SecretStore>>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    let notes = store.notes();
    let mem_ingest = ingest.clone();
    registry.register(Arc::new(CreateNoteTool {
        notes: notes.clone(),
        ingest: ingest.clone(),
    }));
    registry.register(Arc::new(EditNoteTool {
        notes: notes.clone(),
        ingest: ingest.clone(),
    }));
    // `delete_note` completes note CRUD for the agent (parity with the
    // `DELETE /notes/{id}` route + the workbench's delete control); gated on
    // `notes:write` like the route, and it reconciles the derived projection.
    registry.register(Arc::new(DeleteNoteTool {
        notes: notes.clone(),
        ingest: ingest.clone(),
    }));
    registry.register(Arc::new(ReadNoteTool {
        notes: notes.clone(),
    }));
    registry.register(Arc::new(ListNotesTool { notes }));
    // Link tools (SOUL §5/§6.3): relate any two objects. Thin `LinkRepo` clients,
    // gated on `links:read`/`links:write` (parity with the `/links` REST routes);
    // writes best-effort project/purge the `RELATES_TO` edge via the ingest hook.
    let links = store.links();
    registry.register(Arc::new(CreateLinkTool {
        links: links.clone(),
        ingest: ingest.clone(),
    }));
    registry.register(Arc::new(ListLinksTool {
        links: links.clone(),
    }));
    registry.register(Arc::new(DeleteLinkTool {
        links,
        ingest: ingest.clone(),
    }));
    // Emerged-UI tools (the "emerged UI" feature): always available, thin
    // `UiDefinitionRepo` clients. `present_ui`/`create_ui_components`/
    // `edit_ui_components`/`edit_ui` author Apps without requiring one giant
    // definition payload;
    // `read_ui`/`list_uis`/`delete_ui` round out CRUD. All are gated on
    // `ui:write`/`ui:read`; `explain_ui_schema` lists the component vocabulary
    // and `get_ui_schema` returns selected details on demand.
    // Handler-tool validation uses the
    // server-defined allow-list (`[ui].handler_tools`) — the trust boundary that
    // keeps an AI-authored UI strictly less powerful than chat.
    let ui_allow = Arc::new(ui_handler_tools.into_iter().collect::<HashSet<String>>());
    let uis = store.ui_definitions();
    registry.register(Arc::new(PresentUiTool {
        uis: uis.clone(),
        allow: ui_allow.clone(),
    }));
    registry.register(Arc::new(CreateUiComponentsTool {
        uis: uis.clone(),
        allow: ui_allow.clone(),
    }));
    registry.register(Arc::new(EditUiComponentsTool {
        uis: uis.clone(),
        allow: ui_allow.clone(),
    }));
    registry.register(Arc::new(EditUiTool {
        uis: uis.clone(),
        allow: ui_allow.clone(),
    }));
    registry.register(Arc::new(ReadUiTool { uis: uis.clone() }));
    registry.register(Arc::new(ListUisTool { uis: uis.clone() }));
    registry.register(Arc::new(DeleteUiTool { uis }));
    registry.register(Arc::new(ExplainUiSchemaTool));
    registry.register(Arc::new(GetUiSchemaTool));

    // Per-App durable key/value store (SOUL §12/§29) — where an emerged App persists
    // its data model. Thin `AppDataRepo` clients gated on `ui:read`/`ui:write`; from an
    // App handler the namespace is forced to the firing App (isolation), elsewhere it is
    // an explicit `app` argument. Always available (store-only).
    registry.register(Arc::new(AppDataGetTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(AppDataSetTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(AppDataListTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(AppDataDeleteTool {
        store: store.clone(),
    }));

    // `ask_user` — always registered; it degrades to an error outside an interactive
    // chat (no `ToolContext::conversation_id`). Persists a durable pending question so
    // the form survives a reload/reconnect. Ungated: it grants no authority.
    registry.register(Arc::new(AskUserTool {
        pending: store.pending_questions(),
    }));

    // `query_structured` only needs the store, so it is always available. `AppState`
    // re-registers it with the storage registry (`register_query_structured`) so
    // object operations resolve store names; this copy falls back to connection names.
    registry.register(Arc::new(QueryStructuredTool {
        store: store.clone(),
        storage: None,
    }));
    // Automation-authoring tools (SOUL §11): create/edit/test/run the workspace's
    // automations — thin `Store` clients, gated on `automation:read`/`automation:write`
    // (§19). `search_automation_node_types` (the node-type discovery tool) is registered
    // from `AppState` since it needs the embedding index, not the store.
    register_automation_tools(&mut registry, store);
    // Agent-profile-authoring tools (SOUL §19/§25): list/get/create/update/delete the
    // workspace's named scoped-agent profiles — thin `Store` clients, gated on
    // `agent_profile:read`/`agent_profile:write` (§19, admin-only: no base role
    // implies that domain, so only an Owner/Admin `*` reaches them — deny-by-default,
    // like grants). Mirrors `register_automation_tools`.
    register_agent_profile_tools(&mut registry, store);
    // `read_object` (SOUL §9/§10) — full extracted text of one stored file by id;
    // thin store client, gated on `storage:read`.
    registry.register(Arc::new(ReadObjectTool {
        store: store.clone(),
    }));
    // `search_files` (SOUL §9/§10) — literal substring search over files' extracted
    // text; thin store client, gated on `storage:read`. Literal complement to the
    // semantic `search_semantic`.
    registry.register(Arc::new(SearchObjectsTool {
        store: store.clone(),
    }));
    // `search_messages` (SOUL §7/§12) — literal substring search over past chat
    // messages; thin store client, gated on `conversation:read`. The only way to
    // search chat history (messages aren't embedded for `search_semantic`).
    registry.register(Arc::new(SearchMessagesTool {
        store: store.clone(),
    }));
    // `read_conversation` (SOUL §7/§12) — read one chat thread's recent messages by
    // id; the read half of `search_messages`, gated on `conversation:read`.
    registry.register(Arc::new(ReadConversationTool {
        store: store.clone(),
    }));
    // `get_emails` (SOUL §7/§28) — read-only email lookups; always available
    // (thin store client), gated on `email:read` (deny-by-default per §19).
    registry.register(Arc::new(GetEmailsTool {
        store: store.clone(),
    }));
    // `read_email` (SOUL §7/§28) — one email's full body by id; thin store client,
    // gated on `email:read`.
    registry.register(Arc::new(ReadEmailTool {
        store: store.clone(),
    }));
    // Source-connection tools (SOUL §8/§10/§28): discover + register the email/
    // calendar sources a collect trigger pulls from, so a chat can author a full
    // ingest automation end-to-end (connection → collect trigger → write). List is
    // per-kind gated in-invoke (`email:read`/`calendar:read`); the creates are
    // gated `email:write`/`calendar:write` like their REST twins.
    registry.register(Arc::new(ListConnectionsTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateEmailConnectionTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateCalendarConnectionTool {
        store: store.clone(),
    }));
    // Memory tools (SOUL §22) are always available — thin `MemoryRepo` clients;
    // forget/update carry the ingest hook so a memory (re-)embeds on write.
    // `remember` instead routes through the shared dedup seam (SOUL §29), which
    // handles the (re-)embed itself; it carries `search` (present iff a vector
    // backend is configured) for the seam's embedding-similarity layer.
    let memories = store.memories();
    registry.register(Arc::new(RememberTool {
        store: store.clone(),
        search: search.clone(),
    }));
    registry.register(Arc::new(RecallTool {
        memories: memories.clone(),
    }));
    registry.register(Arc::new(UpdateMemoryTool {
        memories: memories.clone(),
        ingest: mem_ingest.clone(),
    }));
    registry.register(Arc::new(ForgetTool {
        memories,
        ingest: mem_ingest,
    }));
    // Profile tool (SOUL §22) — always available, a thin `ProfileRepo` client.
    registry.register(Arc::new(UpdateProfileTool {
        profiles: store.profiles(),
    }));
    // `current_time` (SOUL §7) — the wall-clock now, rendered in the user's
    // profile timezone (or an explicit one). Ungated pure utility; reads the
    // profile only to resolve the timezone, so it shares the `ProfileRepo`.
    registry.register(Arc::new(CurrentTimeTool {
        profiles: store.profiles(),
    }));
    // `create_calendar` (SOUL §8/§11) — mint a new local calendar to write
    // events into; thin `CalendarRepo` client, gated on `calendar:write`.
    registry.register(Arc::new(CreateCalendarTool {
        store: store.clone(),
    }));
    // Calendar write tool (SOUL §8/§11) — always available, a thin
    // `CalendarRepo`/`EventRepo` client; gated on `calendar:write`. The
    // `CreateEvent` automation action dispatches through this same tool.
    // `secrets` powers provider write-back on writable provider calendars
    // (CalDAV/Google/Outlook) via `crate::calendar_writeback`.
    registry.register(Arc::new(CreateEventTool {
        store: store.clone(),
        ingest: ingest.clone(),
        secrets: secrets.clone(),
    }));
    // The `UpdateEvent` automation action edits an existing event in place
    // through this same `EventRepo` client; gated on `calendar:write`.
    registry.register(Arc::new(UpdateEventTool {
        store: store.clone(),
        ingest: ingest.clone(),
        secrets: secrets.clone(),
    }));
    // Deleting an event completes the calendar CRUD set; gated on
    // `calendar:delete` (no base role holds it — admin/granted only).
    registry.register(Arc::new(DeleteEventTool {
        store: store.clone(),
        ingest: ingest.clone(),
        secrets: secrets.clone(),
    }));
    // `read_event` (SOUL §7/§8) — one event's full detail by id; thin store
    // client, gated on `calendar:read`.
    registry.register(Arc::new(ReadEventTool {
        store: store.clone(),
    }));
    // `search_events` (SOUL §7/§8) — literal substring search over event
    // summary/location/body/attendees across ALL dates (past included) — the
    // content-search complement to query_structured's date-window event ops.
    // Thin store client, gated on `calendar:read`.
    registry.register(Arc::new(SearchEventsTool {
        store: store.clone(),
    }));
    // Task tools (SOUL §24) — always available, thin board/task repo clients.
    // Board- and column-addressing is by *name* (or id) via `resolve_board_arg`
    // / `resolve_column_arg`, so chat can act on "the Sprint board" directly.
    registry.register(Arc::new(CreateBoardTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateTaskTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(MoveTaskTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(CompleteTaskTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(SetTaskStatusTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(DeleteTaskTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(EditTaskTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(NextTaskTool {
        store: store.clone(),
    }));
    // `fire_trigger` (SOUL §11/§12) — emit a named signal that runs every automation
    // headed by a matching `{ kind: "trigger", name }` trigger. The on-demand bridge
    // an emerged UI (or chat) uses to kick off a backend workflow; thin dispatch
    // client, gated on `automation:write`.
    registry.register(Arc::new(FireTriggerTool {
        store: store.clone(),
    }));
    // `read_task` (SOUL §7/§24) — one task's full detail (incl. body) by id; the
    // read twin of query_structured's task summaries. Thin store client, `tasks:read`.
    registry.register(Arc::new(ReadTaskTool {
        store: store.clone(),
    }));
    // `search_tasks` (SOUL §7/§24) — literal substring search over task title+body;
    // the content-search complement to query_structured. Thin store client, `tasks:read`.
    registry.register(Arc::new(SearchTasksTool {
        store: store.clone(),
    }));
    // Skill tools (SOUL §23) — always available, thin `SkillRepo` clients.
    registry.register(Arc::new(UseSkillTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(ListSkillsTool {
        store: store.clone(),
    }));
    // `create_skill` / `edit_skill` (SOUL §23) — chat authors the skill library:
    // create a named runbook, or partially update one in place. Thin `SkillRepo`
    // clients at parity with `POST /skills` / `PUT /skills/{name}`, gated on
    // `skill:write`.
    registry.register(Arc::new(CreateSkillTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(EditSkillTool {
        store: store.clone(),
    }));
    if let Some(fetcher) = fetcher {
        registry.register(Arc::new(FetchUrlTool::new(fetcher.clone())));
    }
    // Pure HTML transforms (SOUL §27): `html_to_markdown` and `extract_html` perform
    // no egress and read no resource, so they need no backend and no capability —
    // always registered. They back the `HtmlToMarkdown` / `ExtractHtml` automation
    // actions and are equally available as chat tools (e.g. clean up HTML the model
    // already holds, or pull a field out by CSS selector).
    registry.register(Arc::new(HtmlToMarkdownTool));
    registry.register(Arc::new(ExtractHtmlTool));
    // `run_javascript` (SOUL §11): the Boa JS sandbox exposed as a tool — exact
    // arithmetic / JSON + string shaping, plus `catalerum.callTool(name, args)`
    // for chaining registry tools with exact logic. Each nested call re-dispatches
    // through the registry the outer call arrived from (`ToolContext::registry`)
    // under the caller's own context, so it passes the identical deny-by-default
    // capability check + tool guard a direct call would — the tool itself stays
    // ungated because it confers no authority of its own. The wall-clock backstop
    // is raised from the 5s pure-transform default to leave room for nested tool
    // I/O; runaway JS is still cut by the loop/recursion/stack limits.
    registry.register(Arc::new(RunJavascriptTool {
        runner: Arc::new(ScriptCodeRunner::new().with_js_limits(JsLimits {
            timeout: std::time::Duration::from_secs(240),
            ..JsLimits::default()
        })),
    }));
    if let Some(search) = search {
        // Grab the vector store before `search` is moved into the search tool — the
        // index tool needs it for the de-index (delete-by-key) path.
        let vector = search.vector.clone();
        // `search_emails` (SOUL §28) — semantic email search, gated on `email:read`
        // (its own sensitive domain); registered alongside `search_semantic`, both
        // needing the same embedder + vector index ([qdrant]).
        registry.register(Arc::new(SearchEmailsTool {
            search: search.clone(),
            store: store.clone(),
        }));
        registry.register(Arc::new(SearchSemanticTool {
            search,
            store: store.clone(),
        }));
        // `index_document` (SOUL §6.4/§10) — enqueue the embed pipeline for a note /
        // object / memory so it becomes semantically searchable. Gated on the same
        // `[qdrant]` toggle as the search tools: indexing into an absent vector store
        // is a no-op the worker couldn't serve. The `IndexDocument` automation action
        // dispatches through this tool.
        registry.register(Arc::new(IndexDocumentTool {
            store: store.clone(),
            vector,
        }));
        // `reindex_objects` — the bulk companion: (re)index every file under a
        // bucket/prefix in one call (e.g. a whole wiki folder just copied in).
        registry.register(Arc::new(ReindexObjectsTool {
            store: store.clone(),
        }));
    }
    if let Some(graph) = graph {
        registry.register(Arc::new(QueryGraphTool { graph }));
    }
    // `run_command` is registered only when an executor is configured ([exec]);
    // even then it requires `exec:run`, which no base role holds (a protected
    // scope, §19/§20) — so it is doubly deny-by-default. When the per-workspace
    // sandbox is enabled it runs *inside* the workspace's sandbox; otherwise it
    // uses the per-call executor backend.
    if executor.is_some() || sandbox.is_some() {
        registry.register(Arc::new(RunCommandTool { executor, sandbox }));
    }
    registry
}

/// Per-user default-provider resolver backed by the `search_settings` store
/// (SOUL §7/§13). Powers [`WebSearchTool`]'s fallback when the model omits
/// `provider`: it reads the caller's stored preference, transparently yielding
/// `None` (→ the router's configured default) for an agent (non-user) caller, an
/// unset preference, or a store error (best-effort — a settings hiccup must not
/// fail a search).
struct StoreSearchDefaults {
    repo: SearchSettingsRepo,
}

#[async_trait]
impl SearchDefaults for StoreSearchDefaults {
    async fn default_provider(
        &self,
        workspace_id: WorkspaceId,
        user_id: Option<UserId>,
    ) -> Option<String> {
        let user_id = user_id?;
        self.repo
            .get(workspace_id, user_id)
            .await
            .ok()
            .and_then(|s| s.default_provider)
    }
}

/// Register the `web_search` tool (SOUL §27), backed by the configured searcher
/// (the `MultiSearcher` over the enabled providers) and the per-user
/// default-provider resolver. Registered post-`build_registry` from [`AppState`],
/// like `notify`, so the searcher need not thread through `build_registry`'s
/// signature or its many call sites. Gated on `web:search` by the tool itself.
///
/// [`AppState`]: crate::state::AppState
pub(crate) fn register_web_search_tool(
    registry: &mut ToolRegistry,
    store: &Store,
    searcher: Arc<dyn WebSearcher>,
) {
    let defaults = Arc::new(StoreSearchDefaults {
        repo: store.search_settings(),
    });
    registry.register(Arc::new(
        WebSearchTool::new(searcher).with_defaults(defaults),
    ));
}

/// The services `search_semantic` needs: an [`Embedder`] to vectorise the query
/// and a [`VectorStore`] to search (SOUL §6.4/§6.5). Built only when a vector
/// index is configured (`[qdrant].enabled`); cloning is cheap (the embedder is
/// `Arc`-shared, the store a thin client).
#[derive(Clone)]
pub(crate) struct SemanticSearch {
    pub(crate) embedder: Arc<dyn Embedder>,
    pub(crate) vector: VectorStore,
    pub(crate) embed_model: String,
}

/// Compile-time application graph facade. Neo4j remains a first-class derived
/// backend; when it is disabled, reads are assembled directly from the native
/// relational repositories (notes/tags/links), so the graph surface never
/// disappears in the all-in-one image.
#[derive(Clone)]
pub(crate) enum GraphQuery {
    Neo4j(GraphStore),
    Database(Store),
}

impl GraphQuery {
    pub(crate) async fn healthz(&self) -> std::result::Result<(), String> {
        match self {
            Self::Neo4j(graph) => graph.healthz().await.map_err(|e| e.to_string()),
            Self::Database(store) => store.ping().await.map_err(|e| e.to_string()),
        }
    }
    pub(crate) async fn related_notes(
        &self,
        workspace_id: WorkspaceId,
        note_id: NoteId,
        limit: i64,
    ) -> std::result::Result<Vec<RelatedNote>, String> {
        match self {
            Self::Neo4j(graph) => graph
                .related_notes(workspace_id, note_id, limit)
                .await
                .map_err(|e| e.to_string()),
            Self::Database(store) => {
                let notes = store
                    .notes()
                    .list_by_workspace(workspace_id, catalerum_store::DEFAULT_NOTE_LIMIT)
                    .await
                    .map_err(|e| e.to_string())?;
                let source = notes
                    .iter()
                    .find(|note| note.id == note_id)
                    .ok_or_else(|| "note not found".to_string())?;
                let wanted: HashSet<_> = source.tags.iter().map(|t| t.to_lowercase()).collect();
                let mut rows: Vec<_> = notes
                    .into_iter()
                    .filter(|note| note.id != note_id)
                    .filter_map(|note| {
                        let shared = note
                            .tags
                            .iter()
                            .filter(|tag| wanted.contains(&tag.to_lowercase()))
                            .count() as i64;
                        (shared > 0).then(|| RelatedNote {
                            note_id: note.id.to_string(),
                            title: Some(note.title),
                            shared_topics: shared,
                        })
                    })
                    .collect();
                rows.sort_by_key(|r| std::cmp::Reverse(r.shared_topics));
                rows.truncate(limit.max(1) as usize);
                Ok(rows)
            }
        }
    }

    pub(crate) async fn notes_by_topic(
        &self,
        workspace_id: WorkspaceId,
        topic: &str,
        limit: i64,
    ) -> std::result::Result<Vec<NoteHit>, String> {
        match self {
            Self::Neo4j(graph) => graph
                .notes_by_topic(workspace_id, topic, limit)
                .await
                .map_err(|e| e.to_string()),
            Self::Database(store) => Ok(store
                .notes()
                .list_by_workspace(workspace_id, catalerum_store::DEFAULT_NOTE_LIMIT)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|note| note.tags.iter().any(|tag| tag.eq_ignore_ascii_case(topic)))
                .take(limit.max(1) as usize)
                .map(|note| NoteHit {
                    note_id: note.id.to_string(),
                    title: Some(note.title),
                })
                .collect()),
        }
    }

    pub(crate) async fn load_workspace_facts(
        &self,
        workspace_id: WorkspaceId,
        node_cap: i64,
        edge_cap: i64,
    ) -> std::result::Result<WorkspaceFacts, String> {
        match self {
            Self::Neo4j(graph) => graph
                .load_workspace_facts(workspace_id, node_cap, edge_cap)
                .await
                .map_err(|e| e.to_string()),
            Self::Database(store) => {
                let notes = store
                    .notes()
                    .list_by_workspace(workspace_id, node_cap.max(1))
                    .await
                    .map_err(|e| e.to_string())?;
                let mut facts = WorkspaceFacts::default();
                let mut topics = HashSet::new();
                for note in notes {
                    let id = note.id.to_string();
                    facts.nodes.push((id.clone(), "Note".to_string()));
                    facts
                        .props
                        .push((id.clone(), "title".to_string(), note.title));
                    for tag in note.tags {
                        let topic_id = format!("topic:{}", tag.to_lowercase());
                        if topics.insert(topic_id.clone()) {
                            facts.nodes.push((topic_id.clone(), "Topic".to_string()));
                            facts.props.push((
                                topic_id.clone(),
                                "display_name".to_string(),
                                tag.clone(),
                            ));
                        }
                        facts.props.push((id.clone(), "tags".to_string(), tag));
                        facts
                            .edges
                            .push((id.clone(), "REFERENCES".to_string(), topic_id));
                    }
                }
                let nodes_over = facts.nodes.len() > node_cap.max(1) as usize;
                let edges_over = facts.edges.len() > edge_cap.max(1) as usize;
                facts.nodes.truncate(node_cap.max(1) as usize);
                facts.edges.truncate(edge_cap.max(1) as usize);
                facts.truncated = nodes_over || edges_over;
                Ok(facts)
            }
        }
    }
}

// Tool implementations, split by domain; everything is re-exported so
// `crate::tools::X` paths (and `use super::*` in the modules) keep working.
mod agent_profiles;
mod app_data;
mod ask_user;
mod audio;
mod automations;
mod calendar;
mod chat;
mod computer;
mod connections;
mod discovery;
mod email;
mod kanban;
mod links;
mod mcp;
mod memory;
mod notes;
mod ocr;
mod query;
mod run_code;
mod search;
mod skills;
mod storage;
mod time;
mod ui;
mod util;
pub(crate) use self::agent_profiles::*;
pub(crate) use self::app_data::*;
pub(crate) use self::ask_user::*;
pub(crate) use self::audio::*;
pub(crate) use self::automations::*;
pub(crate) use self::calendar::*;
pub(crate) use self::chat::*;
pub(crate) use self::computer::*;
pub(crate) use self::connections::*;
pub(crate) use self::discovery::*;
pub(crate) use self::email::*;
pub(crate) use self::kanban::*;
pub(crate) use self::links::*;
pub(crate) use self::mcp::*;
pub(crate) use self::memory::*;
pub(crate) use self::notes::*;
pub(crate) use self::ocr::*;
pub(crate) use self::query::*;
pub(crate) use self::run_code::*;
pub(crate) use self::search::*;
pub(crate) use self::skills::*;
pub(crate) use self::storage::*;
pub(crate) use self::time::*;
pub(crate) use self::ui::*;
pub(crate) use self::util::*;

#[cfg(test)]
mod tests;
