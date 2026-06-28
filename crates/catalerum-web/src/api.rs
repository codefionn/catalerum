//! API contract + auth for the catalerum workbench (SOUL §12).
//!
//! Defines the JSON shapes exchanged with `catalerum-api` over the chat
//! WebSocket (`/ws/chat`) and the small REST surface used for dev login, plus
//! the dev auth-token plumbing (URL `?token=` or `localStorage`).
//!
//! ## WS contract this crate codes against
//!
//! Outbound (client → server), one JSON text frame per user turn (matches
//! `catalerum_api::routes::ws::ClientFrame`):
//! ```json
//! { "conversation_id": "<uuid>", "content": "hello" }
//! ```
//!
//! Inbound (server → client), a stream of JSON text frames. The API
//! (`catalerum_api::routes::ws::ServerFrame`) wraps every model event in a
//! `token` envelope and ends the turn with a `message_done` frame:
//! ```json
//! { "type": "token", "event": { "type": "text_delta", "text": "par" } }
//! { "type": "token", "event": { "type": "text_delta", "text": "tial" } }
//! { "type": "token", "event": { "type": "done", "finish_reason": "stop" } }
//! { "type": "message_done", "message_id": "<uuid>", "conversation_id": "<uuid>", "content": "partial" }
//! ```
//! A turn-level failure arrives as `{ "type": "error", "message": "…" }` and the
//! socket then closes.
//!
//! The inner `event` object mirrors `catalerum_core::stream::StreamEvent`
//! (`#[serde(tag = "type", rename_all = "snake_case")]`): `text_delta`,
//! `tool_call_delta`, `done`, `error`. The decoder is intentionally lenient
//! about a couple of field-name spellings so a benign contract drift degrades
//! gracefully instead of killing the stream. See [`ServerFrame`].

use serde::{Deserialize, Serialize};

/// Fallback API base URL. Used verbatim on native/unit-test builds and in local
/// `trunk serve` dev (served on `localhost`, where the dev server proxies `/ws`
/// etc. and cross-origin to `:8787` also works — the API's CORS is permissive).
/// Production resolves the base at runtime via [`api_base`] instead.
pub const API_BASE: &str = "http://localhost:8787";

/// Resolve the API origin at runtime. In the browser the SPA talks to the API on
/// the **same domain prefixed with `api.`** — so the app served at
/// `catalerum.example` calls `https://api.catalerum.example`. Any localhost dev
/// host (and every non-wasm / unit-test build) falls back to [`API_BASE`], which
/// keeps the `trunk serve` proxy and the pure URL-builder tests working.
#[must_use]
pub fn api_base() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(win) = web_sys::window() {
            let loc = win.location();
            if let Some(configured) = option_env!("CATALERUM_WEB_API_BASE") {
                if configured.starts_with('/') {
                    let proto = loc.protocol().unwrap_or_else(|_| "http:".to_string());
                    let host = loc.host().unwrap_or_default();
                    return format!("{proto}//{host}{}", configured.trim_end_matches('/'));
                }
                if !configured.trim().is_empty() {
                    return configured.trim_end_matches('/').to_string();
                }
            }
            let host = loc.hostname().unwrap_or_default();
            if !host.is_empty() && host != "localhost" && host != "127.0.0.1" && host != "[::1]" {
                let proto = loc.protocol().unwrap_or_else(|_| "https:".to_string());
                return format!("{proto}//api.{host}");
            }
        }
    }
    API_BASE.to_string()
}

/// Path of the streaming chat WebSocket endpoint on the API.
pub const WS_CHAT_PATH: &str = "/ws/chat";
pub const AUTH_SETUP_PATH: &str = "/auth/setup";
pub const AUTH_PASSWORD_PATH: &str = "/auth/password";
pub const LLMLEAF_PATH: &str = "/llmleaf";
pub const USERS_PATH: &str = "/users";

/// Path of the speech-synthesis WebSocket endpoint (SOUL §7/§12) — the voice
/// overlay's TTS channel: a JSON speak frame in, `speech_start` + binary audio
/// chunks + `speech_end` out.
pub const WS_SPEECH_PATH: &str = "/ws/speech";

/// REST path: list / create calendar connections.
pub const CONNECTIONS_PATH: &str = "/connections";

/// REST path: list this workspace's calendars.
pub const CALENDARS_PATH: &str = "/calendars";

/// REST path: list events (optionally date/calendar-filtered).
pub const EVENTS_PATH: &str = "/events";

/// REST path: list / create markdown notes (and `/notes/{id}` for one note).
pub const NOTES_PATH: &str = "/notes";

/// REST path: list emerged UIs (and `/uis/{id}` for one full definition).
pub const UIS_PATH: &str = "/uis";

/// REST path: storage blob backend — list (`?prefix=`), and
/// `/storage/objects/{key}` for download / upload / delete.
pub const STORAGE_OBJECTS_PATH: &str = "/storage/objects";

/// REST path: the catalogued-objects listing (Postgres truth, `?prefix=`).
pub const STORAGE_CATALOGUE_PATH: &str = "/storage/catalogue";

/// REST path: the workspace's storage backends ("stores") — list / create, and
/// `/storage/stores/{name}` to delete a runtime one (SOUL §9).
pub const STORAGE_STORES_PATH: &str = "/storage/stores";

/// REST path: labels on stored files & directories — list (`?store=&prefix=` or
/// `?label=`) / create, and `/storage/labels/{id}` to delete one (SOUL §9).
pub const STORAGE_LABELS_PATH: &str = "/storage/labels";

/// REST path: list / create skills (and `/skills/{name}` for one skill).
pub const SKILLS_PATH: &str = "/skills";

/// Agent-profile management routes (SOUL §19/§25).
pub const AGENT_PROFILES_PATH: &str = "/agent-profiles";

/// REST path: the agent tool catalog (the Profiles tools checklist). Read-only,
/// global/static (workspace-independent). The model dropdown reuses the existing
/// `LLM_MODELS_PATH` (`/llm-models`) from the LLM-settings surface.
pub const TOOLS_PATH: &str = "/tools";

/// REST path: list / create automations (and `/automations/{name}`,
/// `/automations/{name}/enabled`, `/automations/{name}/runs` for one).
pub const AUTOMATIONS_PATH: &str = "/automations";

/// REST path: the automation node-type catalog (`/automations/node-types`) and its
/// semantic search (`/automations/node-types/search?q=…&limit=N`).
pub const AUTOMATION_NODE_TYPES_PATH: &str = "/automations/node-types";

/// REST path: fire a named-signal `trigger` automation on demand
/// (`/triggers/{name}`, SOUL §11).
pub const TRIGGERS_PATH: &str = "/triggers";

/// REST path: list / create grants (and `/grants/{id}` for one). Admin-only.
pub const GRANTS_PATH: &str = "/grants";

/// REST path: list conversations (and `/conversations/{id}/messages` for one's
/// transcript).
pub const CONVERSATIONS_PATH: &str = "/conversations";

/// REST path: list the workspace's mailboxes (SOUL §28).
pub const MAILBOXES_PATH: &str = "/mailboxes";

/// REST path: list emails (filtered) and `/emails/{id}` for one's detail.
pub const EMAILS_PATH: &str = "/emails";

/// REST path: list + create email sources (connections) (SOUL §28). Singular
/// `email` so it never collides with the `/emails/{id}` detail route.
pub const EMAIL_CONNECTIONS_PATH: &str = "/email/connections";

/// REST path: this workspace's external Postgres connections (SOUL §11/§19) —
/// the sources a `collect_sql` trigger polls and `sql_query` reads/writes.
pub const DB_CONNECTIONS_PATH: &str = "/db/connections";

/// REST path: fetch a web page as Markdown/HTML/text (SOUL §27).
pub const FETCH_PATH: &str = "/fetch";

/// REST path: list / create Kanban boards (and `/boards/{id}`,
/// `/boards/{id}/tasks`).
pub const BOARDS_PATH: &str = "/boards";

/// REST path: per-task actions — `/tasks/{id}/move`, `/tasks/{id}/status`.
pub const TASKS_PATH: &str = "/tasks";

/// REST path: per-column actions — `PUT`/`DELETE /columns/{id}`.
pub const COLUMNS_PATH: &str = "/columns";

/// REST path: list / create memories (and `/memories/{id}` for one).
pub const MEMORIES_PATH: &str = "/memories";

/// REST path: list active terminal sessions (SOUL §20).
pub const TERMINAL_SESSIONS_PATH: &str = "/terminals/sessions";

/// A live terminal session (SOUL §20) — the picker for the read-only pane.
/// Mirrors the API's `TerminalSession` (extra fields ignored).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TerminalSession {
    pub id: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default)]
    pub status: String,
}

/// REST path: the caller's profile (`GET`/`PUT`, SOUL §22).
pub const PROFILE_PATH: &str = "/profile";

/// REST path: run a safe Datalog program over the derived graph (SOUL §6.3).
pub const GRAPH_QUERY_PATH: &str = "/graph/query";

/// REST path: the workspaces the authenticated user is a member of (SOUL §18).
pub const WORKSPACES_PATH: &str = "/workspaces";

/// REST path: switch the session to another workspace (SOUL §18).
pub const AUTH_SWITCH_PATH: &str = "/auth/switch";

/// REST path: exchange a one-time handoff code (the `?code=` the magic-link /
/// SSO browser login redirects here with) for the real session bearer (SOUL §18).
pub const AUTH_EXCHANGE_PATH: &str = "/auth/exchange";

/// REST path: the caller's organisations + the workspaces they can see in each,
/// plus org create / members / policy / workspace administration (SOUL §18).
pub const ORGANISATIONS_PATH: &str = "/organisations";

/// REST path: server version + LLM gateway config + service health (SOUL §12).
pub const STATUS_PATH: &str = "/status";
pub const LOGIN_STATUS_PATH: &str = "/status/login";

/// REST path: the caller's per-user LLM model/voice selections (`GET`/`PUT`,
/// SOUL §7/§13).
pub const LLM_SETTINGS_PATH: &str = "/llm-settings";

/// REST path: the gateway's full model catalog for autocomplete (`?search=`).
pub const LLM_MODELS_PATH: &str = "/llm-models";

/// REST path: a speech model's voice list for autocomplete (`?model=`).
pub const LLM_VOICES_PATH: &str = "/llm-voices";

/// REST path: transcribe a recorded-audio body to text (`POST`, SOUL §7) — the
/// chat composer's microphone dictation.
pub const AUDIO_TRANSCRIBE_PATH: &str = "/audio/transcriptions";

/// REST path: the caller's per-user default web-search provider (`GET`/`PUT`,
/// SOUL §27/§13).
pub const SEARCH_SETTINGS_PATH: &str = "/search-settings";

/// REST path: the web-search provider catalog (which engines exist, which are
/// configured, and the caller's default) (SOUL §27).
pub const SEARCH_PROVIDERS_PATH: &str = "/search-providers";

/// REST path: the caller's per-user default files store (`GET`/`PUT`, SOUL §9/§13).
pub const STORAGE_SETTINGS_PATH: &str = "/storage-settings";

/// REST path: the caller's quick-start/onboarding state (`GET`, SOUL §12).
pub const ONBOARDING_STATE_PATH: &str = "/onboarding/state";

/// REST path: one turn of the onboarding personalization chat (`POST`, SOUL §22/§23).
pub const ONBOARDING_PERSONALIZE_PATH: &str = "/onboarding/personalize";

/// REST path: mark the quick-start finished (`POST`, stamps the profile sentinel).
pub const ONBOARDING_COMPLETE_PATH: &str = "/onboarding/complete";

/// REST path: list/create API-key bearer tokens; `/tokens/{id}` to revoke (SOUL §18).
pub const TOKENS_PATH: &str = "/tokens";

/// REST path: the workspace's scripted MCP endpoints — list here,
/// `/mcp-endpoints/{id}/token` to mint a shareable scoped URL (SOUL §30).
pub const MCP_ENDPOINTS_PATH: &str = "/mcp-endpoints";

/// Serve path of the workspace's **main** MCP endpoint (`POST /mcp` on the API
/// origin, workspace bearer token) — the URL an external agent connects to.
pub const MCP_SERVE_PATH: &str = "/mcp";

/// REST path: the workspace's **external** MCP servers — catalerum as an MCP
/// *client* connecting out (SOUL §26). List here; `/mcp-servers/{name}` to
/// update/delete one. Workspace-admin gated (lifecycle writes).
pub const MCP_SERVERS_PATH: &str = "/mcp-servers";

/// REST path: the workspace's enrolled **computer agents** — installed daemons on
/// servers/desktops the LLM drives (SOUL §19/§20). List/enroll here;
/// `/computer-agents/{id}` to revoke. Workspace-admin gated.
pub const COMPUTER_AGENTS_PATH: &str = "/computer-agents";

/// `localStorage` key under which the session/magic token is cached.
pub const TOKEN_STORAGE_KEY: &str = "catalerum_token";

/// One user turn sent to the server over the chat WebSocket.
///
/// Serializes to `{"conversation_id": …, "content": …}` for an ordinary turn; a
/// regenerate adds `"regenerate_from": "<message id>"` (the field is omitted
/// otherwise, so the common shape is unchanged).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientChatMessage {
    /// Conversation this turn belongs to. Kept as a `String` (the API IDs are
    /// UUID strings) so the web crate stays free of a `catalerum-core` dep.
    pub conversation_id: String,
    /// The user's message text.
    pub content: String,
    /// Client-generated idempotency id for an ordinary user turn. Retries reuse
    /// this id, so the server can attach to/replay the original turn without
    /// inserting the message or running the model twice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_id: Option<String>,
    /// Regenerate anchor (SOUL §12): the id of an existing **user** message to
    /// re-answer. The server drops the transcript tail after it and runs the loop
    /// against the history ending there, instead of persisting `content` as a new
    /// turn. `None` (and omitted on the wire) for an ordinary turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regenerate_from: Option<String>,
    /// File/image references for this turn (SOUL §9/§12): each file is uploaded to
    /// the files store first (`PUT /storage/objects/{key}`), then only the reference
    /// rides here — never the bytes. Empty (and omitted on the wire) for a plain
    /// text turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// A `/<skill>` composer invocation (SOUL §12/§23): the skill name whose
    /// runbook the server snapshots onto this message for the model — the UI
    /// (and the stored transcript) keep only the typed `content`. `None` (and
    /// omitted on the wire) for an ordinary turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// Structured `ask_user` form answers (SOUL §7/§12), sent when this turn is
    /// the reply to a pending question form. `content` carries the same answers
    /// flattened to prose (what the model reads); this is the durable structured
    /// record the server stamps onto the resolved question row. Empty (and
    /// omitted on the wire) for an ordinary turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<Answer>,
    /// Whether this turn came from the hands-free conversation overlay. The
    /// server uses this only to select speech-friendly response guidance; typed
    /// chat turns omit it and retain the normal Markdown-rich response format.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conversation_mode: bool,
}

impl ClientChatMessage {
    /// Build a new outbound chat turn.
    #[must_use]
    pub fn new(conversation_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            content: content.into(),
            user_message_id: None,
            regenerate_from: None,
            attachments: Vec::new(),
            skill: None,
            answers: Vec::new(),
            conversation_mode: false,
        }
    }

    /// Assign the stable client-generated id used for idempotent delivery.
    #[must_use]
    pub fn with_user_message_id(mut self, id: impl Into<String>) -> Self {
        self.user_message_id = Some(id.into());
        self
    }

    /// Attach uploaded file references to this turn (builder style).
    #[must_use]
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Mark this turn as a `/<skill>` invocation (builder style): the server
    /// attaches the named skill's runbook for the model (SOUL §12/§23).
    #[must_use]
    pub fn with_skill(mut self, skill: Option<String>) -> Self {
        self.skill = skill;
        self
    }

    /// Attach the structured `ask_user` form answers this turn replies with
    /// (builder style, SOUL §7/§12): the server stamps them onto the pending
    /// question row the turn resolves.
    #[must_use]
    pub fn with_answers(mut self, answers: Vec<Answer>) -> Self {
        self.answers = answers;
        self
    }

    /// Ask the server for a short, plain-text reply suitable for spoken
    /// hands-free conversation.
    #[must_use]
    pub fn with_conversation_mode(mut self, conversation_mode: bool) -> Self {
        self.conversation_mode = conversation_mode;
        self
    }

    /// Build an outbound turn that **regenerates** from an existing user message:
    /// the server re-answers `from_message_id` after dropping everything that came
    /// after it. `content` carries the anchor's text (the server reads the stored
    /// message, so it is advisory only).
    #[must_use]
    pub fn regenerate(
        conversation_id: impl Into<String>,
        content: impl Into<String>,
        from_message_id: impl Into<String>,
    ) -> Self {
        Self {
            conversation_id: conversation_id.into(),
            content: content.into(),
            user_message_id: None,
            regenerate_from: Some(from_message_id.into()),
            attachments: Vec::new(),
            skill: None,
            answers: Vec::new(),
            conversation_mode: false,
        }
    }
}

/// One question in an `ask_user` form (SOUL §7/§12). Mirrors
/// `catalerum_core::ask::Question` (this crate has no core dep) — decoded from a
/// [`ServerFrame::QuestionRequest`] and rendered as a choice / free-text field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    /// Stable key the answer is returned under.
    #[serde(default)]
    pub id: String,
    /// The question text shown to the user.
    #[serde(default)]
    pub text: String,
    /// Suggested answers to choose from. Empty = a pure free-text question.
    #[serde(default)]
    pub options: Vec<String>,
    /// Multiple-choice (checkboxes) when `true`; single-choice (radios) when `false`.
    #[serde(default)]
    pub multiple: bool,
    /// Whether a typed free-text reply is accepted alongside the options.
    #[serde(default)]
    pub allow_text: bool,
}

impl Question {
    /// Whether a free-text reply is accepted — explicit, or implied by having no
    /// options to pick from.
    #[must_use]
    pub fn accepts_text(&self) -> bool {
        self.allow_text || self.options.is_empty()
    }
}

/// The user's answer to one [`Question`]. Mirrors `catalerum_core::ask::Answer`;
/// serialized back to the server inside a `{ question_id, answers }` reply frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// The [`Question::id`] this answers.
    pub id: String,
    /// The option(s) the user selected (empty when they answered with free text).
    #[serde(default)]
    pub selected: Vec<String>,
    /// The free text the user typed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The unresolved `ask_user` form for a conversation, fetched on load so a question
/// survives a reload/reconnect (SOUL §7/§12). Mirrors the subset of
/// `catalerum_core::model::PendingQuestion` the client renders — the server's other
/// fields (ids, timestamps) are ignored on decode.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PendingQuestion {
    #[serde(default)]
    pub questions: Vec<Question>,
}

/// One entry of `GET /conversations/{id}/questions` — an `ask_user` form the
/// thread asked, with the structured answers the user gave (SOUL §7/§12).
/// `answers` is `None` while the form is pending or when it was superseded
/// unanswered. The transcript replay correlates each entry to its `ask_user`
/// tool call via `id` (the `pending_question_id` in the call's result).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ConversationQuestion {
    pub id: String,
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub answers: Option<Vec<Answer>>,
}

/// The unresolved guard-deferred tool call for a conversation, fetched on load so
/// an approval prompt survives a reload / reconnect / restart (SOUL §19). Mirrors
/// the subset of `catalerum_core::model::PendingApproval` the client renders.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PendingApproval {
    /// The pending-approval id, echoed back in the `{approval_id, approved}` reply.
    pub id: String,
    /// The tool awaiting approval.
    #[serde(default)]
    pub tool: String,
    /// Its JSON arguments (shown in the prompt).
    #[serde(default)]
    pub arguments: serde_json::Value,
    /// Why the guard escalated to the user.
    #[serde(default)]
    pub reason: String,
}

/// One inbound frame from the server over the chat WebSocket.
///
/// Matches `catalerum_api::routes::ws::ServerFrame`: the API forwards each model
/// [`StreamEvent`] wrapped in a [`ServerFrame::Token`] envelope, ends the turn
/// with a [`ServerFrame::MessageDone`], and reports turn-level failures as
/// [`ServerFrame::Error`]. Unknown tags decode to [`ServerFrame::Unknown`]
/// rather than failing the whole frame, so a contract drift degrades gracefully
/// instead of killing the stream.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// A streamed model event, nested under `event`. This is the API's actual
    /// per-delta shape: `{"type":"token","event":{"type":"text_delta",…}}`.
    Token {
        /// The inner model event (text/tool-call delta, the stream's own
        /// `done`, or a stream-level `error`).
        event: StreamEvent,
    },

    /// The turn is complete and the assistant message was persisted. The API
    /// also includes the full `content`; the transcript is already assembled
    /// from the `text_delta`s, but the voice overlay speaks `content` — it is
    /// exactly the *final* reply, never an intermediate tool-round fragment.
    MessageDone {
        #[serde(default)]
        message_id: Option<String>,
        /// The anchoring user message of this exchange. The chat panel backfills it
        /// onto the just-sent user line so its "regenerate" control can target it.
        /// Absent on older servers.
        #[serde(default)]
        user_message_id: Option<String>,
        #[serde(default)]
        conversation_id: Option<String>,
        #[serde(default)]
        content: Option<String>,
        /// The server synthesized this terminal because its replay buffer was
        /// unavailable; replace the partial transcript from durable history.
        #[serde(default)]
        reconcile: bool,
        /// `true` iff the agent stopped at its tool-use iteration cap — the reply
        /// is a best-effort partial, so the UI appends a "cut off" note.
        #[serde(default)]
        truncated: bool,
        /// `true` iff the user stopped the turn (the Stop button) — the reply is
        /// a deliberate partial, and any still-queued (not yet placed) user
        /// messages were discarded server-side, so the UI returns them to the
        /// composer. Absent on older servers.
        #[serde(default)]
        stopped: bool,
        /// The LLM cost (USD) for this turn, when the backend reported one — shown
        /// as a per-turn cost readout. Absent when unknown.
        #[serde(default)]
        cost_usd: Option<f64>,
        /// Prompt (input) tokens for the whole exchange, when usage was reported.
        /// Feeds the per-turn token tooltip under the bubble. Absent on older
        /// servers / when usage is unknown.
        #[serde(default)]
        prompt_tokens: Option<u32>,
        /// Completion (output) tokens for the whole exchange. Absent when unknown.
        #[serde(default)]
        completion_tokens: Option<u32>,
        /// Total tokens (prompt + completion) for the whole exchange. Absent when
        /// unknown.
        #[serde(default)]
        total_tokens: Option<u32>,
        /// Prompt tokens served from the provider's cache (a cache read/hit). Absent
        /// when usage is unknown; `Some(0)` when reported with no cache reads.
        #[serde(default)]
        cached_tokens: Option<u32>,
        /// Prompt tokens written to the provider's cache (a cache write/creation).
        /// Same presence semantics as `cached_tokens`.
        #[serde(default)]
        cache_creation_tokens: Option<u32>,
    },

    /// A turn-level error; the socket closes after this frame.
    Error {
        #[serde(default, alias = "error")]
        message: String,
    },

    /// An emerged UI the assistant created/updated this turn, to mount inline.
    /// The frame also carries the full `definition`, but the client re-fetches it
    /// by id, so only `ui_id` + `version` are decoded — `version` busts the inline
    /// mount so a *re-present*/edit of the same id re-fetches the fresh definition.
    UiArtifact {
        #[serde(default)]
        ui_id: String,
        #[serde(default)]
        version: i64,
    },

    /// A guarded tool call (SOUL §19) needs the user's OK before it runs. The turn
    /// is paused; the client shows an approve/reject prompt and replies with an
    /// `{ approval_id, approved }` frame carrying this `id`.
    ApprovalRequest {
        #[serde(default)]
        id: String,
        #[serde(default)]
        tool: String,
        #[serde(default)]
        arguments: serde_json::Value,
        #[serde(default)]
        reason: String,
    },

    /// An `ask_user` question form (SOUL §7/§12) is paused awaiting the user's
    /// answers. The client renders the form and replies with a
    /// `{ question_id, answers }` frame carrying this `id` to resume the turn.
    QuestionRequest {
        #[serde(default)]
        id: String,
        #[serde(default)]
        questions: Vec<Question>,
    },

    /// A user message was persisted server-side — the turn's anchor, or a
    /// mid-turn queued message the instant the agent loop placed it. Stamps the
    /// matching optimistic user line with its server id (enabling regenerate)
    /// and clears its "queued" styling.
    UserMessage {
        #[serde(default)]
        message_id: String,
    },

    /// Any tag this client doesn't recognize — kept so decoding never fails.
    #[serde(other)]
    Unknown,
}

/// One inner model event, nested inside a [`ServerFrame::Token`] envelope.
///
/// Mirrors `catalerum_core::stream::StreamEvent`
/// (`#[serde(tag = "type", rename_all = "snake_case")]`). Tolerant of the
/// `delta`/`content` field-name spellings for the text fragment.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A chunk of assistant text to append to the in-flight message.
    TextDelta {
        #[serde(default, alias = "delta", alias = "content")]
        text: String,
    },

    /// A chunk of the model's reasoning ("thinking") for this turn — shown live
    /// as a muted trace, separate from the answer text.
    ReasoningDelta {
        #[serde(default, alias = "delta", alias = "content")]
        text: String,
    },

    /// A fragment of the model's tool-call *request* (assembled server-side).
    /// Surfaced here only so the frame decodes — the UI renders tool calls from
    /// the discrete `tool_call_started`/`tool_result` events below instead (the
    /// per-iteration `index` is not safe to accumulate across rounds client-side).
    ToolCallDelta {
        #[serde(default)]
        index: u32,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        arguments: Option<String>,
    },

    /// A tool call has started executing (the agent loop dispatched it). Drives
    /// the live "running" card.
    ToolCallStarted {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        arguments: String,
    },

    /// A tool call finished — its result (or error) and timing. Resolves the live
    /// card to success/failure.
    ToolResult {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        result: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        truncated: bool,
    },

    /// The stream's own terminal event. The turn really ends on the outer
    /// `message_done`, so this is just informational to the UI.
    Done {
        #[serde(default)]
        finish_reason: Option<String>,
    },

    /// The stream errored mid-turn.
    Error {
        #[serde(default, alias = "error")]
        message: String,
    },

    /// Any tag this client doesn't recognize — kept so decoding never fails.
    #[serde(other)]
    Unknown,
}

impl ServerFrame {
    /// Normalize this frame into a [`StreamUpdate`] the UI reasons about.
    #[must_use]
    pub fn into_update(self) -> StreamUpdate {
        match self {
            ServerFrame::Token { event } => event.into_update(),
            ServerFrame::MessageDone {
                user_message_id,
                truncated,
                stopped,
                cost_usd,
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cached_tokens,
                cache_creation_tokens,
                content,
                reconcile,
                ..
            } => StreamUpdate::Done {
                truncated,
                stopped,
                cost_usd,
                tokens: TurnTokens::from_parts(
                    prompt_tokens,
                    completion_tokens,
                    total_tokens,
                    cached_tokens,
                    cache_creation_tokens,
                ),
                user_message_id,
                content,
                reconcile,
            },
            ServerFrame::UserMessage { message_id } => StreamUpdate::UserPlaced { message_id },
            ServerFrame::Error { message } => StreamUpdate::Error(message),
            ServerFrame::UiArtifact { ui_id, version } => StreamUpdate::Ui { id: ui_id, version },
            ServerFrame::ApprovalRequest {
                id,
                tool,
                arguments,
                reason,
            } => StreamUpdate::ApprovalRequested {
                id,
                tool,
                arguments,
                reason,
            },
            ServerFrame::QuestionRequest { id, questions } => {
                StreamUpdate::QuestionsRequested { id, questions }
            }
            ServerFrame::Unknown => StreamUpdate::Ignore,
        }
    }
}

impl StreamEvent {
    /// Normalize an inner model event into a [`StreamUpdate`].
    #[must_use]
    pub fn into_update(self) -> StreamUpdate {
        match self {
            StreamEvent::TextDelta { text } => StreamUpdate::Append(text),
            StreamEvent::ReasoningDelta { text } => StreamUpdate::Reasoning(text),
            // The inner `done` is informational; the turn ends on the outer
            // `message_done`. Don't finalize early — more frames may follow.
            StreamEvent::Done { .. } => StreamUpdate::Ignore,
            StreamEvent::Error { message } => StreamUpdate::Error(message),
            StreamEvent::ToolCallStarted {
                id,
                name,
                arguments,
            } => StreamUpdate::ToolStarted {
                id,
                name,
                arguments,
            },
            StreamEvent::ToolResult {
                id,
                name,
                result,
                is_error,
                duration_ms,
                truncated,
            } => StreamUpdate::ToolResult {
                id,
                name,
                result,
                is_error,
                duration_ms,
                truncated,
            },
            // Raw tool-call request deltas and unknown tags are not rendered (tool
            // cards come from the discrete started/result events above).
            StreamEvent::ToolCallDelta { .. } | StreamEvent::Unknown => StreamUpdate::Ignore,
        }
    }
}

/// Per-turn token + cache accounting carried on the terminal `message_done`
/// frame, surfaced as a hover readout under the assistant bubble. Every count is
/// for the whole user-message exchange (the agent loop sums it across each
/// tool-call turn). Persisted on the exchange's final assistant message
/// ([`MsgUsage`]) and rehydrated on replay, so the readout survives a reload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TurnTokens {
    /// Prompt (input) tokens.
    pub prompt_tokens: u32,
    /// Completion (output) tokens.
    pub completion_tokens: u32,
    /// Total tokens (prompt + completion).
    pub total_tokens: u32,
    /// Prompt tokens served from the provider's cache (a cache read/hit).
    pub cached_tokens: u32,
    /// Prompt tokens written to the provider's cache (a cache write/creation).
    pub cache_creation_tokens: u32,
}

impl TurnTokens {
    /// Build from the terminal frame's flat optional counts. Returns `None` when
    /// no usage was reported at all (every field absent); otherwise treats an
    /// individual absent field as `0` so a partially-populated frame still shows.
    #[must_use]
    pub fn from_parts(
        prompt: Option<u32>,
        completion: Option<u32>,
        total: Option<u32>,
        cached: Option<u32>,
        cache_creation: Option<u32>,
    ) -> Option<Self> {
        if prompt.is_none()
            && completion.is_none()
            && total.is_none()
            && cached.is_none()
            && cache_creation.is_none()
        {
            return None;
        }
        Some(Self {
            prompt_tokens: prompt.unwrap_or(0),
            completion_tokens: completion.unwrap_or(0),
            total_tokens: total.unwrap_or(0),
            cached_tokens: cached.unwrap_or(0),
            cache_creation_tokens: cache_creation.unwrap_or(0),
        })
    }
}

/// The UI-facing reduction of a [`ServerEvent`].
///
/// Not `Eq` because [`StreamUpdate::Done`] carries an `f64` cost.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamUpdate {
    /// Append this text fragment to the streaming assistant message.
    Append(String),
    /// Append this fragment to the message's reasoning ("thinking") trace,
    /// rendered apart from the answer text.
    Reasoning(String),
    /// The turn is complete; finalize the streaming message. `truncated` is set
    /// when the agent stopped at its tool-use iteration cap (a partial reply);
    /// `stopped` when the user's Stop button ended it (also a partial — and any
    /// still-queued user messages were discarded server-side, so the panel
    /// returns them to the composer); `cost_usd` is the turn's LLM cost when the
    /// backend reported one; `tokens` is the exchange's token + cache accounting
    /// when usage was reported.
    Done {
        truncated: bool,
        stopped: bool,
        cost_usd: Option<f64>,
        tokens: Option<TurnTokens>,
        /// The anchoring user message id, when the server reported it. The chat
        /// panel backfills it onto the just-sent user line so that line's
        /// "regenerate" control can target it. `None` on older servers.
        user_message_id: Option<String>,
        /// The turn's final assistant text, verbatim from the terminal frame.
        /// The transcript is already assembled from the text deltas — this is
        /// for the voice overlay, which speaks exactly the *final* reply (never
        /// an intermediate tool-round fragment). `None` on older servers.
        content: Option<String>,
        /// Whether durable transcript reconciliation is required.
        reconcile: bool,
    },
    /// The stream errored; surface the message and finalize.
    Error(String),
    /// A tool call started executing this turn; attach a "running" card to the
    /// in-flight assistant line, keyed by `id`.
    ToolStarted {
        /// The tool call id (correlates with a later [`StreamUpdate::ToolResult`]).
        id: String,
        /// The tool/function name dispatched.
        name: String,
        /// The JSON-encoded arguments string.
        arguments: String,
    },
    /// A tool call finished; resolve its card to success/failure with timing.
    ToolResult {
        /// The tool call id this answers (matches a prior [`StreamUpdate::ToolStarted`]).
        id: String,
        /// The tool/function name.
        name: String,
        /// The result string (raw JSON / text), possibly wire-truncated.
        result: String,
        /// Whether the call failed (the `result` holds the error payload).
        is_error: bool,
        /// Execution time in milliseconds, when measured.
        duration_ms: Option<u64>,
        /// Whether `result` was truncated for the wire (full text on reload).
        truncated: bool,
    },
    /// The assistant created/updated an emerged UI this turn; mount it on the
    /// in-flight assistant line. Carries the UI id (the renderer fetches the
    /// definition) and its `version` (so an edit/re-present of the same id forces
    /// a fresh re-mount).
    Ui {
        /// The emerged-UI id (UUID string) to mount.
        id: String,
        /// The definition version (monotonic; bumped on every edit/re-present).
        version: i64,
    },
    /// A user message was persisted server-side (the turn's anchor, or a queued
    /// mid-turn message the moment the agent loop placed it). Stamp the matching
    /// optimistic user line with this server id and clear its "queued" styling.
    UserPlaced {
        /// The persisted message's id (UUID string).
        message_id: String,
    },
    /// A guarded tool call is paused awaiting the user's approval (SOUL §19). Show
    /// an inline approve/reject prompt on the in-flight assistant line; the buttons
    /// reply with `{ approval_id: id, approved }` over the socket to resume the turn.
    ApprovalRequested {
        /// Correlation id to echo back in the approval reply.
        id: String,
        /// The tool awaiting approval.
        tool: String,
        /// Its JSON arguments (for the prompt).
        arguments: serde_json::Value,
        /// Why the guard escalated to the user.
        reason: String,
    },
    /// An `ask_user` form is paused awaiting the user's answers (SOUL §7/§12). Show
    /// an inline question form on the in-flight assistant line; on submit the client
    /// replies with `{ question_id: id, answers }` over the socket to resume the turn.
    QuestionsRequested {
        /// Correlation id to echo back in the answer reply.
        id: String,
        /// The questions to render as a form.
        questions: Vec<Question>,
    },
    /// Nothing to render for this frame.
    Ignore,
}

/// Parse one inbound WS text frame into a [`StreamUpdate`].
///
/// Returns [`StreamUpdate::Error`] (rather than panicking) if the frame is not
/// valid JSON for the contract.
#[must_use]
pub fn parse_frame(text: &str) -> StreamUpdate {
    match serde_json::from_str::<ServerFrame>(text) {
        Ok(frame) => frame.into_update(),
        Err(e) => StreamUpdate::Error(format!("malformed server frame: {e}")),
    }
}

/// Extract the transport-level `seq` (stream-entry id) a forwarding server
/// stamps on every frame (SOUL §7/§12), so the client can record its resume
/// cursor without any `ServerFrame` variant needing a field. `None` for a frame
/// with no `seq` (an older server, or a locally-synthesized update).
#[must_use]
pub fn frame_seq(text: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct SeqPeek {
        #[serde(default)]
        seq: Option<String>,
    }
    serde_json::from_str::<SeqPeek>(text)
        .ok()
        .and_then(|p| p.seq)
}

/// The in-flight turn a client should (re)attach to (SOUL §7/§12), mirroring
/// `catalerum-api`'s `GET /conversations/{id}/active_turn`. `null` (→ `None`)
/// when nothing is streaming for the conversation right now.
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveTurn {
    /// The anchoring user message id — the turn key the client attaches with.
    pub user_message_id: String,
}

// ===========================================================================
// Calendar REST contract (SOUL §8, §12 — M2 calendar view + connect form).
//
// These types mirror `catalerum-api`'s calendar routes (mounted at the API
// root: `/connections`, `/calendars`, `/events`). The API serializes the core
// `Connection`/`Calendar`/`Event` projections directly; we re-declare the JSON
// shapes here (rather than depend on `catalerum-core`) so the wasm bundle stays
// lean, exactly as the WS contract above does. All ids are UUID strings; all
// datetimes are RFC 3339 / ISO-8601 UTC.
// ===========================================================================

/// The provider sub-kind chosen in the "Connect calendar" form. Serializes to
/// the snake_case wire token the API's `CalendarProviderKind` expects
/// (`"local" | "caldav" | "webcal"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarProviderKind {
    /// A local directory of `.ics` files (read-only by default).
    Local,
    /// A CalDAV server (RFC 4791 sync-collection + ETags).
    Caldav,
    /// A published `webcal://` / `https://` ICS feed (read-only).
    Webcal,
}

impl CalendarProviderKind {
    /// Human label for the form select.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CalendarProviderKind::Local => "Local .ics directory",
            CalendarProviderKind::Caldav => "CalDAV server",
            CalendarProviderKind::Webcal => "webcal / ICS URL",
        }
    }

    /// The stable wire token (`local` | `caldav` | `webcal`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CalendarProviderKind::Local => "local",
            CalendarProviderKind::Caldav => "caldav",
            CalendarProviderKind::Webcal => "webcal",
        }
    }

    /// Parse a wire token back into a kind (used by the form select handler).
    #[must_use]
    pub fn parse_token(s: &str) -> Option<Self> {
        match s {
            "local" => Some(CalendarProviderKind::Local),
            "caldav" => Some(CalendarProviderKind::Caldav),
            "webcal" => Some(CalendarProviderKind::Webcal),
            _ => None,
        }
    }

    /// Whether this kind takes a directory path (`config.dir`) rather than a URL.
    #[must_use]
    pub fn is_local(self) -> bool {
        matches!(self, CalendarProviderKind::Local)
    }

    /// The required `config` key for this kind (`dir` for local, `base_url`
    /// otherwise) — matches the API's `validate_config`.
    #[must_use]
    pub fn config_key(self) -> &'static str {
        if self.is_local() {
            "dir"
        } else {
            "base_url"
        }
    }
}

/// Request body for `POST /connections` (matches the API's `CreateConnection`).
///
/// `config` carries the per-provider settings: `{ "dir": "…" }` for local,
/// `{ "base_url": "…" }` for caldav/webcal.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CreateConnection {
    /// Provider sub-kind: `local` | `caldav` | `webcal`.
    pub kind: CalendarProviderKind,
    /// Human-readable connection name.
    pub name: String,
    /// Per-provider settings blob.
    pub config: serde_json::Value,
    /// Optional opaque secret-store reference (never plaintext).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

impl CreateConnection {
    /// Build a connection-create body from the form's raw target (a directory
    /// path for `local`, else a URL), placing it under the correct `config` key.
    #[must_use]
    pub fn new(
        kind: CalendarProviderKind,
        name: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        let mut config = serde_json::Map::new();
        config.insert(
            kind.config_key().to_string(),
            serde_json::Value::String(target.into()),
        );
        Self {
            kind,
            name: name.into(),
            config: serde_json::Value::Object(config),
            credentials: None,
        }
    }
}

/// A calendar connection as projected by the API (the core `Connection`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Connection {
    /// Connection id (UUID string).
    pub id: String,
    /// Owning workspace (UUID string).
    pub workspace_id: String,
    /// Abstract connection category (`"calendar"` for calendars).
    pub kind: String,
    /// Human-readable name.
    pub name: String,
    /// Opaque secret-store reference, if any.
    #[serde(default)]
    pub credential_ref: Option<String>,
    /// Last incremental-sync cursor, if any.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Whether an **enabled** Collect automation ingests from this connection
    /// (SOUL §29). `false` ⇒ the source is **dormant** — configured but nothing
    /// will ever collect from it — which the UI flags with an inline "idle"
    /// warning. Defaults to `true` when the field is absent (an older response) so
    /// a missing annotation never raises a false alarm.
    #[serde(default = "default_true")]
    pub collecting: bool,
}

/// serde default for [`Connection::collecting`]: absent ⇒ assume live, so a
/// response that predates the §29 annotation never falsely reads as dormant.
fn default_true() -> bool {
    true
}

/// Response for `POST /connections/{id}/sync` (the API's `SyncEnqueued`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct SyncEnqueued {
    /// The enqueued `job_queue` row id.
    pub job_id: String,
    /// The job kind (always `"sync_calendar"`).
    pub kind: String,
    /// The connection being synced.
    pub connection_id: String,
}

/// A calendar as projected by the API (the core `Calendar`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Calendar {
    /// Calendar id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Owning provider connection, or `None` for a **local** (database-native)
    /// calendar — one with no external source whose events are edited directly.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// Provider-native calendar id (an opaque local key for a local calendar).
    pub external_id: String,
    /// Display name.
    pub name: String,
    /// Whether the calendar is read-only.
    #[serde(default)]
    pub read_only: bool,
}

impl Calendar {
    /// Whether this is a local (database-native) calendar — the kind whose
    /// events the workbench can create / edit / delete directly.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.connection_id.is_none()
    }

    /// Whether the workbench may write events to this calendar: any calendar
    /// that isn't read-only. A provider-backed writable calendar (CalDAV,
    /// Google, Outlook) is edited through the server's write-back seam (SOUL
    /// §8); read-only ones (webcal/ics subscriptions, `.ics` directories) keep
    /// their sync-managed posture.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        !self.read_only
    }
}

/// Request body for `POST /calendars` — create a local (database-native)
/// calendar (matches the API's `CreateCalendar`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateCalendar {
    /// Human-readable calendar name (must be non-empty).
    pub name: String,
}

/// Request body for `POST /events` — create an event on a local calendar
/// (matches the API's `CreateEvent`). Datetimes are RFC 3339 / ISO-8601 UTC.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateEvent {
    /// The local calendar to add the event to.
    pub calendar_id: String,
    /// Event title (must be non-empty).
    pub summary: String,
    /// Start (RFC 3339).
    pub start: String,
    /// End (RFC 3339); must not precede `start`.
    pub end: String,
    /// All-day flag.
    pub all_day: bool,
    /// Optional location (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Optional description / notes (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Category labels / tags (omitted when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// File / image attachments (omitted when empty).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A file or image attached to an event (matches the API's `Attachment`).
/// `url` is an absolute link or a `/storage/objects/{key}` path for an upload;
/// an `image/*` `content_type` renders inline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Absolute URL or workspace storage path the bytes live at.
    pub url: String,
    /// Display filename, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// MIME type, if known (`image/*` renders inline).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// Size in bytes, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Request body for `PUT /events/{id}` — edit an event on a local calendar
/// (matches the API's `UpdateEvent`). A full replacement of the editable fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateEvent {
    /// New title (must be non-empty).
    pub summary: String,
    /// New start (RFC 3339).
    pub start: String,
    /// New end (RFC 3339); must not precede `start`.
    pub end: String,
    /// New all-day flag.
    pub all_day: bool,
    /// New location (omitted when absent → cleared server-side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// New body (omitted when absent → cleared server-side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// New recurrence rule (omitted when absent → cleared server-side). The
    /// edit form has no rrule field, so it sends the stored rule back verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rrule: Option<String>,
    /// New labels (replaces the prior set; empty clears them server-side).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// New attachments (replaces the prior set; empty clears them server-side).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A typed entity pointer on an event attendee (the core `EntityRef`). The UI
/// only needs the optional display name, but we keep the shape faithful.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EntityRef {
    /// Owning workspace.
    pub workspace_id: String,
    /// The catalogued entity.
    pub entity_id: String,
    /// Entity kind (`person`/`org`/…).
    pub kind: String,
    /// Denormalized display name, if known.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// An event as projected by the API (the core `Event`).
///
/// Note the timestamp fields are `start` / `end` (RFC 3339), per the core
/// `Event` struct.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Event {
    /// Event id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Owning calendar.
    pub calendar_id: String,
    /// iCalendar UID (stable across edits).
    pub uid: String,
    /// Event start (RFC 3339 / ISO-8601 UTC).
    pub start: String,
    /// Event end (RFC 3339 / ISO-8601 UTC).
    pub end: String,
    /// Whole-day event: covers calendar *dates*, so the grids show it in the
    /// all-day strip rather than as a timed block.
    #[serde(default)]
    pub all_day: bool,
    /// RFC 5545 recurrence rule, if any.
    #[serde(default)]
    pub rrule: Option<String>,
    /// Event title.
    pub summary: String,
    /// Location text, if any.
    #[serde(default)]
    pub location: Option<String>,
    /// Resolved attendees.
    #[serde(default)]
    pub attendees: Vec<EntityRef>,
    /// Free-text description / body.
    #[serde(default)]
    pub body: Option<String>,
    /// Category labels / tags.
    #[serde(default)]
    pub labels: Vec<String>,
    /// File / image attachments.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Provider ETag, if any.
    #[serde(default)]
    pub etag: Option<String>,
    /// iCalendar SEQUENCE.
    #[serde(default)]
    pub sequence: i64,
}

// ===========================================================================
// Notes REST contract (SOUL §21, §12 — M3 markdown notes editor).
//
// Mirrors `catalerum-api`'s notes routes (root-mounted: `/notes`,
// `/notes/{id}`). The API serializes the core `Note` directly; we re-declare the
// JSON shape here (rather than depend on `catalerum-core`) so the wasm bundle
// stays lean, exactly as the calendar contract above does. All ids are UUID
// strings; `updated_at` is an RFC 3339 / ISO-8601 UTC string.
// ===========================================================================

/// The author of a note (the core `Author` sum type). `kind` is `"user"` or
/// `"agent"`; `id` is the referenced principal id.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NoteAuthor {
    /// Author discriminator: `"user"` | `"agent"`.
    pub kind: String,
    /// The referenced user or agent id (UUID string).
    pub id: String,
}

/// A markdown note as projected by the API (the core `Note`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Note {
    /// Note id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Who authored the note (user or agent).
    pub author: NoteAuthor,
    /// Note title.
    pub title: String,
    /// Markdown body.
    #[serde(default)]
    pub markdown: String,
    /// Free-text tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Last edit time (RFC 3339 / ISO-8601 UTC).
    pub updated_at: String,
}

/// Request body for `POST /notes` (matches the API's `CreateNote`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateNote {
    /// Note title (must be non-empty).
    pub title: String,
    /// Markdown body.
    pub markdown: String,
    /// Free-text tags.
    pub tags: Vec<String>,
}

/// Request body for `PUT /notes/{id}` (matches the API's `UpdateNote`). A full
/// replacement of the editable fields; the author is immutable (SOUL §21).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateNote {
    /// New title (must be non-empty).
    pub title: String,
    /// New markdown body.
    pub markdown: String,
    /// New tag set (replaces the existing tags).
    pub tags: Vec<String>,
}

// ===========================================================================
// Storage REST contract (SOUL §9, §12 — M3 object-storage browser).
//
// Mirrors `catalerum-api`'s storage routes (root-mounted: `/storage/catalogue`,
// `/storage/objects/{key}`). [`StorageObject`] is the API's `ObjectView` — a
// catalogued object (Postgres truth) carrying its bucket name + the §10
// extracted-text link. The user-facing `key` never exposes the physical
// `<workspace_id>/…` namespace (SOUL §18). As elsewhere, we re-declare the JSON
// shape here rather than depend on `catalerum-core`, keeping the wasm bundle
// lean. All ids are UUID strings; `last_modified` is RFC 3339 / ISO-8601 UTC.
// ===========================================================================

/// A catalogued storage object as projected by `GET /storage/catalogue`
/// (the API's `routes::storage::ObjectView`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StorageObject {
    /// Catalogue object id (UUID string).
    pub id: String,
    /// The bucket name the object lives in.
    pub bucket: String,
    /// The store (`?store=` selector) the object lives on — used for per-object
    /// download/delete. Defaults to empty (→ the default store) for older payloads.
    #[serde(default)]
    pub store: String,
    /// The user-facing object key (never the physical namespaced key, §18).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Guessed content type, if known.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Backend ETag, if any.
    #[serde(default)]
    pub etag: Option<String>,
    /// Last-modified time (RFC 3339 / ISO-8601 UTC).
    pub last_modified: String,
    /// Content hash (hex), if computed.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The §10 extracted-text document id, present once the object is ingested.
    #[serde(default)]
    pub extracted_text_id: Option<String>,
}

impl StorageObject {
    /// Whether the object's text has been extracted + indexed (§10).
    #[must_use]
    pub fn is_ingested(&self) -> bool {
        self.extracted_text_id.is_some()
    }
}

/// A raw **backend** object as projected by `GET /storage/objects` (the API's
/// `catalerum_core::provider::ObjectMeta`) — the actual files on a store's
/// filesystem, *not* the Postgres catalogue. This is what the Files panel browses
/// as a tree (so a *browse* store's pre-existing on-disk files appear); catalogue
/// rows ([`StorageObject`]) are matched in by `key` to layer the "Indexed" badge.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct BackendObject {
    /// The user-facing object key (a `/`-separated path from the store root).
    pub key: String,
    /// Object size in bytes.
    pub size: u64,
    /// Backend ETag, if any.
    #[serde(default)]
    pub etag: Option<String>,
    /// Guessed content type, if known.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Last-modified time (RFC 3339 / ISO-8601 UTC).
    pub last_modified: String,
}

/// One object content-search hit, from `GET /storage/catalogue/search`: which
/// object matched + a short excerpt of its §10 extracted text around the match.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ObjectHit {
    /// Catalogue object id (for the text viewer).
    pub id: String,
    /// The user-facing object key.
    pub key: String,
    /// Guessed content type, if known.
    #[serde(default)]
    pub content_type: Option<String>,
    /// A snippet of the extracted text windowed on the match.
    pub excerpt: String,
    /// The `?store=` selector the hit lives on, so its download targets the right
    /// backend (content search spans every store). Empty → the default store.
    #[serde(default)]
    pub store: String,
}

/// The §10 extracted text for a catalogued object, from
/// `GET /storage/catalogue/{id}/text` (the API's `routes::storage::ObjectTextView`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ObjectText {
    /// The object's catalogue id.
    pub id: String,
    /// The user-facing object key (the modal title).
    pub key: String,
    /// Whether the object has extracted text (false → not yet ingested).
    pub has_text: bool,
    /// The extracted text (bounded server-side; empty when `has_text` is false).
    pub text: String,
    /// Whether `text` was truncated at the server's size cap.
    #[serde(default)]
    pub truncated: bool,
    /// An optional one-paragraph summary, when the ingest produced one.
    #[serde(default)]
    pub summary: Option<String>,
}

/// A storage backend ("store") a file can be saved to, from `GET /storage/stores`
/// (the API's `routes::storage::StoreView`). The Files panel's destination picker
/// and the storage manager are built from these. Secrets are never included.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StorageStore {
    /// Store name — the `?store=` selector value.
    pub name: String,
    /// Backend kind (`local` / `s3` / `webdav` / `unknown`).
    pub kind: String,
    /// `config` (declared in TOML, read-only) or `runtime` (user-added, deletable).
    pub source: String,
    /// Whether this is the default store (the destination when none is picked).
    #[serde(default)]
    pub is_default: bool,
    /// Whether catalerum is watching this store (auto-reindex on change, §9/§10).
    #[serde(default)]
    pub watch: bool,
}

impl StorageStore {
    /// Whether this store was added at runtime (so it can be deleted via the API).
    #[must_use]
    pub fn is_runtime(&self) -> bool {
        self.source == "runtime"
    }
}

/// What a `POST /storage/stores/{name}/scan` pass reconciled (the API's
/// `routes::storage::ScanReport`). Surfaced as a transient banner after a manual
/// scan; indexing itself runs asynchronously, so badges appear shortly after.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ScanReport {
    /// Backend objects seen.
    #[serde(default)]
    pub scanned: usize,
    /// New or changed objects (re-)catalogued + queued for §10 ingest.
    #[serde(default)]
    pub indexed: usize,
    /// Already-catalogued, unchanged objects.
    #[serde(default)]
    pub unchanged: usize,
    /// Catalogue rows purged because the file is gone from the backend.
    #[serde(default)]
    pub removed: usize,
    /// Whether the listing hit the server cap (deletions past it weren't reconciled).
    #[serde(default)]
    pub truncated: bool,
}

/// `POST /storage/stores` body — add a runtime storage backend (SOUL §9). `config`
/// carries the backend's fields: local → `local_path`; s3 → `endpoint` / `region`
/// / `access_key` / `secret_key` / `bucket` / `path_style`; webdav → `url` /
/// `username` / `password`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NewStorageStore {
    pub name: String,
    pub kind: String,
    pub config: serde_json::Value,
}

/// A label on a stored file or directory (the API's `catalerum_core::ObjectLabel`),
/// from `GET /storage/labels`. The Files panel tags + filters its tree with these;
/// only the fields the panel renders are decoded (the API also carries the author
/// + timestamps, which serde ignores here).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct FileLabel {
    /// Label id (UUID string) — the delete key.
    pub id: String,
    /// The `?store=` selector the labelled path lives on (empty → default store).
    #[serde(default)]
    pub store: String,
    /// The user-facing path — a file's key or a directory path (no trailing `/`).
    pub path: String,
    /// Whether `path` is a directory (`true`) or a single file (`false`).
    #[serde(default)]
    pub is_dir: bool,
    /// The free-text label.
    pub label: String,
}

/// `POST /storage/labels` body — apply `label` to `path` in `store` (SOUL §9).
/// `store` empty targets the default store; `is_dir` records whether the path is a
/// directory or a file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NewFileLabel {
    pub store: String,
    pub path: String,
    pub is_dir: bool,
    pub label: String,
}

// ===========================================================================
// Skills REST contract (SOUL §23, §12 — skills manager).
//
// Mirrors `catalerum-api`'s skills routes (root-mounted: `/skills`,
// `/skills/{name}`). A skill is keyed by its per-workspace-unique `name` (not a
// UUID) — that name is the path key for update/delete. The API serializes the
// core `Skill`/`Code` directly; we re-declare the JSON shapes here (rather than
// depend on `catalerum-core`) so the wasm bundle stays lean.
// ===========================================================================

/// Optional executable code attached to a skill (the core `Code`, run via the
/// Executor §20). `language` is required; `source` is the body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Code {
    /// Language identifier (e.g. `python`).
    pub language: String,
    /// The source to execute.
    pub source: String,
    /// Optional pinned entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<String>,
}

/// A skill as projected by the API (the core `Skill`): a named, reusable bundle
/// of instructions + a restricted tool set + optional code (SOUL §23).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Skill {
    /// Skill id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Per-workspace-unique name (the invocation + path key).
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Markdown runbook / instructions.
    #[serde(default)]
    pub instructions_md: String,
    /// Tool names the skill may use (a subset of the registry).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Optional executable code.
    #[serde(default)]
    pub code: Option<Code>,
    /// Whether the skill's name + description are advertised to the chat agent
    /// in its system prompt ("visible to agent"). Defaults to `true`.
    #[serde(default = "default_true")]
    pub advertised: bool,
}

/// Request body for `POST /skills` (matches the API's `CreateSkill`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateSkill {
    /// Unique (per workspace) skill name.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Markdown runbook / instructions.
    pub instructions_md: String,
    /// Tool names the skill may use.
    pub tools: Vec<String>,
    /// Optional executable code (omitted from the payload when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    /// "Visible to agent": advertise the skill in the chat system prompt.
    pub advertised: bool,
}

/// Request body for `PUT /skills/{name}` (matches the API's `UpdateSkill`) — a
/// full replacement of the editable fields; the name comes from the path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateSkill {
    /// New one-line description.
    pub description: String,
    /// New markdown runbook / instructions.
    pub instructions_md: String,
    /// New tool set (replaces the existing tools).
    pub tools: Vec<String>,
    /// New optional code (omitted from the payload when absent → clears it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Code>,
    /// New "visible to agent" flag (full replacement, like the rest).
    pub advertised: bool,
}

/// A per-profile **tool guard** (mirrors the API's `ToolGuard`, SOUL §19): a Boa
/// JS and/or LLM classifier gating every tool call as allow / deny / require-user-
/// feedback, on top of the capability grant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGuard {
    /// A Boa JS classifier (a function body returning a decision).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// A declarative LLM classifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<ToolGuardLlm>,
    /// A declarative object-label allow/deny policy (SOUL §9).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_labels: Option<ObjectLabelPolicy>,
    /// Fallback when the classifier errors / is unparseable.
    #[serde(default)]
    pub on_error: GuardFail,
}

/// Allow/deny a tool call by the labels on the file it touches (mirrors the API's
/// `ObjectLabelPolicy`, SOUL §9/§19).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectLabelPolicy {
    /// If non-empty, the touched object must carry at least one of these labels.
    #[serde(default)]
    pub require_any: Vec<String>,
    /// An object carrying any of these labels is blocked (wins over require_any).
    #[serde(default)]
    pub deny: Vec<String>,
}

/// The declarative LLM classifier of a [`ToolGuard`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolGuardLlm {
    /// Model to judge with; absent uses the profile's model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The judge's standing instruction (policy).
    pub instruction: String,
}

/// The fallback ruling when a [`ToolGuard`] classifier can't decide.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardFail {
    /// Block the call (fail-closed). The default.
    #[default]
    Deny,
    /// Let it proceed (fail-open).
    Allow,
    /// Escalate to the user.
    Ask,
}

/// An agent profile as projected by the API (the core `AgentProfile`): a named,
/// reusable scoped-agent configuration — a model, a system prompt, a tool/skill
/// set, the subagents it may delegate to, the channels it listens on, and the §19
/// grant that is its authority (SOUL §19/§25).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AgentProfile {
    /// Profile id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Per-workspace-unique name (the path key for update/delete).
    pub name: String,
    /// Model id; absent uses the workspace default.
    #[serde(default)]
    pub model: Option<String>,
    /// System prompt; absent uses the default agent system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Tool names the profile may dispatch (subset of the registry); empty = all.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Skill names whose runbooks seed the system prompt.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Agent-profile names this profile may delegate to (subagents).
    #[serde(default)]
    pub subagents: Vec<String>,
    /// Channel names this profile listens on.
    #[serde(default)]
    pub channels: Vec<String>,
    /// The §19 grant id (UUID string) that is this profile's authority; absent =
    /// bounded base-Member capabilities.
    #[serde(default)]
    pub grant_id: Option<String>,
    /// The profile's optional tool guard (SOUL §19); absent = gated only by caps.
    #[serde(default)]
    pub guard: Option<ToolGuard>,
}

/// Request body for `POST /agent-profiles` (matches the API's
/// `CreateAgentProfile`). Optional string fields are omitted from the payload when
/// `None`, which the server reads as "unset".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateAgentProfile {
    /// Unique (per workspace) profile name.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub subagents: Vec<String>,
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard: Option<ToolGuard>,
}

/// Request body for `PUT /agent-profiles/{name}` (matches the API's
/// `UpdateAgentProfile`) — a full replacement; the name comes from the path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateAgentProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub tools: Vec<String>,
    pub skills: Vec<String>,
    pub subagents: Vec<String>,
    pub channels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guard: Option<ToolGuard>,
}

/// One agent tool from `GET /tools` (the API's slim `ToolInfo`) — the Profiles
/// tools checklist. `name` is the value a profile lists in its `tools` set.
/// (The Profiles model dropdown reuses the existing [`ModelInfo`] from the
/// LLM-settings surface, fetched via [`crate::rest::list_llm_models`].)
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ToolInfo {
    /// Stable tool name.
    pub name: String,
    /// One-line description (may be empty).
    #[serde(default)]
    pub description: String,
}

// ===========================================================================
// Automations REST contract (SOUL §11, §12 — automations builder).
//
// Mirrors `catalerum-api`'s automation routes (root-mounted: `/automations`,
// `/automations/{name}` + `/enabled` + `/runs`). An automation is keyed by its
// per-workspace-unique `name`. Triggers / condition / actions are free-form JSON
// (`serde_json::Value`) — the server validates them against the typed spec
// (`AutomationSpec`) and rejects a malformed authoring with `400`. Run status is
// the snake_case core `RunStatus` (`running`/`succeeded`/`failed`/`cancelled`),
// decoded as a plain string so a new variant never breaks the listing.
// ===========================================================================

/// An automation as projected by the API (the core `Automation`). `grant_id` is
/// present on the wire but the UI does not surface it (serde ignores it).
/// `Serialize` is derived so the Automations panel can pretty-print the whole
/// stored object (ids included) into its Raw-JSON editor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Automation {
    /// Automation id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Per-workspace-unique name (the invocation + path key).
    pub name: String,
    /// Whether the automation is active.
    #[serde(default)]
    pub enabled: bool,
    /// Trigger specs (`{ "kind": "schedule", "cron": "…" }`, …).
    #[serde(default)]
    pub triggers: Vec<serde_json::Value>,
    /// Optional condition predicate.
    #[serde(default)]
    pub condition: Option<serde_json::Value>,
    /// Ordered typed action specs.
    #[serde(default)]
    pub actions: Vec<serde_json::Value>,
    /// The full original authoring spec, if one was supplied.
    #[serde(default)]
    pub spec: Option<serde_json::Value>,
}

/// One documented parameter of an automation node type (mirrors the API's
/// `catalerum_automation::NodeParam`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct NodeTypeParam {
    /// JSON key (e.g. `cron`, `title`).
    pub name: String,
    /// Loose type hint (`string`, `integer`, `object`, `string[]`, …).
    pub ty: String,
    /// Whether the field is required.
    #[serde(default)]
    pub required: bool,
    /// What the field means / how to fill it.
    #[serde(default)]
    pub description: String,
}

/// A documented automation node type with its relevance score, as returned by
/// `GET /automations/node-types/search` (the catalog `NodeDoc` flattened + `score`).
/// The full catalog (`GET /automations/node-types`) returns the same shape with
/// `score` defaulting to 0.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct NodeTypeHit {
    /// Stable id: `trigger.<kind>` / `action.<kind>` / `code` / `condition`.
    pub id: String,
    /// Owning node kind: `trigger` / `action` / `code` / `condition`.
    pub node_kind: String,
    /// Inner kind tag for a trigger/action (empty for code/condition).
    #[serde(default)]
    pub kind: String,
    /// Human-readable label.
    pub title: String,
    /// One-line summary.
    #[serde(default)]
    pub summary: String,
    /// Author-facing description (what it does / when to use it).
    #[serde(default)]
    pub description: String,
    /// Typed parameters.
    #[serde(default)]
    pub params: Vec<NodeTypeParam>,
    /// A ready-to-paste example graph node (`{id, kind, …, position}`).
    #[serde(default)]
    pub example: serde_json::Value,
    /// Cosine similarity to the query (0 for the unranked full-catalog listing).
    #[serde(default)]
    pub score: f32,
}

/// One recent run of an automation (the core `AutomationRun`), for the run
/// history. `status` is the snake_case `RunStatus`; timestamps are RFC 3339.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AutomationRun {
    /// Run id (UUID string).
    pub id: String,
    /// Lifecycle status: `running` | `succeeded` | `failed` | `cancelled`.
    pub status: String,
    /// What fired the run — the matched trigger + event payload.
    #[serde(default)]
    pub trigger: Option<serde_json::Value>,
    /// Failure detail when `status` is `failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// When the run started (RFC 3339 / ISO-8601 UTC).
    pub started_at: String,
    /// When the run reached a terminal state (absent while running).
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// Request body for `POST /automations` (matches the API's `CreateAutomation`).
/// `grant_id` is intentionally absent — assigned by the §19 policy engine, never
/// claimed by the client.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CreateAutomation {
    /// Unique (per workspace) name.
    pub name: String,
    /// Whether the automation is active.
    pub enabled: bool,
    /// Trigger specs.
    pub triggers: Vec<serde_json::Value>,
    /// Optional condition predicate (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<serde_json::Value>,
    /// Ordered typed action specs.
    pub actions: Vec<serde_json::Value>,
    /// The full authoring spec (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

/// Request body for `PUT /automations/{name}` (the API's `UpdateAutomation`) — a
/// full replacement; the name comes from the path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct UpdateAutomation {
    /// Whether the automation is active.
    pub enabled: bool,
    /// Trigger specs.
    pub triggers: Vec<serde_json::Value>,
    /// Optional condition predicate (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<serde_json::Value>,
    /// Ordered typed action specs.
    pub actions: Vec<serde_json::Value>,
    /// The full authoring spec (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

/// Request body for `POST /automations/{name}/enabled` — pause / resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SetEnabled {
    /// The new active state.
    pub enabled: bool,
}

/// Response for `POST /automations/{name}/collect` — "collect now" (SOUL §29). The
/// server enqueues one immediate poll of a Collect-headed automation and returns the
/// durable collect-job id (`202 Accepted`). Mirrors the API's `CollectNowResult`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CollectNowResult {
    /// The durable collect-job id enqueued for the poll (one job per call).
    pub job: String,
}

/// Response for `POST /triggers/{name}` — fire a named signal (SOUL §11). Reports how
/// many enabled automations matched the signal name and the durable `run_automation`
/// jobs enqueued (`202 Accepted`). Mirrors the API's `FireResult`; job ids are decoded
/// as plain strings.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct FireResult {
    /// Number of enabled automations whose trigger matched the fired signal name.
    #[serde(default)]
    pub matched: usize,
    /// The enqueued `run_automation` job ids (one per matched automation).
    #[serde(default)]
    pub jobs: Vec<String>,
}

/// One executed action within a run (the core `AutomationStep`), for the
/// run-detail view. `status` is the snake_case `StepStatus`
/// (`running`/`succeeded`/`failed`/`skipped`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct AutomationStep {
    /// Step id (UUID string).
    pub id: String,
    /// Position within the run (0-based execution order).
    #[serde(default)]
    pub ordinal: i32,
    /// The action spec that was executed.
    #[serde(default)]
    pub action: serde_json::Value,
    /// Step status.
    #[serde(default)]
    pub status: String,
    /// The action's output, when it produced one.
    #[serde(default)]
    pub output: Option<serde_json::Value>,
    /// Failure detail when `status` is `failed`.
    #[serde(default)]
    pub error: Option<String>,
    /// When the step started (RFC 3339).
    #[serde(default)]
    pub started_at: String,
    /// When the step finished (absent while running).
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// `GET /automations/{name}/runs/{run_id}` response (the API's `RunDetail`) — a
/// run plus its ordered steps (the durable audit trail of one execution).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RunDetail {
    /// The run.
    pub run: AutomationRun,
    /// The run's ordered steps.
    #[serde(default)]
    pub steps: Vec<AutomationStep>,
}

// ===========================================================================
// Grants REST contract (SOUL §19, §12 — capability-grant builder, admin-only).
//
// Mirrors `catalerum-api`'s grant routes (root-mounted: `/grants`,
// `/grants/{id}`). A grant is `{ name, capabilities, constraints }`. `POST` is
// create-or-replace by name (keeps the id); get/delete are by **id**. We
// re-declare the core `Capability`/`Resource`/`Action` here (faithful to the
// wire) so a grant round-trips losslessly; grant-global `constraints` ride as a
// raw JSON object (the core `Constraints` is `deny_unknown_fields`, so the
// server rejects an unknown key — surfaced as the form error).
// ===========================================================================

/// The verb of a capability (the core `Action`, snake_case on the wire).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Wildcard — any action on the matched resource.
    Any,
    Read,
    Write,
    Delete,
    /// Invoke a skill (`skill:use@…`).
    Use,
    /// Run code/commands (`exec:run@…`).
    Run,
    /// Query the graph (`graph:query`).
    Query,
    /// Semantic search (`vector:search`).
    Search,
    /// Expose over MCP (`mcp:expose@…`).
    Expose,
}

/// The resource a capability applies to (the core `Resource`): a domain + an
/// optional `@`-selector glob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resource {
    /// The resource domain, e.g. `calendar`, `storage`, `notes`, `exec`.
    pub domain: String,
    /// Optional `@`-suffix selector (`None` = the whole domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
}

/// A single capability (the core `Capability`): `(action, resource,
/// per-capability constraints)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    /// The action verb.
    pub action: Action,
    /// The resource the action applies to.
    pub resource: Resource,
    /// Resource-specific constraints (e.g. `{"lang":"python"}`), usually empty.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub constraints: serde_json::Map<String, serde_json::Value>,
}

/// A capability grant as projected by the API (the core `Grant`). `constraints`
/// is decoded as a raw JSON object so the editor can show + round-trip it without
/// re-declaring the full `Constraints` schema.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Grant {
    /// Grant id (UUID string) — the get/delete key.
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// The grant name (the create-or-replace key).
    pub name: String,
    /// The capabilities this grant confers.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Global constraints (raw JSON object: env allow-list, rate/cost caps, time
    /// window, dry-run, per-action approval).
    #[serde(default)]
    pub constraints: serde_json::Value,
}

/// Request body for `POST /grants` (matches the API's `CreateGrant`). Idempotent
/// by name (create-or-replace).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CreateGrant {
    /// The grant name.
    pub name: String,
    /// The capabilities to confer.
    pub capabilities: Vec<Capability>,
    /// Global constraints (a raw JSON object; `{}` for none).
    pub constraints: serde_json::Value,
}

// ===========================================================================
// Conversations REST contract (SOUL §12 — conversation history browser).
//
// Mirrors `catalerum-api`'s conversation routes (root-mounted: `/conversations`,
// `/conversations/{id}/messages`). A read view of past chat threads + their
// transcripts. `origin` (`web`/`automation`/`channel`/`mcp`) and `role`
// (`system`/`user`/`assistant`/`tool`) are decoded as plain strings so a new
// variant never breaks the listing.
// ===========================================================================

/// A chat thread as projected by the API (the core `Conversation`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Conversation {
    /// Conversation id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Optional human title.
    #[serde(default)]
    pub title: Option<String>,
    /// Where the thread originated: `web` | `automation` | `channel` | `mcp`.
    #[serde(default)]
    pub origin: String,
    /// When the thread was started (RFC 3339 / ISO-8601 UTC). Drives the Chat
    /// sidebar's newest-first ordering + time grouping (last 7 days / weeks /
    /// months). `#[serde(default)]` keeps an older API (without the field) decoding.
    #[serde(default)]
    pub created_at: String,
    /// The agent profile (UUID string) this thread runs *as*, if bound via the chat
    /// picker (SOUL §19); absent/`None` = the default chat (the user's own role).
    #[serde(default)]
    pub agent_profile_id: Option<String>,
    /// The model this thread is pinned to via the chat "model" picker (SOUL §7);
    /// a free-form gateway model id. Absent/`None` = no override (the bound
    /// profile's model, then the user/workspace default).
    #[serde(default)]
    pub model: Option<String>,
    /// The reasoning ("thinking") effort this thread requests via the chat "thinking"
    /// picker (SOUL §7): a free-form gateway token (`low`/`medium`/`high`/`xhigh`/`max`).
    /// Absent/`None` = no reasoning requested (the provider default).
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Topic tags generated by the backend's background auto-title/auto-tag
    /// pass, rendered as pills in the sidebar. Absent/empty on older servers
    /// and until the pass lands (the optimistic title shows meanwhile).
    #[serde(default)]
    pub tags: Vec<String>,
    /// `true` iff the title was set by an explicit user rename — the backend's
    /// auto-title generator must not overwrite it (the client just mirrors the
    /// flag for completeness).
    #[serde(default)]
    pub title_manual: bool,
}

/// Request body for `POST /conversations` (matches the API's `CreateConversation`).
/// Only `title` is sent; `origin` defaults to `web` server-side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateConversation {
    /// Client-generated idempotency id for the conversation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Optional human title (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Request body for `PUT /conversations/{id}` — rename a conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameConversation {
    /// The new title (non-empty).
    pub title: String,
}

/// Request body for `POST /conversations/{id}/profile` — the chat "run as a
/// profile" picker (SOUL §19). The field is always sent (with `null` to unbind), so
/// the server sees the explicit choice.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetConversationProfile {
    /// The agent profile id (UUID) to run this thread as; `None` unbinds.
    pub agent_profile_id: Option<String>,
}

/// Request body for `POST /conversations/{id}/model` — the chat "model" picker
/// (SOUL §7). Always sent (with `null` to clear the override).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetConversationModel {
    /// The gateway model id to pin for this thread; `None` clears the override.
    pub model: Option<String>,
}

/// Request body for `POST /conversations/{id}/reasoning` — the chat "thinking" picker
/// (SOUL §7). Always sent (with `null` to clear = no reasoning requested).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetConversationReasoning {
    /// The reasoning-effort token to request for this thread; `None` clears it.
    pub reasoning_effort: Option<String>,
}

/// A tool call emitted by an assistant turn (the core `ToolCall`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id.
    pub id: String,
    /// Tool/function name dispatched.
    pub name: String,
    /// JSON-encoded arguments (kept as a string, matching the wire shape).
    #[serde(default)]
    pub arguments: String,
}

/// Per-turn token + cache + cost accounting persisted on the final assistant
/// message of an exchange (mirrors `catalerum_core::stream::Usage`). Present on a
/// replayed transcript so the token info-icon / cost readout survive a reload;
/// absent on user/tool rows, non-final turns, and pre-feature transcripts. Counts
/// default to `0` and `cost_usd` to `None` (both omitted on the wire when zero).
#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct MsgUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub cost_usd: Option<f64>,
    #[serde(default)]
    pub cached_tokens: u32,
    #[serde(default)]
    pub cache_creation_tokens: u32,
}

/// A single message in a conversation transcript (the core `Message`).
///
/// Not `Eq`: [`usage`](Self::usage) carries an `f64` cost.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Message {
    /// Message id (UUID string).
    pub id: String,
    /// Owning conversation.
    pub conversation_id: String,
    /// Role: `system` | `user` | `assistant` | `tool`.
    pub role: String,
    /// Message text (may be empty for a pure tool-call turn).
    #[serde(default)]
    pub content: String,
    /// Tool calls emitted by an assistant turn (empty otherwise).
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// For a `tool` message, the id of the tool call it answers.
    #[serde(default)]
    pub tool_call_id: Option<String>,
    /// For a `tool` message, whether the call failed (persisted; replaces the
    /// `{"error":…}` content heuristic). Absent/`false` for older transcripts.
    #[serde(default)]
    pub tool_is_error: bool,
    /// For a `tool` message, the dispatch duration in milliseconds, when recorded.
    #[serde(default)]
    pub tool_duration_ms: Option<i64>,
    /// Per-turn token + cost accounting, persisted on the final assistant message
    /// of an exchange. Drives the replayed token info-icon / cost readout; absent
    /// on other rows and pre-feature transcripts.
    #[serde(default)]
    pub usage: Option<MsgUsage>,
    /// File / image references attached to a **user** turn (SOUL §9/§12). The
    /// bytes live in the workspace files store; only these references ride on the
    /// row (the same [`Attachment`] shape events carry). Shown at the top of the
    /// message bubble on replay. Absent on an older server / non-user rows → empty.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// When the message was created (RFC 3339 / ISO-8601 UTC).
    pub created_at: String,
}

/// One hit from `GET /conversations/search`: a matched [`Message`] (flattened on
/// the wire) plus the title of the conversation it belongs to.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct MessageHit {
    #[serde(flatten)]
    pub message: Message,
    /// The owning conversation's title (`None` if untitled).
    #[serde(default)]
    pub conversation_title: Option<String>,
}

// ===========================================================================
// Email REST contract (SOUL §28, §12 — read-only inbox view).
//
// Mirrors `catalerum-api`'s email routes (root-mounted: `/mailboxes`, `/emails`,
// `/emails/{id}`). A read view of ingested mail (catalerum reads mail, never
// sends, §14): mailboxes, compact list rows ([`EmailView`], no body), and a full
// [`EmailDetail`] (body + recipients). `received_at` is RFC 3339.
// ===========================================================================

/// A mailbox as projected by the API (the `MailboxView`: the core `Mailbox`
/// plus the sidebar annotations — unread count + owning account name).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Mailbox {
    /// Mailbox id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Owning connection.
    pub connection_id: String,
    /// The owning email source's display name — the sidebar's account header.
    /// Absent on an older server → empty (the sidebar falls back to a generic
    /// account label).
    #[serde(default)]
    pub connection_name: String,
    /// Provider-native identifier.
    pub external_id: String,
    /// Display name (the `?mailbox=` filter key).
    pub name: String,
    /// Whether the mailbox is read-only.
    #[serde(default)]
    pub read_only: bool,
    /// How many stored emails here lack the `seen` flag — the sidebar badge.
    /// Absent on an older server → `0`.
    #[serde(default)]
    pub unread_count: i64,
}

/// A normalized email address (the core `EmailAddress`).
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct EmailAddress {
    /// Optional display name.
    #[serde(default)]
    pub name: Option<String>,
    /// The address itself.
    #[serde(default)]
    pub address: String,
}

impl EmailAddress {
    /// Render as `"Name <addr>"`, or just the address when unnamed.
    #[must_use]
    pub fn display(&self) -> String {
        match &self.name {
            Some(n) if !n.trim().is_empty() => format!("{n} <{}>", self.address),
            _ => self.address.clone(),
        }
    }
}

/// serde default for a `folder_count` field: a message filed in exactly one folder.
/// An older server omits the field entirely (single-filed is the common case), so it
/// must decode to `1`, not `0`.
fn default_folder_count() -> usize {
    1
}

/// A compact email list row (the API's `EmailView`; no body).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EmailView {
    /// Email id (UUID string).
    pub id: String,
    /// The mailbox name the email lives in.
    #[serde(default)]
    pub mailbox: String,
    /// Pre-formatted `From` (`"Name <addr>"`), if present.
    #[serde(default)]
    pub from: Option<String>,
    /// Subject line.
    #[serde(default)]
    pub subject: String,
    /// When the email was received (RFC 3339), if known.
    #[serde(default)]
    pub received_at: Option<String>,
    /// Whether the email is unread (no provider `seen` flag).
    #[serde(default)]
    pub unread: bool,
    /// Whether the email has attachments.
    #[serde(default)]
    pub has_attachments: bool,
    /// How many distinct folders this message is filed under across the workspace
    /// (SOUL §29 cross-folder dedup). `1` = single-filed; `>1` = cross-filed. An older
    /// server omits it → defaults to `1`.
    #[serde(default = "default_folder_count")]
    pub folder_count: usize,
    /// The OTHER folders this message is also filed in (this row's own mailbox removed),
    /// for the cross-folder badge tooltip. Empty when single-filed or on an older server.
    #[serde(default)]
    pub also_in: Vec<String>,
}

/// A single email with body + recipients (the API's `EmailDetail`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EmailDetail {
    /// Email id (UUID string).
    pub id: String,
    /// The mailbox name.
    #[serde(default)]
    pub mailbox: String,
    /// RFC `Message-ID`, if present.
    #[serde(default)]
    pub message_id: Option<String>,
    /// The sender.
    #[serde(default)]
    pub from: Option<EmailAddress>,
    /// `To` recipients.
    #[serde(default)]
    pub to: Vec<EmailAddress>,
    /// `Cc` recipients.
    #[serde(default)]
    pub cc: Vec<EmailAddress>,
    /// Subject line.
    #[serde(default)]
    pub subject: String,
    /// When the email was received (RFC 3339), if known.
    #[serde(default)]
    pub received_at: Option<String>,
    /// Plain-text body, if extracted.
    #[serde(default)]
    pub body_text: Option<String>,
    /// HTML body, if present. The API sanitizes it (allowlist; no scripts /
    /// event handlers / dangerous URLs) before it crosses the wire; the panel
    /// renders it inside a fully sandboxed iframe, never in the page DOM.
    #[serde(default)]
    pub body_html: Option<String>,
    /// Whether the email is unread.
    #[serde(default)]
    pub unread: bool,
    /// Whether the email has attachments.
    #[serde(default)]
    pub has_attachments: bool,
    /// Archived attachment **references** (SOUL §9/§28/§29): each MIME attachment,
    /// once archived, is a separate object in the workspace files store, linked
    /// here by an [`Attachment`] whose `url` is `/storage/objects/<key>`. Absent on
    /// an older server that doesn't yet project them → decodes to empty.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Object-storage key of the archived raw RFC 5322 `.eml`, when the message has
    /// been archived. Absent on an older server → `None`.
    #[serde(default)]
    pub raw_ref: Option<String>,
    /// How many distinct folders this message is filed under (SOUL §29 cross-folder
    /// dedup). `1` = single-filed; an older server omits it → defaults to `1`.
    #[serde(default = "default_folder_count")]
    pub folder_count: usize,
    /// The OTHER folders this message is also filed in (this mailbox removed). Empty
    /// when single-filed or on an older server.
    #[serde(default)]
    pub also_in: Vec<String>,
}

/// Response of `PATCH /emails/{id}` — the email's new read state (the API's
/// `EmailReadState`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EmailReadState {
    /// Email id (UUID string).
    pub id: String,
    /// The state after the toggle.
    pub unread: bool,
}

/// The provider sub-kind of an email source (matches the API's
/// `EmailProviderKind`). All four backends are implemented: a local Maildir plus
/// the network providers IMAP, JMAP, and Gmail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailProviderKind {
    /// A local Maildir directory.
    #[default]
    Maildir,
    /// RFC 3501 IMAP over TLS.
    Imap,
    /// RFC 8621 JMAP over HTTP.
    Jmap,
    /// The Gmail API (OAuth2 refresh-token grant).
    Gmail,
}

impl EmailProviderKind {
    /// Every provider kind, in `<select>` display order.
    #[must_use]
    pub fn all() -> [EmailProviderKind; 4] {
        [
            EmailProviderKind::Maildir,
            EmailProviderKind::Imap,
            EmailProviderKind::Jmap,
            EmailProviderKind::Gmail,
        ]
    }

    /// Human-readable label for the provider `<select>`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            EmailProviderKind::Maildir => "Local Maildir directory",
            EmailProviderKind::Imap => "IMAP server",
            EmailProviderKind::Jmap => "JMAP server",
            EmailProviderKind::Gmail => "Gmail",
        }
    }

    /// The snake_case wire token the API's `EmailProviderKind` expects.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EmailProviderKind::Maildir => "maildir",
            EmailProviderKind::Imap => "imap",
            EmailProviderKind::Jmap => "jmap",
            EmailProviderKind::Gmail => "gmail",
        }
    }

    /// Parse a wire token back into a provider kind.
    #[must_use]
    pub fn parse_token(token: &str) -> Option<Self> {
        match token {
            "maildir" => Some(EmailProviderKind::Maildir),
            "imap" => Some(EmailProviderKind::Imap),
            "jmap" => Some(EmailProviderKind::Jmap),
            "gmail" => Some(EmailProviderKind::Gmail),
            _ => None,
        }
    }
}

/// Request body for `POST /email/connections` (matches the API's
/// `CreateEmailConnection`). Read-only ingest — catalerum reads mail, it never
/// sends/replies (SOUL §14/§28). Only the fields relevant to `provider` are sent;
/// the rest are omitted from the payload.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CreateEmailConnection {
    /// Provider sub-kind.
    pub provider: EmailProviderKind,
    /// Human-readable name for the source.
    pub name: String,
    /// **Maildir**: root directory (contains `new/`/`cur/`/`tmp/`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub root: String,
    /// **Maildir/IMAP**: mailbox/folder name (server defaults to `"INBOX"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mailbox: Option<String>,
    /// **IMAP**: server hostname.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// **IMAP**: server port (server defaults to 993).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// **IMAP**: login username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// **IMAP**: login password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// **JMAP**: session resource URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
    /// **JMAP**: bearer token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// **JMAP**: optional account-id override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// **Gmail**: OAuth2 client id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// **Gmail**: OAuth2 client secret.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    /// **Gmail**: long-lived OAuth2 refresh token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// **Gmail**: label id to ingest (server defaults to `"INBOX"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One email source with its **non-secret** settings (the API's
/// `EmailConnectionDetail`) — the edit form's prefill for
/// `GET /email/connections/{id}`. Secrets never cross the wire; `has_secrets`
/// only says whether any are stored (so the form can show "(unchanged)").
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct EmailConnectionDetail {
    /// Connection id (UUID string).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Provider wire token (`maildir` / `imap` / `jmap` / `gmail`).
    #[serde(default)]
    pub provider: String,
    /// Non-secret provider settings (`root`/`host`/`port`/`username`/…).
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
    /// Whether any secret (password / token / client secret) is stored.
    #[serde(default)]
    pub has_secrets: bool,
}

impl EmailConnectionDetail {
    /// A settings field as a display string: strings verbatim, other scalars
    /// (a numeric `port`) via their JSON rendering, absent/null → `""`.
    #[must_use]
    pub fn setting(&self, key: &str) -> String {
        match self.settings.get(key) {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
        }
    }
}

// ===========================================================================
// Status + API-keys REST contract (SOUL §12/§18 — Settings panel).
//
// `GET /status` → version + non-secret LLM gateway config + per-service health.
// `GET/POST /tokens` + `DELETE /tokens/{id}` → manage workspace bearer tokens.
// ===========================================================================

/// The non-secret LLM gateway config (the API's `LlmInfo`). The api_key is never
/// sent — only the base URL and model names.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LlmInfo {
    /// Gateway origin (e.g. `http://localhost:8088`).
    #[serde(default)]
    pub base_url: String,
    /// Chat model.
    #[serde(default)]
    pub default_model: String,
    /// Embedding model.
    #[serde(default)]
    pub embedding_model: String,
    /// Text-to-speech model.
    #[serde(default)]
    pub speech_model: String,
    /// Default text-to-speech voice.
    #[serde(default)]
    pub speech_voice: String,
    /// Speech-to-text model.
    #[serde(default)]
    pub transcription_model: String,
    /// The configured `[ocr]` engines in chain order (`mistral`, `vision`,
    /// `tesseract`); empty = OCR off. Absent on an older server ⇒ empty.
    #[serde(default)]
    pub ocr_engines: Vec<String>,
}

/// The liveness of one backing service (the API's `ServiceStatus`). `state` is
/// `"up"`, `"down"`, or `"disabled"` (forward-compatible as a plain string).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ServiceStatus {
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// `"up"` | `"down"` | `"disabled"`.
    #[serde(default)]
    pub state: String,
    /// Short human detail (URL, mode, or error summary).
    #[serde(default)]
    pub detail: String,
}

/// serde default for [`StatusInfo::mode`]: an older server that omits the field
/// is treated as `single_user` — the leaner presentation that hides member/role
/// chrome, so a missing `mode` never surfaces multi-user admin surfaces.
fn default_single_user() -> String {
    "single_user".to_string()
}

/// `GET /status` response (the API's `StatusResponse`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct StatusInfo {
    /// Server version.
    #[serde(default)]
    pub version: String,
    /// The deployment mode (`single_user` | `multi_user`, SOUL §18) — presentation
    /// defaults only, never authz. Absent ⇒ `single_user` (see
    /// [`default_single_user`]), so an older server keeps the minimal chrome.
    #[serde(default = "default_single_user")]
    pub mode: String,
    /// Whether OIDC single sign-on is configured on this instance (SOUL §18/§29) —
    /// the seam for the login view's "Sign in with SSO" button. Absent ⇒ `false`
    /// (an older server, or a build without SSO, presents no SSO affordance).
    #[serde(default)]
    pub sso: bool,
    /// Whether this deployment exposes the llmleaf topology editor. Absent on
    /// older servers means disabled, matching the safe deployment default.
    #[serde(default)]
    pub llm_control_plane: bool,
    /// Rolled-up verdict: `true` iff no backing service is `down` (an older server
    /// that omits the field defaults to `false`).
    #[serde(default)]
    pub healthy: bool,
    /// LLM gateway config (non-secret).
    pub llm: LlmInfo,
    /// Per-service health, in display order.
    #[serde(default)]
    pub services: Vec<ServiceStatus>,
}

/// Mirror of `GET /status/login` — the **anonymous** slice of [`StatusInfo`] the
/// login view reads before any session exists: just the presentation flags.
/// Both fields default so an older server (404/absent fields) never breaks the
/// login page — `sso` absent ⇒ `false` is handled by the caller's tri-state
/// probe, not the decode default.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LoginStatusInfo {
    /// Whether OIDC single sign-on is configured on this instance.
    #[serde(default)]
    pub sso: bool,
    /// The full browser-facing `GET /auth/sso/login` URL, when the server pins
    /// one in config (`[sso].public_url` / `[server].base_url` — for APIs not
    /// reachable at the derived `api.<host>`, e.g. behind a Kubernetes ingress).
    /// Absent ⇒ the login href is built on [`api_base`].
    #[serde(default)]
    pub sso_login_url: Option<String>,
    /// The deployment mode (`single_user` | `multi_user`) — presentation only.
    #[serde(default = "default_single_user")]
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SetupStatusInfo {
    pub enabled: bool,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupAccount {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PasswordLogin {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoginSession {
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmleafTopologyEntry {
    pub kind: String,
    pub name: String,
    pub enabled: bool,
    pub spec: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct PutLlmleafTopology {
    pub enabled: bool,
    pub spec: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ManagedUser {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateManagedUser {
    pub email: String,
    pub display_name: String,
    pub password: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResetManagedPassword {
    pub password: String,
}

// ===========================================================================
// LLM settings + catalog contract (SOUL §7/§13 — the Settings "Models" tab).
//
// Mirrors `catalerum-api`'s `routes::settings` (root-mounted: `/llm-settings`,
// `/llm-models`, `/llm-voices`). The user picks a chat model + speech model/voice
// over the immutable `[llm]` config base; each blank choice means "use the
// gateway default". [`ModelInfo`]/[`VoiceInfo`] feed the autocomplete; only the
// fields the picker needs are decoded (the API sends more). As elsewhere, the
// shapes are re-declared here rather than depending on `catalerum-core`/`-llm`.
// ===========================================================================

/// The caller's per-user model/voice selections (the API's `LlmSettings`). Each
/// field is `None`/absent when unset → the gateway default applies. Used as both
/// the `GET /llm-settings` response and the `PUT /llm-settings` body, so it both
/// (de)serializes; an unset field is omitted on the wire (cleared server-side).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LlmSettings {
    /// Chat / completion model id; `None` → gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_model: Option<String>,
    /// Text-to-speech model id; `None` → gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_model: Option<String>,
    /// Text-to-speech voice id; `None` → gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speech_voice: Option<String>,
    /// Speech-to-text model id; `None` → gateway default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcription_model: Option<String>,
    /// Time-compression factor applied to microphone recordings before STT.
    #[serde(default = "default_voice_input_speed")]
    pub voice_input_speed: f32,
    /// OCR vision model id (image → text); `None` → the server's `[ocr]` engine
    /// chain decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_model: Option<String>,
    /// Model ids the user forces to accept image input regardless of the gateway
    /// catalog (SOUL §7/§9) — read from `GET /llm-settings`, edited via the
    /// dedicated `PUT /llm-settings/image-models` (a plain `PUT /llm-settings` does
    /// NOT touch this list). Drives the chat "force image input" toggle and the
    /// capability chip's forced state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_input_models: Vec<String>,
}

/// Default for servers/settings records that predate voice-input compression.
#[must_use]
pub const fn default_voice_input_speed() -> f32 {
    1.5
}

/// One model in the gateway catalog (the API's `ModelInfo`), for the autocomplete
/// datalist and the chat "Model capabilities" readout. Only the picker- and
/// capability-relevant fields are decoded.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ModelInfo {
    /// Model id / routing alias — the value the picker stores.
    pub id: String,
    /// Human-friendly display name.
    #[serde(default)]
    pub name: String,
    /// Max context window in tokens, when known (shown as a hint).
    #[serde(default)]
    pub context_length: Option<u32>,
    /// Accepted input modalities (e.g. `text`, `image`) — `image` means the model
    /// can see an inlined image attachment (SOUL §7/§9).
    #[serde(default)]
    pub input_modalities: Vec<String>,
    /// Produced output modalities (e.g. `text`, `image`).
    #[serde(default)]
    pub output_modalities: Vec<String>,
    /// Request parameters the model supports (e.g. `tools`, `reasoning`) — drives
    /// the Tools / Reasoning capability chips.
    #[serde(default)]
    pub supported_parameters: Vec<String>,
}

/// One TTS voice (the API's `VoiceInfo`), for the autocomplete datalist.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct VoiceInfo {
    /// Voice id — the value the picker stores.
    pub id: String,
    /// Display name, when given.
    #[serde(default)]
    pub name: Option<String>,
}

/// The caller's per-user web-search settings (the API's `SearchSettings`, SOUL
/// §27/§13). Only `default_provider` is editable; the other fields the server
/// returns (workspace/user id) are ignored. Doubles as the `PUT` body — a `None`
/// provider serializes to `{}`, which the server reads as "clear the override".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSettings {
    /// Preferred default provider; `None` → the `[search].backend` config default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
}

/// The caller's per-user storage settings (the API's `StorageSettings`, SOUL
/// §9/§13). Only `default_store` is editable; the server's workspace/user ids are
/// ignored. Doubles as the `PUT` body — a `None` store serializes to `{}`, which
/// the server reads as "clear the override" (→ the `[storage]` config default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSettings {
    /// Preferred default store name; `None` → the `[storage]` config default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_store: Option<String>,
}

/// One row of the search-providers catalog (the API's `SearchProviderInfo`, SOUL
/// §27). Carries no secret — just the engine name, whether it is configured
/// server-side, and whether it is the caller's effective default.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SearchProviderInfo {
    /// Provider id (`brave`, `tavily`, …).
    pub name: String,
    /// Whether the provider is configured server-side (its credential is set).
    #[serde(default)]
    pub enabled: bool,
    /// Whether this provider is the caller's effective default.
    #[serde(default)]
    pub is_default: bool,
}

// ===========================================================================
// External MCP servers (SOUL §26) — catalerum as an MCP *client*. The workspace
// registers the servers it connects out to (stdio: spawn a command; http:
// connect to a URL with optional auth); each enabled server's tools fold into
// the agent's tool set. As with `LlmSettings`, the shapes mirror the API's
// (`routes::mcp_servers`) rather than depending on `catalerum-core`. Secrets
// never cross the wire: a view reports only *whether* each secret is set, and an
// update with a blank secret keeps the stored one.
// ===========================================================================

/// A redacted view of one stored external MCP server (the API's `McpServerView`,
/// `GET /mcp-servers`). Secrets are absent — only `env_keys` (not values) and the
/// per-secret bools on [`McpAuthView`] say what is configured.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct McpServerView {
    pub name: String,
    /// `"stdio"` or `"http"`.
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// The `env` keys (values redacted).
    #[serde(default)]
    pub env_keys: Vec<String>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub auth: McpAuthView,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Whether the server is currently connected live.
    #[serde(default)]
    pub connected: bool,
    /// The most recent connect error, if the last (re)connect failed.
    #[serde(default)]
    pub connect_error: Option<String>,
}

/// Redacted auth view (the API's `McpAuthView`): non-secret fields plus a bool
/// per secret so the edit form can show "set" without echoing the value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct McpAuthView {
    /// `none` | `bearer` | `header` | `oauth2`.
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub header_name: String,
    #[serde(default)]
    pub token_url: String,
    #[serde(default)]
    pub grant_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub has_token: bool,
    #[serde(default)]
    pub has_header_value: bool,
    #[serde(default)]
    pub has_client_secret: bool,
    #[serde(default)]
    pub has_refresh_token: bool,
}

/// Create/update body for an external MCP server (the API's `McpServerBody`,
/// `POST /mcp-servers` + `PUT /mcp-servers/{name}`). Secret fields are only sent
/// when (re)entered; a blank secret on update keeps the stored one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct McpServerBody {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub url: String,
    pub auth: McpAuthBody,
    pub enabled: bool,
    pub tools: Vec<String>,
}

/// The auth half of [`McpServerBody`] (the API's `McpAuthBody`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct McpAuthBody {
    pub kind: String,
    pub token: String,
    pub header_name: String,
    pub header_value: String,
    pub token_url: String,
    pub grant_type: String,
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    pub scope: String,
}

// ===========================================================================
// Computer agents (SOUL §19/§20). Installed daemons on servers/desktops the LLM
// drives over an authenticated WebSocket. As elsewhere, the shapes mirror the
// API's (`routes::computer_agents`) rather than depending on `catalerum-core`;
// the enrollment token crosses the wire only once (the create response).
// ===========================================================================

/// One served directory as reported in a computer agent's capabilities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ComputerDir {
    #[serde(default)]
    pub path: String,
    /// `"read"` or `"read_write"`.
    #[serde(default)]
    pub mode: String,
}

/// A computer agent's announced capabilities (the API's `ComputerCapabilities`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ComputerCapabilities {
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub dirs: Vec<ComputerDir>,
    #[serde(default)]
    pub grantable_roots: Vec<String>,
    #[serde(default)]
    pub exec_policy: String,
    #[serde(default)]
    pub desktop: bool,
    #[serde(default)]
    pub sandbox: String,
}

/// A redacted view of one enrolled computer agent (the API's `ComputerAgentView`,
/// `GET /computer-agents`). The token is never listed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct ComputerAgentView {
    pub id: String,
    pub name: String,
    /// Platform token (`linux`/`macos`/`windows`/`other`), if it has connected.
    #[serde(default)]
    pub platform: Option<String>,
    /// Whether a live connection exists on the serving pod right now.
    #[serde(default)]
    pub online: bool,
    /// The machine's last-announced capabilities (present once it has connected).
    #[serde(default)]
    pub capabilities: Option<ComputerCapabilities>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_seen_at: Option<String>,
    #[serde(default)]
    pub revoked_at: Option<String>,
}

/// Request body for `POST /computer-agents` (the API's `EnrollBody`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EnrollComputerAgent {
    pub name: String,
}

/// Response for `POST /computer-agents` (the API's `EnrolledAgent`) — the raw
/// enrollment token, shown **once**.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct EnrolledComputerAgent {
    pub id: String,
    pub name: String,
    /// The bearer token the daemon authenticates with — never shown again.
    pub token: String,
    #[serde(default)]
    pub created_at: Option<String>,
}

/// A workspace bearer token as listed (the API's `TokenView`) — id + timestamps
/// only; the secret is never listed (only its hash is stored).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct TokenView {
    /// Session id (the revoke handle).
    pub id: String,
    /// The named §19 grant this token is scoped to, if any (SOUL §19/§26) — the
    /// token acts under the grant's attenuated authority. Absent = full role.
    #[serde(default)]
    pub grant: Option<String>,
    /// When the token was issued (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// When it expires (RFC 3339).
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// Request body for `POST /tokens` (the API's `CreateToken`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateToken {
    /// Requested lifetime in days (server clamps to `[1, 365]`).
    pub ttl_days: i64,
    /// Optionally scope the token to a named §19 grant (by grant id or name), so
    /// an MCP client holds the grant's attenuated authority (SOUL §19/§26). The
    /// mint is gated: the grant must be ⊆ the caller's own authority. Omitted when
    /// empty so a role-authority mint stays a bare `{ ttl_days }` body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
}

/// `POST /tokens` response (the API's `CreatedToken`) — the raw secret, shown once.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CreatedToken {
    /// The raw bearer secret. Shown once; never recoverable afterwards.
    pub token: String,
    /// The §19 grant this token was scoped to, if any (echoes the request).
    #[serde(default)]
    pub grant: Option<String>,
    /// When it was issued (RFC 3339).
    #[serde(default)]
    pub created_at: Option<String>,
    /// When it expires (RFC 3339).
    #[serde(default)]
    pub expires_at: Option<String>,
}

/// A scripted MCP endpoint (SOUL §30) as returned by `GET /mcp-endpoints` and
/// `POST`/`PUT /mcp-endpoints/{id}`. The management panel reads the whole record
/// into its editor; the connect page only touches `id`/`name`/`description`/
/// `enabled`. Server-only fields (workspace, author, timestamps) are
/// tolerated-and-ignored, so this stays stable across server-side additions.
#[derive(Clone, Debug, PartialEq, Eq, Default, Deserialize)]
pub struct McpEndpoint {
    /// Endpoint id (the get/update/delete/mint key).
    pub id: String,
    /// URL-safe slug, unique per workspace — the `/mcp/e/{name}` path segment.
    pub name: String,
    /// Human description, shown next to the name in the endpoint picker.
    #[serde(default)]
    pub description: String,
    /// The Boa/JavaScript program: declares the endpoint's MCP tools and
    /// implements their `tools/call`.
    #[serde(default)]
    pub script: String,
    /// Search-scope pin: the bucket the script's `search_semantic` is confined
    /// to (absent = any bucket).
    #[serde(default)]
    pub bucket_name: Option<String>,
    /// Search-scope pin: the key prefix (subdir) injected into every search
    /// (absent = no prefix).
    #[serde(default)]
    pub key_prefix: Option<String>,
    /// The §19 grant whose capabilities the script runs under (absent = a
    /// minimal read-only search authority).
    #[serde(default)]
    pub grant_id: Option<String>,
    /// A disabled endpoint 404s at serve time; the list greys it out.
    #[serde(default = "bool_true")]
    pub enabled: bool,
}

/// serde default for a `bool` field that should default to `true`.
fn bool_true() -> bool {
    true
}

/// Request body for `POST /mcp-endpoints` and `PUT /mcp-endpoints/{id}` (matches
/// the API's `EndpointBody`) — create-or-update a scripted endpoint. Unset scope
/// pins / grant are omitted, which the server reads as "no pin" / default authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct McpEndpointBody {
    /// URL-safe slug, unique per workspace.
    pub name: String,
    /// One-line human description.
    pub description: String,
    /// The Boa/JavaScript program.
    pub script: String,
    /// Bucket scope pin (omitted when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket_name: Option<String>,
    /// Key-prefix scope pin (omitted when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_prefix: Option<String>,
    /// The §19 grant id to run under (omitted for the default read-only authority).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_id: Option<String>,
    /// Whether the endpoint serves (disabled → 404, still editable).
    pub enabled: bool,
}

/// Request body for `POST /mcp-endpoints/{id}/token` — mint a shareable scoped
/// URL for one endpoint. `None` takes the server default (90 days).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MintEndpointToken {
    /// Requested lifetime in days (server clamps to `[1, 365]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<i64>,
}

/// `POST /mcp-endpoints/{id}/token` response — a signed, self-verifying share
/// token plus the ready-to-use serve path (`/mcp/s/{token}`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MintedEndpointToken {
    /// The opaque signed token (rides in the URL, not a header).
    pub token: String,
    /// Ready-to-use serve path: `/mcp/s/{token}`.
    pub path: String,
    /// Absolute expiry, Unix seconds.
    #[serde(default)]
    pub expires_at: i64,
}

// ===========================================================================
// Web-fetch REST contract (SOUL §27, §12 — fetch utility).
//
// Mirrors `catalerum-api`'s `POST /fetch` (the core `FetchRequest` →
// `FetchedPage`). `format` (`markdown`/`html`/`text`) and `mode`
// (`auto`/`http`/`browser`) are the snake_case core enums, carried as plain
// strings so the form's selects map straight onto the wire tokens.
// ===========================================================================

/// Request body for `POST /fetch` (matches the core `FetchRequest`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FetchRequest {
    /// Absolute `http(s)` URL to fetch.
    pub url: String,
    /// Representation to return: `markdown` | `html` | `text`.
    pub format: String,
    /// Retrieval strategy: `auto` | `http` | `browser`.
    pub mode: String,
    /// Extract only the main article content (drop nav/header/footer).
    pub main_content_only: bool,
    /// Browser mode: CSS selector to wait for (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_for: Option<String>,
    /// Per-request timeout override in seconds (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// A fetched + normalized page (the core `FetchedPage`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct FetchedPage {
    /// The final URL after redirects.
    pub url: String,
    /// HTTP status of the final response.
    #[serde(default)]
    pub status: u16,
    /// `<title>` of the page, if any.
    #[serde(default)]
    pub title: Option<String>,
    /// Server `Content-Type`, if any.
    #[serde(default)]
    pub content_type: Option<String>,
    /// The page in the requested representation.
    #[serde(default)]
    pub content: String,
    /// Which representation `content` holds.
    #[serde(default)]
    pub format: String,
    /// Size in bytes of the original fetched HTML (pre-conversion).
    #[serde(default)]
    pub raw_bytes: u64,
    /// Size in bytes of the returned `content`.
    #[serde(default)]
    pub content_bytes: u64,
}

// ===========================================================================
// Kanban REST contract (SOUL §24, §12 — tasks board).
//
// Mirrors `catalerum-api`'s board/task routes (root-mounted: `/boards`,
// `/boards/{id}`, `/boards/{id}/tasks`, `/tasks/{id}/move`, `/tasks/{id}/status`).
// `status` is the snake_case core `TaskStatus` (`open`/`in_progress`/`blocked`/
// `done`), decoded as a plain string. `assignee` is kept opaque (just presence is
// surfaced).
// ===========================================================================

/// An ordered column within a [`Board`] (the core `Column`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Column {
    /// Column id (UUID string).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Sort position among the board's columns.
    #[serde(default)]
    pub order: i32,
}

/// A Kanban board with its ordered columns (the core `Board`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Board {
    /// Board id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Board name.
    pub name: String,
    /// The board's columns.
    #[serde(default)]
    pub columns: Vec<Column>,
}

/// A Kanban task (the core `Task`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Task {
    /// Task id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// Owning board.
    pub board_id: String,
    /// The column the task currently sits in.
    pub column_id: String,
    /// Task title.
    pub title: String,
    /// Markdown body.
    #[serde(default)]
    pub body_md: String,
    /// Assignee (user/agent), if any — kept opaque; only presence is surfaced.
    #[serde(default)]
    pub assignee: Option<serde_json::Value>,
    /// Sort position within the column.
    #[serde(default)]
    pub order: i32,
    /// Lifecycle status: `open` | `in_progress` | `blocked` | `done`.
    #[serde(default)]
    pub status: String,
}

/// Request body for `POST /boards`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateBoard {
    /// Board name.
    pub name: String,
    /// Column names (empty → the server's default column set).
    pub columns: Vec<String>,
}

/// Request body for `PUT /boards/{id}` — rename a board.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameBoard {
    /// The new board name (non-empty).
    pub name: String,
}

/// Request body for `POST /boards/{id}/tasks`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateTask {
    /// The column to create the task in.
    pub column_id: String,
    /// Task title.
    pub title: String,
    /// Markdown body.
    pub body_md: String,
}

/// Request body for `POST /tasks/{id}/move`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MoveTask {
    /// The destination column (must be in the same board).
    pub column_id: String,
    /// Final 0-based index in the destination column (clamped server-side);
    /// `None` = the end. A same-column move with a position is a reorder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<i32>,
}

/// Request body for `POST /boards/{id}/columns` — append a column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AddColumn {
    /// The new column's name (non-empty).
    pub name: String,
}

/// Request body for `PUT /columns/{id}` — rename a column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameColumn {
    /// The new column name (non-empty).
    pub name: String,
}

/// Request body for `POST /tasks/{id}/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetTaskStatus {
    /// The new status: `open` | `in_progress` | `blocked` | `done`.
    pub status: String,
}

/// Request body for `PUT /tasks/{id}` — edit a card's title + markdown body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EditTask {
    /// The new title (must be non-empty).
    pub title: String,
    /// The new markdown body.
    pub body_md: String,
}

// ===========================================================================
// Memories + profile REST contract (SOUL §22, §12 — memory manager).
//
// Mirrors `catalerum-api`'s memory routes (root-mounted: `/memories`,
// `/memories/{id}`, `/profile`). `scope` is the snake_case core `MemoryScope`
// (`user`/`workspace`), decoded as a plain string. The profile `fields` is a raw
// JSON object (the core `Map`), shown + merged as JSON.
// ===========================================================================

/// A durable free-text memory (the core `Memory`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Memory {
    /// Memory id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: String,
    /// `user` (private to a member) or `workspace` (shared).
    #[serde(default)]
    pub scope: String,
    /// The member a `user`-scoped memory belongs to, if any.
    #[serde(default)]
    pub user_id: Option<String>,
    /// The fact text.
    #[serde(default)]
    pub text: String,
    /// When the memory was created (RFC 3339 / ISO-8601 UTC).
    #[serde(default)]
    pub created_at: String,
}

/// The caller's personalization profile (the core `Profile`).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct Profile {
    /// Owning workspace.
    #[serde(default)]
    pub workspace_id: String,
    /// The user the profile belongs to.
    #[serde(default)]
    pub user_id: String,
    /// Free-form structured fields (timezone, hours, preferences, …).
    #[serde(default)]
    pub fields: serde_json::Value,
}

/// Request body for `POST /memories`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateMemory {
    /// `user` | `workspace`.
    pub scope: String,
    /// The fact text.
    pub text: String,
}

/// Request body for `PUT /memories/{id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateMemory {
    /// The replacement text.
    pub text: String,
}

// ===========================================================================
// Onboarding / quick-start contract (SOUL §12/§22/§23 — the first-run wizard).
//
// Mirrors `catalerum-api`'s `routes::onboarding` (root-mounted: `/onboarding/state`,
// `/onboarding/personalize`, `/onboarding/complete`). The wizard otherwise drives
// the existing status / llm-settings / profile / memories / skills endpoints; these
// three add the first-run signal, the LLM personalization chat, and the completion
// sentinel.
// ===========================================================================

/// `GET /onboarding/state` response (the API's `OnboardingState`) — whether the
/// caller has finished the quick-start, has a chat-model override, and still has
/// an empty profile. Drives the shell's first-run auto-open.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct OnboardingState {
    /// Whether the quick-start has been completed (the profile sentinel is set).
    #[serde(default)]
    pub completed: bool,
    /// When it was completed (RFC 3339), if ever.
    #[serde(default)]
    pub completed_at: Option<String>,
    /// Whether the caller has an explicit per-user chat-model override.
    #[serde(default)]
    pub chat_model_set: bool,
    /// Whether the profile carries no user-entered fields yet.
    #[serde(default)]
    pub profile_empty: bool,
}

/// One visible turn of the personalization chat (the API's `PersonalizeTurn`).
/// `role` is `"user"` or `"assistant"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PersonalizeTurn {
    /// `"user"` | `"assistant"`.
    pub role: String,
    /// The turn's text.
    pub content: String,
}

/// Request body for `POST /onboarding/personalize` (the API's `PersonalizeRequest`)
/// — the visible conversation so far. An empty list drives the opening turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct PersonalizeRequest {
    /// The chat so far, oldest turn first.
    pub messages: Vec<PersonalizeTurn>,
}

/// One proposed skill (the API's `SkillDraft`) — review, then persist via
/// `PUT /skills/{name}`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SkillDraft {
    /// Kebab-case, per-workspace-unique name.
    pub name: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Markdown runbook.
    #[serde(default)]
    pub instructions_md: String,
    /// Advisory tool names.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// `POST /onboarding/personalize` response (the API's `PersonalizeResponse`) — the
/// assistant's next message plus the memories/skills it proposes from the exchange.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct PersonalizeResponse {
    /// The assistant's next message (greeting + question, follow-up, or wrap-up).
    #[serde(default)]
    pub reply: String,
    /// Newly learned durable facts, as memory candidates.
    #[serde(default)]
    pub memories: Vec<String>,
    /// Newly proposed skill drafts (empty when none surfaced this turn).
    #[serde(default)]
    pub skills: Vec<SkillDraft>,
    /// The model's cue that it has gathered enough (a "Finish" nudge).
    #[serde(default)]
    pub done: bool,
}

// ===========================================================================
// Graph query REST contract (SOUL §6.3, §12 — graph explorer).
//
// Mirrors `catalerum-api`'s `POST /graph/query`. The `query` is a safe Datalog
// program the server parses, validates, and evaluates in-process over the caller's
// workspace facts — scope is structural (the language cannot name a workspace), so
// there is no injection or cross-tenant read. The response is column names + rows of
// JSON string cells, capped server-side (a partial result is flagged `truncated`,
// §18/§19).
// ===========================================================================

/// Request body for `POST /graph/query`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphQueryRequest {
    /// The Datalog program to run (optional rules + one `?- …` goal). Scope is
    /// implicit — the language cannot name a workspace.
    pub query: String,
}

/// Response from `POST /graph/query` — column names + records (each row aligned
/// to `columns`; cells are JSON strings).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct GraphQueryResponse {
    /// The goal's output column names, in order.
    #[serde(default)]
    pub columns: Vec<String>,
    /// One entry per returned record, aligned to `columns`.
    #[serde(default)]
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Whether the server capped `rows` to its hard limit (a broad read bounded for
    /// §18/§19 blast radius). When set, the result is partial — narrow the query to
    /// see the rest.
    #[serde(default)]
    pub truncated: bool,
}

// ===========================================================================
// Workspace switcher REST contract (SOUL §18, §12).
//
// Mirrors `catalerum-api`'s `GET /workspaces` (the caller's memberships) and
// `POST /auth/switch` (mint a session bound to another workspace). The switch
// response is the session — the UI only needs the new `token`.
// ===========================================================================

/// One of the caller's workspace memberships (the API's `WorkspaceMembership`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorkspaceMembership {
    /// Workspace id (UUID string) — the switch key.
    pub id: String,
    /// Workspace name.
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
    /// The caller's role here (`owner`/`admin`/`member`/`viewer`).
    #[serde(default)]
    pub role: String,
    /// The organisation this workspace belongs to (SOUL §18) — the switcher groups
    /// by it. Absent on an older server ⇒ empty, which buckets the workspace under
    /// the fallback group rather than breaking decode.
    #[serde(default)]
    pub organisation_id: String,
    /// Whether this is the active (current-session) workspace.
    #[serde(default)]
    pub active: bool,
}

/// Request body for `POST /auth/switch`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchWorkspace {
    /// The workspace to switch to.
    pub workspace_id: String,
}

/// Response from `POST /auth/switch` — the new session. Only `token` is used (the
/// rest of the `SessionResponse` is ignored).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct SwitchResponse {
    /// The new session bearer the client must adopt.
    pub token: String,
}

/// Request body for `POST /auth/exchange` — the one-time handoff code from the
/// login redirect's `?code=` param.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExchangeCode {
    /// The one-time handoff code.
    pub code: String,
}

/// Response from `POST /auth/exchange` — the session. Only `token` is used (the
/// rest of the `SessionResponse` is ignored).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ExchangeSession {
    /// The session bearer the client must adopt.
    pub token: String,
}

// ===========================================================================
// Organisations REST contract (SOUL §18) — the administrative grouping above
// workspaces. Mirrors `catalerum-api`'s `routes::organisations`:
//   GET  /organisations                       → Vec<MyOrganisation>
//   POST /organisations                       (CreateOrg)          → Organisation
//   GET  /organisations/{id}/members          → Vec<OrgMemberView>
//   POST /organisations/{id}/members          (AddOrgMember)       → OrgMemberView
//   DELETE /organisations/{id}/members/{uid}
//   PUT  /organisations/{id}/policy           (SetOrgPolicy)       → Organisation
//   POST /organisations/{id}/workspaces       (CreateOrgWorkspace) → Workspace
//
// Org roles govern administration only and confer no data access; switching into
// a workspace still goes through the unchanged `POST /auth/switch`. As elsewhere
// the shapes are re-declared here rather than depending on `catalerum-core`, and
// every read field carries a serde default so an older/leaner server never breaks
// decode.
// ===========================================================================

/// A workspace within an organisation the caller is a member of (the API's
/// `MyWorkspace`). This is a workspace the caller can actually enter (their data
/// boundary), not merely a shell an org admin administers.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MyWorkspace {
    /// Workspace id (the switch key).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
    /// The caller's workspace role token (`owner`/`admin`/`member`/`viewer`).
    #[serde(default)]
    pub role: String,
}

/// One of the caller's organisations, with the workspaces in it they can see (the
/// API's `MyOrganisation`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct MyOrganisation {
    /// Organisation id.
    pub id: String,
    /// Display name (the switcher group header).
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
    /// The caller's org role token (`owner`/`admin`/`member`).
    #[serde(default)]
    pub role: String,
    /// The org's `workspace_creation` policy (`disabled`/`admins`/`members`).
    #[serde(default)]
    pub workspace_creation: String,
    /// The server's own deny-by-default verdict on whether the caller may create a
    /// workspace here — presentation reads it, but the POST is still the authority.
    #[serde(default)]
    pub can_create_workspace: bool,
    /// The workspaces in this org the caller is a member of.
    #[serde(default)]
    pub workspaces: Vec<MyWorkspace>,
}

/// A member of an organisation, for the multi-user members panel (the API's
/// `OrgMemberView`).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct OrgMemberView {
    /// The member's user id (the remove key).
    pub user_id: String,
    /// The member's email.
    #[serde(default)]
    pub email: String,
    /// The member's display name.
    #[serde(default)]
    pub display_name: String,
    /// Org role token (`owner`/`admin`/`member`).
    #[serde(default)]
    pub role: String,
}

/// A created / updated organisation (the API's `Organisation`) — only the fields
/// the UI reads. Returned by `POST /organisations` and `PUT .../policy`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Organisation {
    /// Organisation id.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
    /// The org's `workspace_creation` policy (`disabled`/`admins`/`members`).
    #[serde(default)]
    pub workspace_creation: String,
}

/// A created workspace (the API's `Workspace`) — only the fields the UI reads.
/// Returned by `POST /organisations/{id}/workspaces`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct CreatedWorkspace {
    /// Workspace id (so the caller can switch into it).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
}

/// A workspace **shell** an org admin administers (the API's `WorkspaceShell`),
/// returned by `GET /organisations/{id}/workspaces`. This listing includes archived
/// shells: `archived_at` is present (an RFC 3339 timestamp) only when the workspace
/// has been soft-archived, so the org panel can bucket live vs. archived and offer
/// archive/restore. Absent on an older server ⇒ treated as live (SOUL §18).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorkspaceShell {
    /// Workspace id (the archive/restore key).
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// URL-safe handle.
    #[serde(default)]
    pub slug: String,
    /// When the shell was soft-archived, or `None` while live.
    #[serde(default)]
    pub archived_at: Option<String>,
}

impl WorkspaceShell {
    /// Whether this shell is currently archived (hidden from the switcher).
    #[must_use]
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }
}

/// The user resolved by `GET /organisations/{id}/user-lookup?email=…` (the API's
/// `UserLookupView`) — enough to add them by id and confirm who was matched. A
/// `404` means "no user with that email" (an opaque miss, not enumeration).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct UserLookup {
    /// The resolved user id (the add-member key).
    pub user_id: String,
    /// The matched email (server-canonical casing).
    #[serde(default)]
    pub email: String,
    /// The user's display name, if any.
    #[serde(default)]
    pub display_name: String,
}

/// Body for `POST /organisations`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateOrg {
    /// Display name.
    pub name: String,
    /// URL-safe handle (lowercased server-side).
    pub slug: String,
}

/// Body for `POST /organisations/{id}/workspaces`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CreateOrgWorkspace {
    /// Display name.
    pub name: String,
    /// URL-safe handle (lowercased server-side).
    pub slug: String,
}

/// Body for `POST /organisations/{id}/members`. The server accepts a `user_id`
/// (not an email) plus an org role token. The panel lets an admin type either a
/// user id **or** an email — an email is first resolved to a `user_id` through the
/// org-gated `user-lookup` route, so this body always carries the resolved id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AddOrgMember {
    /// The user to add / re-role.
    pub user_id: String,
    /// Org role token (`owner`/`admin`/`member`).
    pub role: String,
}

/// Body for `PUT /organisations/{id}/policy`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetOrgPolicy {
    /// New `workspace_creation` policy (`disabled`/`admins`/`members`).
    pub workspace_creation: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_shape() {
        let m = ClientChatMessage::new("c1", "hi");
        let j = serde_json::to_string(&m).unwrap();
        // The regenerate field is omitted on an ordinary turn, so the common shape
        // is unchanged.
        assert_eq!(j, r#"{"conversation_id":"c1","content":"hi"}"#);
    }

    #[test]
    fn outbound_idempotency_id_is_sent() {
        let m = ClientChatMessage::new("c1", "hi").with_user_message_id("u1");
        assert_eq!(
            serde_json::to_string(&m).unwrap(),
            r#"{"conversation_id":"c1","content":"hi","user_message_id":"u1"}"#
        );
    }

    #[test]
    fn outbound_conversation_mode_is_explicit_only_when_enabled() {
        let normal = ClientChatMessage::new("c1", "hi");
        assert_eq!(
            serde_json::to_string(&normal).unwrap(),
            r#"{"conversation_id":"c1","content":"hi"}"#
        );

        let spoken = normal.with_conversation_mode(true);
        assert_eq!(
            serde_json::to_string(&spoken).unwrap(),
            r#"{"conversation_id":"c1","content":"hi","conversation_mode":true}"#
        );

        let legacy: ClientChatMessage =
            serde_json::from_str(r#"{"conversation_id":"c1","content":"hi"}"#).unwrap();
        assert!(!legacy.conversation_mode);
    }

    #[test]
    fn outbound_regenerate_shape() {
        let m = ClientChatMessage::regenerate("c1", "hi", "m7");
        let j = serde_json::to_string(&m).unwrap();
        assert_eq!(
            j,
            r#"{"conversation_id":"c1","content":"hi","regenerate_from":"m7"}"#
        );
    }

    #[test]
    fn token_text_delta_is_appended() {
        // The API's actual per-delta shape: a `text_delta` nested in `token`.
        assert_eq!(
            parse_frame(r#"{"type":"token","event":{"type":"text_delta","text":"ab"}}"#),
            StreamUpdate::Append("ab".into())
        );
    }

    #[test]
    fn token_inner_done_is_ignored() {
        // The inner stream `done` is informational; the turn ends on the outer
        // `message_done`, so an inner done must not finalize the UI early.
        assert_eq!(
            parse_frame(r#"{"type":"token","event":{"type":"done","finish_reason":"stop"}}"#),
            StreamUpdate::Ignore
        );
    }

    #[test]
    fn message_done_finalizes() {
        // The API's terminal frame carries message_id/conversation_id/content; an
        // omitted `truncated`/`cost_usd`/token fields default to false/None
        // (back-compat with older servers that don't send them).
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","conversation_id":"c1","content":"hi"}"#
            ),
            StreamUpdate::Done {
                truncated: false,
                stopped: false,
                cost_usd: None,
                tokens: None,
                user_message_id: None,
                content: Some("hi".into()),
                reconcile: false,
            }
        );
        // A truncated terminal frame carries `truncated: true` through to the UI.
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","conversation_id":"c1","content":"hi","truncated":true}"#
            ),
            StreamUpdate::Done {
                truncated: true,
                stopped: false,
                cost_usd: None,
                tokens: None,
                user_message_id: None,
                content: Some("hi".into()),
                reconcile: false,
            }
        );
        // The per-turn cost rides through when the server reports it.
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","conversation_id":"c1","content":"hi","cost_usd":0.0123}"#
            ),
            StreamUpdate::Done {
                truncated: false,
                stopped: false,
                cost_usd: Some(0.0123),
                tokens: None,
                user_message_id: None,
                content: Some("hi".into()),
                reconcile: false,
            }
        );
        // Token + cache accounting rides through when the server reports usage.
        // `cached_tokens`/`cache_creation_tokens` omitted by the server's
        // skip-zero serialization decode to 0 (usage was still present).
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","conversation_id":"c1","content":"hi","prompt_tokens":1200,"completion_tokens":340,"total_tokens":1540,"cached_tokens":800}"#
            ),
            StreamUpdate::Done {
                truncated: false,
                stopped: false,
                cost_usd: None,
                tokens: Some(TurnTokens {
                    prompt_tokens: 1200,
                    completion_tokens: 340,
                    total_tokens: 1540,
                    cached_tokens: 800,
                    cache_creation_tokens: 0,
                }),
                user_message_id: None,
                content: Some("hi".into()),
                reconcile: false,
            }
        );
        // The anchoring user message id rides through so the just-sent user line
        // can be made regeneratable.
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","user_message_id":"u9","conversation_id":"c1","content":"hi"}"#
            ),
            StreamUpdate::Done {
                truncated: false,
                stopped: false,
                cost_usd: None,
                tokens: None,
                user_message_id: Some("u9".into()),
                content: Some("hi".into()),
                reconcile: false,
            }
        );
    }

    #[test]
    fn message_done_carries_the_stopped_flag() {
        // A user-stopped turn: the terminal frame flags the deliberate partial so
        // the panel can note it and return any still-queued drafts to the composer.
        assert_eq!(
            parse_frame(
                r#"{"type":"message_done","message_id":"m1","conversation_id":"c1","content":"hi","stopped":true}"#
            ),
            StreamUpdate::Done {
                truncated: false,
                stopped: true,
                cost_usd: None,
                tokens: None,
                user_message_id: None,
                content: Some("hi".into()),
                reconcile: false,
            }
        );
    }

    #[test]
    fn synthetic_done_requests_durable_reconciliation() {
        let update = parse_frame(r#"{"type":"message_done","content":"","reconcile":true}"#);
        assert!(matches!(
            update,
            StreamUpdate::Done {
                reconcile: true,
                ..
            }
        ));
    }

    #[test]
    fn user_message_frame_yields_user_placed() {
        // The server's persisted-user-message ack: stamps the optimistic line
        // with its server id (and clears "queued" styling on a mid-turn send).
        assert_eq!(
            parse_frame(r#"{"type":"user_message","message_id":"u3","conversation_id":"c1"}"#),
            StreamUpdate::UserPlaced {
                message_id: "u3".into()
            }
        );
    }

    #[test]
    fn ui_artifact_frame_yields_ui_update() {
        // The server ships ui_id + version + the full definition; the client keeps
        // only the id (it re-fetches the cached definition) and ignores the rest.
        assert_eq!(
            parse_frame(
                r#"{"type":"ui_artifact","ui_id":"abc","version":3,"definition":{"id":"abc","title":"x"}}"#
            ),
            StreamUpdate::Ui {
                id: "abc".into(),
                version: 3
            }
        );
    }

    #[test]
    fn token_inner_error_surfaces() {
        assert_eq!(
            parse_frame(r#"{"type":"token","event":{"type":"error","message":"boom"}}"#),
            StreamUpdate::Error("boom".into())
        );
    }

    #[test]
    fn approval_request_frame_yields_approval_requested() {
        let u = parse_frame(
            r#"{"type":"approval_request","id":"ap-2","tool":"delete_object","arguments":{"key":"x"},"reason":"a delete"}"#,
        );
        match u {
            StreamUpdate::ApprovalRequested {
                id, tool, reason, ..
            } => {
                assert_eq!(id, "ap-2");
                assert_eq!(tool, "delete_object");
                assert_eq!(reason, "a delete");
            }
            other => panic!("expected ApprovalRequested, got {other:?}"),
        }
    }

    #[test]
    fn agent_profile_decodes_with_and_without_a_guard() {
        // A profile with no guard key (older server / unguarded) still decodes.
        let p: AgentProfile =
            serde_json::from_str(r#"{"id":"i","workspace_id":"w","name":"n"}"#).unwrap();
        assert!(p.guard.is_none());

        // A full guard round-trips (script + declarative LLM + snake_case on_error).
        let p: AgentProfile = serde_json::from_str(
            r#"{"id":"i","workspace_id":"w","name":"n","guard":{"script":"return 'deny';","llm":{"instruction":"no prod"},"on_error":"ask"}}"#,
        )
        .unwrap();
        let g = p.guard.unwrap();
        assert_eq!(g.script.as_deref(), Some("return 'deny';"));
        assert_eq!(g.on_error, GuardFail::Ask);
        assert_eq!(g.llm.unwrap().instruction, "no prod");
    }

    #[test]
    fn turn_level_error_frame() {
        assert_eq!(
            parse_frame(r#"{"type":"error","message":"boom"}"#),
            StreamUpdate::Error("boom".into())
        );
    }

    #[test]
    fn token_tool_call_delta_is_ignored() {
        assert_eq!(
            parse_frame(
                r#"{"type":"token","event":{"type":"tool_call_delta","index":0,"name":"search"}}"#
            ),
            StreamUpdate::Ignore
        );
    }

    #[test]
    fn token_tool_call_started_yields_tool_started() {
        let u = parse_frame(
            r#"{"type":"token","event":{"type":"tool_call_started","id":"c1","name":"web_search","arguments":"{\"queries\":[\"rust\"]}"}}"#,
        );
        match u {
            StreamUpdate::ToolStarted {
                id,
                name,
                arguments,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "web_search");
                assert_eq!(arguments, r#"{"queries":["rust"]}"#);
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn token_tool_result_yields_tool_result() {
        // Full shape (duration + truncated) and the minimal shape (defaults).
        let u = parse_frame(
            r#"{"type":"token","event":{"type":"tool_result","id":"c1","name":"web_search","result":"{}","is_error":false,"duration_ms":412,"truncated":true}}"#,
        );
        match u {
            StreamUpdate::ToolResult {
                id,
                name,
                is_error,
                duration_ms,
                truncated,
                ..
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "web_search");
                assert!(!is_error);
                assert_eq!(duration_ms, Some(412));
                assert!(truncated);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
        // Minimal: omitted duration_ms/truncated default to None/false.
        let u = parse_frame(
            r#"{"type":"token","event":{"type":"tool_result","id":"c2","name":"x","result":"{\"error\":\"boom\"}","is_error":true}}"#,
        );
        match u {
            StreamUpdate::ToolResult {
                is_error,
                duration_ms,
                truncated,
                ..
            } => {
                assert!(is_error);
                assert_eq!(duration_ms, None);
                assert!(!truncated);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn unknown_outer_tag_is_ignored() {
        assert_eq!(parse_frame(r#"{"type":"heartbeat"}"#), StreamUpdate::Ignore);
    }

    #[test]
    fn malformed_is_error() {
        assert!(matches!(parse_frame("not json"), StreamUpdate::Error(_)));
    }

    #[test]
    fn create_connection_local_shape() {
        let body = CreateConnection::new(CalendarProviderKind::Local, "Home", "/srv/cal");
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["kind"], "local");
        assert_eq!(j["name"], "Home");
        assert_eq!(j["config"]["dir"], "/srv/cal");
        assert!(j.get("credentials").is_none());
    }

    #[test]
    fn create_connection_remote_uses_base_url() {
        let body = CreateConnection::new(
            CalendarProviderKind::Caldav,
            "Work",
            "https://dav.example/cal",
        );
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["kind"], "caldav");
        assert_eq!(j["config"]["base_url"], "https://dav.example/cal");
    }

    #[test]
    fn provider_kind_round_trips() {
        for k in [
            CalendarProviderKind::Local,
            CalendarProviderKind::Caldav,
            CalendarProviderKind::Webcal,
        ] {
            assert_eq!(CalendarProviderKind::parse_token(k.as_str()), Some(k));
        }
        assert_eq!(CalendarProviderKind::parse_token("nope"), None);
    }

    #[test]
    fn event_decodes_from_api_shape() {
        let ev: Event = serde_json::from_str(
            r#"{"id":"e1","workspace_id":"w1","calendar_id":"c1","uid":"u1",
                "start":"2026-06-13T09:00:00Z","end":"2026-06-13T10:00:00Z",
                "summary":"Standup","location":"Room 4","attendees":[],"sequence":0}"#,
        )
        .unwrap();
        assert_eq!(ev.summary, "Standup");
        assert_eq!(ev.start, "2026-06-13T09:00:00Z");
        assert_eq!(ev.location.as_deref(), Some("Room 4"));
        assert!(ev.rrule.is_none());
    }

    #[test]
    fn note_decodes_from_api_shape() {
        let note: Note = serde_json::from_str(
            r#"{"id":"n1","workspace_id":"w1",
                "author":{"kind":"user","id":"u1"},
                "title":"Groceries","markdown":"- milk\n- eggs",
                "tags":["home"],"updated_at":"2026-06-14T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(note.title, "Groceries");
        assert_eq!(note.author.kind, "user");
        assert_eq!(note.tags, vec!["home".to_string()]);
        assert_eq!(note.updated_at, "2026-06-14T09:00:00Z");
    }

    #[test]
    fn note_decodes_with_defaulted_body_and_tags() {
        // A title-only note (empty markdown/tags omitted on the wire) still decodes.
        let note: Note = serde_json::from_str(
            r#"{"id":"n2","workspace_id":"w1","author":{"kind":"agent","id":"a1"},
                "title":"Draft","updated_at":"2026-06-14T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(note.markdown.is_empty());
        assert!(note.tags.is_empty());
        assert_eq!(note.author.kind, "agent");
    }

    #[test]
    fn create_note_body_shape() {
        let body = CreateNote {
            title: "T".into(),
            markdown: "body".into(),
            tags: vec!["a".into()],
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["title"], "T");
        assert_eq!(j["markdown"], "body");
        assert_eq!(j["tags"][0], "a");
    }

    #[test]
    fn storage_object_decodes_from_catalogue_shape() {
        // The API omits null optionals (`skip_serializing_if`); a sparse,
        // not-yet-ingested object must still decode.
        let o: StorageObject = serde_json::from_str(
            r#"{"id":"o1","bucket":"local","key":"docs/report.pdf","size":2048,
                "content_type":"application/pdf","last_modified":"2026-06-14T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(o.key, "docs/report.pdf");
        assert_eq!(o.size, 2048);
        assert_eq!(o.content_type.as_deref(), Some("application/pdf"));
        assert!(o.etag.is_none());
        assert!(!o.is_ingested());

        // A fully-populated, ingested object.
        let o2: StorageObject = serde_json::from_str(
            r#"{"id":"o2","bucket":"local","key":"a.txt","size":3,
                "etag":"\"abc\"","last_modified":"2026-06-14T10:00:00Z",
                "sha256":"deadbeef","extracted_text_id":"d9"}"#,
        )
        .unwrap();
        assert!(o2.is_ingested());
        assert_eq!(o2.extracted_text_id.as_deref(), Some("d9"));
        assert_eq!(o2.sha256.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn skill_decodes_and_defaults_optionals() {
        // A name-only skill (empty fields omitted on the wire) still decodes.
        let s: Skill =
            serde_json::from_str(r#"{"id":"s1","workspace_id":"w1","name":"triage"}"#).unwrap();
        assert_eq!(s.name, "triage");
        assert!(s.description.is_empty());
        assert!(s.tools.is_empty());
        assert!(s.code.is_none());
        assert!(s.advertised, "visible to agent by default");

        // A fully-populated skill with code.
        let s2: Skill = serde_json::from_str(
            r#"{"id":"s2","workspace_id":"w1","name":"run","description":"runs",
                "instructions_md":"do the thing","tools":["read_note"],
                "code":{"language":"python","source":"print(1)"}}"#,
        )
        .unwrap();
        assert_eq!(s2.tools, vec!["read_note".to_string()]);
        let code = s2.code.unwrap();
        assert_eq!(code.language, "python");
        assert!(code.entrypoint.is_none());
    }

    #[test]
    fn create_skill_omits_absent_code() {
        let body = CreateSkill {
            name: "summarize".into(),
            description: "d".into(),
            instructions_md: "i".into(),
            tools: vec!["read_note".into()],
            code: None,
            advertised: false,
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["name"], "summarize");
        assert_eq!(j["tools"][0], "read_note");
        // No code → the key is absent (not `null`), so the API default applies.
        assert!(j.get("code").is_none());
        // The opt-out always rides the wire (the API default is true).
        assert_eq!(j["advertised"], false);
    }

    #[test]
    fn update_skill_includes_code_when_present() {
        let body = UpdateSkill {
            description: "d".into(),
            instructions_md: "i".into(),
            tools: vec![],
            code: Some(Code {
                language: "bash".into(),
                source: "echo hi".into(),
                entrypoint: None,
            }),
            advertised: true,
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["code"]["language"], "bash");
        assert_eq!(j["code"]["source"], "echo hi");
        // entrypoint None is skipped.
        assert!(j["code"].get("entrypoint").is_none());
    }

    #[test]
    fn automation_decodes_and_ignores_grant_id() {
        // The API includes `grant_id`; the UI shape omits it — serde ignores the
        // extra field (no deny_unknown_fields). Empty lists decode by default.
        let a: Automation = serde_json::from_str(
            r#"{"id":"a1","workspace_id":"w1","name":"daily","enabled":true,
                "triggers":[{"kind":"schedule","cron":"0 9 * * *"}],
                "actions":[{"kind":"summarize"}],"grant_id":"g1"}"#,
        )
        .unwrap();
        assert_eq!(a.name, "daily");
        assert!(a.enabled);
        assert_eq!(a.triggers.len(), 1);
        assert_eq!(a.triggers[0]["kind"], "schedule");
        assert!(a.condition.is_none());

        let bare: Automation =
            serde_json::from_str(r#"{"id":"a2","workspace_id":"w1","name":"x"}"#).unwrap();
        assert!(!bare.enabled);
        assert!(bare.triggers.is_empty() && bare.actions.is_empty());
    }

    #[test]
    fn automation_run_decodes_running_and_failed() {
        let running: AutomationRun = serde_json::from_str(
            r#"{"id":"r1","status":"running","started_at":"2026-06-18T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(running.status, "running");
        assert!(running.finished_at.is_none());
        assert!(running.error.is_none());

        let failed: AutomationRun = serde_json::from_str(
            r#"{"id":"r2","status":"failed","started_at":"2026-06-18T09:00:00Z",
                "finished_at":"2026-06-18T09:00:05Z","error":"boom",
                "trigger":{"kind":"schedule"}}"#,
        )
        .unwrap();
        assert_eq!(failed.status, "failed");
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert_eq!(failed.trigger.unwrap()["kind"], "schedule");
    }

    #[test]
    fn create_automation_omits_absent_condition_and_spec() {
        let body = CreateAutomation {
            name: "daily".into(),
            enabled: true,
            triggers: vec![serde_json::json!({"kind":"schedule","cron":"0 9 * * *"})],
            condition: None,
            actions: vec![serde_json::json!({"kind":"summarize"})],
            spec: None,
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["name"], "daily");
        assert_eq!(j["triggers"][0]["cron"], "0 9 * * *");
        // Absent optionals are omitted (not null) → the API defaults apply.
        assert!(j.get("condition").is_none());
        assert!(j.get("spec").is_none());
        // enabled + actions are always present.
        assert_eq!(j["enabled"], true);
        assert_eq!(j["actions"][0]["kind"], "summarize");
    }

    #[test]
    fn set_enabled_shape() {
        let j = serde_json::to_value(SetEnabled { enabled: false }).unwrap();
        assert_eq!(j["enabled"], false);
    }

    #[test]
    fn collect_now_result_decodes_job_id() {
        let r: CollectNowResult =
            serde_json::from_str(r#"{"job":"3f1e0c9a-0000-4000-8000-000000000001"}"#).unwrap();
        assert_eq!(r.job, "3f1e0c9a-0000-4000-8000-000000000001");
    }

    #[test]
    fn fire_result_decodes_matched_and_jobs() {
        let r: FireResult = serde_json::from_str(r#"{"matched":2,"jobs":["j1","j2"]}"#).unwrap();
        assert_eq!(r.matched, 2);
        assert_eq!(r.jobs, vec!["j1".to_string(), "j2".to_string()]);
        // A `202` with no matches decodes to zero + an empty job list.
        let none: FireResult = serde_json::from_str(r#"{"matched":0,"jobs":[]}"#).unwrap();
        assert_eq!(none.matched, 0);
        assert!(none.jobs.is_empty());
    }

    #[test]
    fn run_detail_decodes_run_and_steps() {
        let d: RunDetail = serde_json::from_str(
            r#"{"run":{"id":"r1","status":"succeeded","started_at":"2026-06-18T09:00:00Z",
                       "finished_at":"2026-06-18T09:00:02Z"},
                "steps":[
                  {"id":"s1","ordinal":0,"action":{"kind":"summarize"},"status":"succeeded",
                   "output":{"ok":true},"started_at":"2026-06-18T09:00:00Z",
                   "finished_at":"2026-06-18T09:00:01Z"},
                  {"id":"s2","ordinal":1,"action":{"kind":"notify"},"status":"failed",
                   "error":"channel down","started_at":"2026-06-18T09:00:01Z"}
                ]}"#,
        )
        .unwrap();
        assert_eq!(d.run.status, "succeeded");
        assert_eq!(d.steps.len(), 2);
        assert_eq!(d.steps[0].action["kind"], "summarize");
        assert_eq!(d.steps[0].status, "succeeded");
        assert_eq!(d.steps[0].output.as_ref().unwrap()["ok"], true);
        assert_eq!(d.steps[1].status, "failed");
        assert_eq!(d.steps[1].error.as_deref(), Some("channel down"));
        assert!(d.steps[1].finished_at.is_none());
    }

    #[test]
    fn grant_decodes_capabilities_and_constraints() {
        let g: Grant = serde_json::from_str(
            r#"{"id":"g1","workspace_id":"w1","name":"ops",
                "capabilities":[
                    {"action":"read","resource":{"domain":"notes"}},
                    {"action":"run","resource":{"domain":"exec","selector":"bao"},
                     "constraints":{"lang":"python"}}],
                "constraints":{"dry_run":true,"env_allow":["dev"]}}"#,
        )
        .unwrap();
        assert_eq!(g.name, "ops");
        assert_eq!(g.capabilities.len(), 2);
        assert_eq!(g.capabilities[0].action, Action::Read);
        assert_eq!(g.capabilities[0].resource.domain, "notes");
        assert!(g.capabilities[0].resource.selector.is_none());
        assert_eq!(g.capabilities[1].action, Action::Run);
        assert_eq!(g.capabilities[1].resource.selector.as_deref(), Some("bao"));
        assert_eq!(g.capabilities[1].constraints["lang"], "python");
        assert_eq!(g.constraints["dry_run"], true);
    }

    #[test]
    fn capability_serializes_in_core_shape() {
        let cap = Capability {
            action: Action::Write,
            resource: Resource {
                domain: "storage".into(),
                selector: Some("local/out/*".into()),
            },
            constraints: serde_json::Map::new(),
        };
        let j = serde_json::to_value(&cap).unwrap();
        assert_eq!(j["action"], "write");
        assert_eq!(j["resource"]["domain"], "storage");
        assert_eq!(j["resource"]["selector"], "local/out/*");
        // Empty per-capability constraints are omitted.
        assert!(j.get("constraints").is_none());
    }

    #[test]
    fn create_grant_shape() {
        let body = CreateGrant {
            name: "ops".into(),
            capabilities: vec![Capability {
                action: Action::Any,
                resource: Resource {
                    domain: "*".into(),
                    selector: None,
                },
                constraints: serde_json::Map::new(),
            }],
            constraints: serde_json::json!({}),
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["name"], "ops");
        assert_eq!(j["capabilities"][0]["action"], "any");
        assert_eq!(j["capabilities"][0]["resource"]["domain"], "*");
        assert!(j["capabilities"][0]["resource"].get("selector").is_none());
        assert_eq!(j["constraints"], serde_json::json!({}));
    }

    #[test]
    fn conversation_decodes_with_and_without_title() {
        let titled: Conversation = serde_json::from_str(
            r#"{"id":"c1","workspace_id":"w1","title":"Trip planning","origin":"web"}"#,
        )
        .unwrap();
        assert_eq!(titled.title.as_deref(), Some("Trip planning"));
        assert_eq!(titled.origin, "web");

        let untitled: Conversation =
            serde_json::from_str(r#"{"id":"c2","workspace_id":"w1","origin":"channel"}"#).unwrap();
        assert!(untitled.title.is_none());
        assert_eq!(untitled.origin, "channel");
    }

    #[test]
    fn message_decodes_user_and_assistant_with_tool_calls() {
        let user: Message = serde_json::from_str(
            r#"{"id":"m1","conversation_id":"c1","role":"user","content":"hi",
                "created_at":"2026-06-18T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "hi");
        assert!(user.tool_calls.is_empty());

        let asst: Message = serde_json::from_str(
            r#"{"id":"m2","conversation_id":"c1","role":"assistant","content":"",
                "tool_calls":[{"id":"t1","name":"search","arguments":"{\"q\":\"x\"}"}],
                "created_at":"2026-06-18T09:00:01Z"}"#,
        )
        .unwrap();
        assert_eq!(asst.tool_calls.len(), 1);
        assert_eq!(asst.tool_calls[0].name, "search");
        assert_eq!(asst.tool_calls[0].arguments, r#"{"q":"x"}"#);
    }

    #[test]
    fn email_view_and_detail_decode() {
        let row: EmailView = serde_json::from_str(
            r#"{"id":"e1","mailbox":"INBOX","from":"Ada <ada@x.com>","subject":"Hi",
                "received_at":"2026-06-18T09:00:00Z","unread":true,"has_attachments":false}"#,
        )
        .unwrap();
        assert_eq!(row.mailbox, "INBOX");
        assert_eq!(row.from.as_deref(), Some("Ada <ada@x.com>"));
        assert!(row.unread);
        // A row with no cross-folder fields (single-filed / older server) → count 1, empty.
        assert_eq!(row.folder_count, 1);
        assert!(row.also_in.is_empty());

        // A cross-filed row carries folder_count + the other folders (SOUL §29).
        let dup: EmailView = serde_json::from_str(
            r#"{"id":"e9","mailbox":"INBOX","subject":"Dup","unread":false,
                "has_attachments":false,"folder_count":3,"also_in":["Archive","Sent"]}"#,
        )
        .unwrap();
        assert_eq!(dup.folder_count, 3);
        assert_eq!(dup.also_in, vec!["Archive".to_string(), "Sent".to_string()]);

        // A sparse detail (no body / no from) still decodes; an older server that
        // omits `attachments` / `raw_ref` entirely → empty / none (serde-additive).
        let detail: EmailDetail = serde_json::from_str(
            r#"{"id":"e2","mailbox":"INBOX","subject":"Re: Hi",
                "to":[{"name":"Bob","address":"bob@x.com"},{"address":"c@x.com"}],
                "unread":false,"has_attachments":true}"#,
        )
        .unwrap();
        assert_eq!(detail.to.len(), 2);
        assert!(detail.from.is_none());
        assert!(detail.body_text.is_none());
        assert!(detail.has_attachments);
        assert!(detail.attachments.is_empty());
        assert!(detail.raw_ref.is_none());
        // No cross-folder fields → single-filed default.
        assert_eq!(detail.folder_count, 1);
        assert!(detail.also_in.is_empty());

        // A newer server projects archived attachment refs + the raw `.eml` key.
        let with_atts: EmailDetail = serde_json::from_str(
            r#"{"id":"e3","mailbox":"INBOX","subject":"Invoice","unread":false,
                "has_attachments":true,"raw_ref":"emails/mb1/42/raw.eml",
                "attachments":[
                    {"url":"/storage/objects/emails/mb1/42/attachments/1-invoice.pdf",
                     "filename":"invoice.pdf","content_type":"application/pdf","size":8192},
                    {"url":"/storage/objects/emails/mb1/42/attachments/2-logo.png"}
                ]}"#,
        )
        .unwrap();
        assert_eq!(with_atts.attachments.len(), 2);
        assert_eq!(
            with_atts.attachments[0].filename.as_deref(),
            Some("invoice.pdf")
        );
        assert_eq!(with_atts.attachments[0].size, Some(8192));
        assert!(with_atts.attachments[1].filename.is_none());
        assert_eq!(with_atts.raw_ref.as_deref(), Some("emails/mb1/42/raw.eml"));
    }

    #[test]
    fn email_address_display() {
        assert_eq!(
            EmailAddress {
                name: Some("Ada".into()),
                address: "ada@x.com".into()
            }
            .display(),
            "Ada <ada@x.com>"
        );
        assert_eq!(
            EmailAddress {
                name: None,
                address: "bob@x.com".into()
            }
            .display(),
            "bob@x.com"
        );
        // A blank name degrades to the bare address.
        assert_eq!(
            EmailAddress {
                name: Some("  ".into()),
                address: "c@x.com".into()
            }
            .display(),
            "c@x.com"
        );
    }

    #[test]
    fn email_provider_kind_tokens_round_trip() {
        for k in [
            EmailProviderKind::Maildir,
            EmailProviderKind::Imap,
            EmailProviderKind::Jmap,
            EmailProviderKind::Gmail,
        ] {
            assert_eq!(EmailProviderKind::parse_token(k.as_str()), Some(k));
        }
        assert_eq!(EmailProviderKind::parse_token("smtp"), None);
    }

    #[test]
    fn create_email_connection_serializes_maildir() {
        // A maildir source serializes its provider as the snake_case token and
        // carries root + a set mailbox.
        let body = CreateEmailConnection {
            provider: EmailProviderKind::Maildir,
            name: "Inbox".into(),
            root: "/var/mail/me".into(),
            mailbox: Some("Archive".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v["provider"], "maildir");
        assert_eq!(v["name"], "Inbox");
        assert_eq!(v["root"], "/var/mail/me");
        assert_eq!(v["mailbox"], "Archive");

        // An unset (None) mailbox is omitted on the wire, so the server defaults it
        // to INBOX. (Blank-to-None trimming is the settings form's `opt` helper's
        // job, not this contract type's.)
        let body = CreateEmailConnection {
            provider: EmailProviderKind::Maildir,
            name: "Inbox".into(),
            root: "/m".into(),
            mailbox: None,
            ..Default::default()
        };
        let v = serde_json::to_value(&body).unwrap();
        assert!(
            v.get("mailbox").is_none(),
            "an unset mailbox is omitted on the wire"
        );
    }

    #[test]
    fn login_status_decodes_and_defaults() {
        let full: LoginStatusInfo = serde_json::from_value(serde_json::json!({
            "sso": true,
            "sso_login_url": "https://sso.example.com/auth/sso/login",
            "mode": "multi_user"
        }))
        .unwrap();
        assert!(full.sso);
        assert_eq!(
            full.sso_login_url.as_deref(),
            Some("https://sso.example.com/auth/sso/login")
        );
        assert_eq!(full.mode, "multi_user");
        // An older server omitting fields must not break the login page.
        let empty: LoginStatusInfo = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!empty.sso);
        assert_eq!(empty.sso_login_url, None);
        assert_eq!(empty.mode, "single_user");
    }

    #[test]
    fn status_info_decodes() {
        let s: StatusInfo = serde_json::from_str(
            r#"{
                "version": "0.1.0",
                "llm": {"base_url": "http://localhost:8088", "default_model": "echo",
                        "embedding_model": "echo", "speech_model": "echo",
                        "transcription_model": "echo"},
                "services": [
                    {"name": "Postgres", "state": "up", "detail": "source of truth"},
                    {"name": "Neo4j (graph)", "state": "disabled", "detail": "not configured"}
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(s.version, "0.1.0");
        assert_eq!(s.llm.base_url, "http://localhost:8088");
        assert_eq!(s.llm.default_model, "echo");
        assert_eq!(s.services.len(), 2);
        assert_eq!(s.services[0].state, "up");
        assert_eq!(s.services[1].state, "disabled");
        // Absent `sso` decodes to the no-SSO default.
        assert!(!s.sso);
        // Older servers did not expose dynamic topology as a capability.
        assert!(!s.llm_control_plane);
        // An older server omitting `ocr_engines` decodes to "OCR off".
        assert!(s.llm.ocr_engines.is_empty());
    }

    #[test]
    fn token_contract_round_trips() {
        // List view: id + timestamps, no secret; grant absent = role-authority token.
        let v: TokenView = serde_json::from_str(
            r#"{"id": "abc", "created_at": "2026-06-18T09:00:00Z", "expires_at": "2026-09-16T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(v.id, "abc");
        assert_eq!(v.expires_at.as_deref(), Some("2026-09-16T09:00:00Z"));
        assert_eq!(v.grant, None);

        // A scoped token surfaces its bound grant name.
        let scoped: TokenView =
            serde_json::from_str(r#"{"id": "def", "grant": "notes-writer"}"#).unwrap();
        assert_eq!(scoped.grant.as_deref(), Some("notes-writer"));

        // Create body serializes ttl_days; a grantless mint OMITS `grant` (bare body).
        let body = CreateToken {
            ttl_days: 30,
            grant: None,
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["ttl_days"], 30);
        assert!(
            j.get("grant").is_none(),
            "no grant field on a role-authority mint"
        );

        // A scoped mint carries the grant reference.
        let scoped_body = CreateToken {
            ttl_days: 30,
            grant: Some("notes-writer".into()),
        };
        assert_eq!(
            serde_json::to_value(&scoped_body).unwrap()["grant"],
            "notes-writer"
        );

        // Created response carries the one-time secret (+ optional grant echo).
        let c: CreatedToken = serde_json::from_str(
            r#"{"token": "secret-xyz", "grant": "notes-writer", "created_at": null, "expires_at": null}"#,
        )
        .unwrap();
        assert_eq!(c.token, "secret-xyz");
        assert_eq!(c.grant.as_deref(), Some("notes-writer"));
    }

    #[test]
    fn fetch_request_omits_absent_optionals() {
        let body = FetchRequest {
            url: "https://example.com".into(),
            format: "markdown".into(),
            mode: "auto".into(),
            main_content_only: true,
            wait_for: None,
            timeout_secs: None,
        };
        let j = serde_json::to_value(&body).unwrap();
        assert_eq!(j["url"], "https://example.com");
        assert_eq!(j["format"], "markdown");
        assert_eq!(j["mode"], "auto");
        assert_eq!(j["main_content_only"], true);
        assert!(j.get("wait_for").is_none());
        assert!(j.get("timeout_secs").is_none());
    }

    #[test]
    fn fetched_page_decodes() {
        let p: FetchedPage = serde_json::from_str(
            r#"{"url":"https://example.com/","status":200,"title":"Example",
                "content_type":"text/html","content":"Example heading","format":"markdown",
                "raw_bytes":4096,"content_bytes":120}"#,
        )
        .unwrap();
        assert_eq!(p.status, 200);
        assert_eq!(p.title.as_deref(), Some("Example"));
        assert_eq!(p.content, "Example heading");
        assert_eq!(p.raw_bytes, 4096);
        assert_eq!(p.content_bytes, 120);
    }

    #[test]
    fn board_and_task_decode() {
        let board: Board = serde_json::from_str(
            r#"{"id":"b1","workspace_id":"w1","name":"Roadmap",
                "columns":[{"id":"c1","name":"To-do","order":0},
                           {"id":"c2","name":"Done","order":1}]}"#,
        )
        .unwrap();
        assert_eq!(board.name, "Roadmap");
        assert_eq!(board.columns.len(), 2);
        assert_eq!(board.columns[1].name, "Done");

        let task: Task = serde_json::from_str(
            r#"{"id":"t1","workspace_id":"w1","board_id":"b1","column_id":"c1",
                "title":"Ship","body_md":"do it","order":0,"status":"in_progress"}"#,
        )
        .unwrap();
        assert_eq!(task.title, "Ship");
        assert_eq!(task.status, "in_progress");
        assert!(task.assignee.is_none());
    }

    #[test]
    fn create_task_and_status_shapes() {
        let j = serde_json::to_value(CreateTask {
            column_id: "c1".into(),
            title: "x".into(),
            body_md: String::new(),
        })
        .unwrap();
        assert_eq!(j["column_id"], "c1");
        assert_eq!(j["title"], "x");

        let s = serde_json::to_value(SetTaskStatus {
            status: "done".into(),
        })
        .unwrap();
        assert_eq!(s["status"], "done");
    }

    #[test]
    fn memory_and_profile_decode() {
        let user_mem: Memory = serde_json::from_str(
            r#"{"id":"m1","workspace_id":"w1","scope":"user","user_id":"u1",
                "text":"prefers tea","created_at":"2026-06-18T09:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(user_mem.scope, "user");
        assert_eq!(user_mem.user_id.as_deref(), Some("u1"));
        assert_eq!(user_mem.text, "prefers tea");

        // A workspace memory has no user_id.
        let ws_mem: Memory = serde_json::from_str(
            r#"{"id":"m2","workspace_id":"w1","scope":"workspace",
                "text":"office in NYC","created_at":"2026-06-18T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(ws_mem.user_id.is_none());

        let profile: Profile =
            serde_json::from_str(r#"{"workspace_id":"w1","user_id":"u1","fields":{"tz":"UTC"}}"#)
                .unwrap();
        assert_eq!(profile.fields["tz"], "UTC");
    }

    #[test]
    fn create_memory_shape() {
        let j = serde_json::to_value(CreateMemory {
            scope: "workspace".into(),
            text: "x".into(),
        })
        .unwrap();
        assert_eq!(j["scope"], "workspace");
        assert_eq!(j["text"], "x");
    }

    #[test]
    fn graph_query_response_decodes() {
        let r: GraphQueryResponse =
            serde_json::from_str(r#"{"columns":["label","n"],"rows":[["Note",3],["Person",1]]}"#)
                .unwrap();
        assert_eq!(r.columns, vec!["label".to_string(), "n".to_string()]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], "Note");
        assert_eq!(r.rows[0][1], 3);
        // `truncated` defaults false when the server omits it (older payloads).
        assert!(!r.truncated);
        // …and decodes when present (a capped, partial result).
        let capped: GraphQueryResponse =
            serde_json::from_str(r#"{"columns":[],"rows":[],"truncated":true}"#).unwrap();
        assert!(capped.truncated);
        // An empty result still decodes (defaulted fields).
        let empty: GraphQueryResponse = serde_json::from_str("{}").unwrap();
        assert!(empty.columns.is_empty() && empty.rows.is_empty() && !empty.truncated);
    }

    #[test]
    fn workspace_membership_and_switch_decode() {
        let list: Vec<WorkspaceMembership> = serde_json::from_str(
            r#"[{"id":"w1","name":"Home","slug":"home","role":"owner","active":true},
                {"id":"w2","name":"Work","slug":"work","role":"member","active":false}]"#,
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        assert!(list[0].active);
        assert_eq!(list[1].role, "member");

        // The switch response carries the new token (other session fields ignored).
        let resp: SwitchResponse = serde_json::from_str(
            r#"{"token":"sess-abc","workspace_id":"w2","user_id":"u1","role":"member"}"#,
        )
        .unwrap();
        assert_eq!(resp.token, "sess-abc");

        let body = serde_json::to_value(SwitchWorkspace {
            workspace_id: "w2".into(),
        })
        .unwrap();
        assert_eq!(body["workspace_id"], "w2");
    }

    #[test]
    fn connection_and_calendar_decode() {
        let conn: Connection = serde_json::from_str(
            r#"{"id":"k1","workspace_id":"w1","kind":"calendar","name":"Home"}"#,
        )
        .unwrap();
        assert_eq!(conn.kind, "calendar");
        assert!(conn.cursor.is_none());
        // No `collecting` field on the wire ⇒ assume live (never a false idle alarm).
        assert!(conn.collecting);

        let cal: Calendar = serde_json::from_str(
            r#"{"id":"c1","workspace_id":"w1","connection_id":"k1",
                "external_id":"default","name":"Personal","read_only":true}"#,
        )
        .unwrap();
        assert!(cal.read_only);
        assert_eq!(cal.name, "Personal");
    }

    #[test]
    fn connection_collect_status_decodes() {
        // The §29 annotation rides as a flat `collecting` boolean alongside the
        // connection's own fields.
        let dormant: Connection = serde_json::from_str(
            r#"{"id":"k2","workspace_id":"w1","kind":"email","name":"Inbox","collecting":false}"#,
        )
        .unwrap();
        assert!(
            !dormant.collecting,
            "a dormant source decodes as not collecting"
        );

        let live: Connection = serde_json::from_str(
            r#"{"id":"k3","workspace_id":"w1","kind":"email","name":"Inbox","collecting":true}"#,
        )
        .unwrap();
        assert!(live.collecting);
    }

    #[test]
    fn status_mode_defaults_to_single_user() {
        // An older server that predates the `mode` field must still decode — and
        // fall back to the leaner single-user presentation (member/role chrome
        // hidden), never accidentally surfacing multi-user admin surfaces.
        let s: StatusInfo =
            serde_json::from_str(r#"{"version":"1","healthy":true,"llm":{},"services":[]}"#)
                .unwrap();
        assert_eq!(s.mode, "single_user");
        // An older server also predates `sso`; it must default to "no SSO".
        assert!(!s.sso);

        // When present it rides through verbatim.
        let m: StatusInfo = serde_json::from_str(
            r#"{"version":"1","mode":"multi_user","sso":true,"llm_control_plane":true,"healthy":true,"llm":{},"services":[]}"#,
        )
        .unwrap();
        assert_eq!(m.mode, "multi_user");
        assert!(m.sso);
        assert!(m.llm_control_plane);
    }

    #[test]
    fn workspace_membership_org_id_defaults_to_empty() {
        // An older `/workspaces` listing without `organisation_id` must still
        // decode; the absent org id becomes empty and buckets under the fallback
        // group rather than breaking the switcher.
        let w: WorkspaceMembership = serde_json::from_str(
            r#"{"id":"w1","name":"Home","slug":"home","role":"owner","active":true}"#,
        )
        .unwrap();
        assert_eq!(w.organisation_id, "");
        assert!(w.active);

        let g: WorkspaceMembership = serde_json::from_str(
            r#"{"id":"w2","organisation_id":"org7","role":"member","active":false}"#,
        )
        .unwrap();
        assert_eq!(g.organisation_id, "org7");
    }

    #[test]
    fn my_organisation_decodes_with_defaults() {
        // The switcher grouping source: an org with its visible workspaces. Absent
        // optional fields default (empty workspaces, `can_create_workspace` false).
        let o: MyOrganisation = serde_json::from_str(
            r#"{"id":"org1","name":"Acme","slug":"acme","role":"owner",
                "workspace_creation":"members","can_create_workspace":true,
                "workspaces":[{"id":"w1","name":"Home","slug":"home","role":"owner"}]}"#,
        )
        .unwrap();
        assert_eq!(o.name, "Acme");
        assert!(o.can_create_workspace);
        assert_eq!(o.workspaces.len(), 1);
        assert_eq!(o.workspaces[0].id, "w1");

        // Minimal shape (just an id) still decodes.
        let bare: MyOrganisation = serde_json::from_str(r#"{"id":"org2"}"#).unwrap();
        assert_eq!(bare.id, "org2");
        assert!(!bare.can_create_workspace);
        assert!(bare.workspaces.is_empty());
    }

    #[test]
    fn org_member_view_decodes() {
        let m: OrgMemberView = serde_json::from_str(
            r#"{"user_id":"u1","email":"a@b.c","display_name":"Ann","role":"admin"}"#,
        )
        .unwrap();
        assert_eq!(m.user_id, "u1");
        assert_eq!(m.role, "admin");
    }

    #[test]
    fn workspace_shell_decodes_live_and_archived() {
        // A live shell: the server omits `archived_at`, so it decodes to `None` and
        // reads as not-archived.
        let live: WorkspaceShell =
            serde_json::from_str(r#"{"id":"w1","name":"Home","slug":"home"}"#).unwrap();
        assert_eq!(live.id, "w1");
        assert!(live.archived_at.is_none());
        assert!(!live.is_archived());

        // An archived shell carries the stamp (an RFC 3339 string) — the flag the
        // panel buckets on to offer restore.
        let archived: WorkspaceShell = serde_json::from_str(
            r#"{"id":"w2","name":"Old","slug":"old","archived_at":"2026-07-02T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(archived.is_archived());
        assert_eq!(
            archived.archived_at.as_deref(),
            Some("2026-07-02T10:00:00Z")
        );

        // Minimal shape (just an id) still decodes and reads as live.
        let bare: WorkspaceShell = serde_json::from_str(r#"{"id":"w3"}"#).unwrap();
        assert_eq!(bare.id, "w3");
        assert!(!bare.is_archived());
    }

    #[test]
    fn user_lookup_decodes_with_and_without_display_name() {
        let full: UserLookup =
            serde_json::from_str(r#"{"user_id":"u9","email":"Ada@Ex.test","display_name":"Ada"}"#)
                .unwrap();
        assert_eq!(full.user_id, "u9");
        assert_eq!(full.email, "Ada@Ex.test");
        assert_eq!(full.display_name, "Ada");

        // The server omits a blank display name; it defaults to empty on decode.
        let anon: UserLookup =
            serde_json::from_str(r#"{"user_id":"u10","email":"x@y.test"}"#).unwrap();
        assert_eq!(anon.user_id, "u10");
        assert!(anon.display_name.is_empty());
    }

    #[test]
    fn org_bodies_serialize() {
        assert_eq!(
            serde_json::to_string(&CreateOrg {
                name: "Acme".into(),
                slug: "acme".into()
            })
            .unwrap(),
            r#"{"name":"Acme","slug":"acme"}"#
        );
        assert_eq!(
            serde_json::to_string(&AddOrgMember {
                user_id: "u1".into(),
                role: "member".into()
            })
            .unwrap(),
            r#"{"user_id":"u1","role":"member"}"#
        );
        assert_eq!(
            serde_json::to_string(&SetOrgPolicy {
                workspace_creation: "admins".into()
            })
            .unwrap(),
            r#"{"workspace_creation":"admins"}"#
        );
    }
}
