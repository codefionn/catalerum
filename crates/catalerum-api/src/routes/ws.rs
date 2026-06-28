//! Streaming chat over WebSocket (SOUL §7, §12).
//!
//! `GET /ws/chat` upgrades to a WebSocket. The client authenticates with a
//! bearer token (sent as the `access_token`/`token` query parameter on the
//! upgrade URL, or an `Authorization: Bearer` header where the client can set
//! one), then drives a turn:
//!
//! 1. Client → server: a JSON text frame `{ "conversation_id": "...", "content": "..." }`.
//!    An optional `regenerate_from` (a user message id) instead re-answers that
//!    message: the server drops the transcript tail after it and runs the loop
//!    against the history ending there (SOUL §12).
//! 2. Server persists the user message, replays the conversation history, and
//!    runs the streamed **agent loop** ([`run_agent_streaming`]) against llmleaf:
//!    it streams a turn, dispatches any `tool_calls` server-side through the
//!    workspace-scoped [`ToolRegistry`](catalerum_core::tool::ToolRegistry)
//!    (SOUL §7), and loops until the model answers without tool calls.
//! 3. Every [`StreamEvent`] of every turn is published to the per-turn
//!    [`catalerum_bus`] relay and forwarded to the client as a JSON frame
//!    `{ "type": "token", "event": <StreamEvent> }`. (The relay is the cross-pod
//!    path of SOUL §7; in-process is the M1 default.) This includes
//!    `reasoning_delta` frames carrying the model's visible thinking for a
//!    reasoning-capable model (the opaque signed reasoning blocks are kept
//!    in-process and never sent). A turn's own `Done` is informational; the turn
//!    ends only on the final `message_done` below.
//! 4. Each assistant turn and tool result is persisted as its own `messages` row
//!    **incrementally** — the instant the loop produces it, not batched at the
//!    end — so a long multi-round turn is durable round-by-round (a crash or
//!    dropped socket mid-loop keeps everything saved so far). Once the loop ends
//!    the exchange's summed token/cost usage is back-filled onto the final
//!    assistant row and the server sends a final `{ "type": "message_done", ... }`
//!    frame.
//!
//! Multi-round semantics: when the model takes several tool rounds, each round
//! is persisted as a distinct assistant row (text + that round's tool calls), so
//! a faithful replay shows the intermediate tool-call turns separately. The live
//! UI mirrors this — it opens a fresh streaming bubble each time a round's prose
//! follows the previous round's tool calls — so streaming and replay render the
//! same interleaving. `message_done.content` still carries only the *final*
//! turn's answer. Single-round turns (no tools) — the common case — collapse to
//! exactly one assistant message.
//!
//! The socket stays **live during a turn** (SOUL §12): while the loop streams,
//! inbound frames are still read. A `{ "stop": true }` frame cancels the running
//! turn (partial text is kept, in-flight/pending tool calls resolve to a
//! synthesized "cancelled" error, and the terminal `message_done` carries
//! `stopped: true`). A chat frame for the *same* conversation is queued and
//! **injected at the next round boundary** — right after a round's tool results,
//! or, if the model just finished, as one more round of the same exchange — each
//! injected message is persisted and acknowledged with a
//! `{ "type": "user_message", "message_id": … }` frame. Anything else (another
//! conversation's turn, a regenerate, an approval decision) is deferred and runs
//! as its own turn once the current one ends; a stop discards the not-yet-injected
//! queue (the client re-drafts those messages).
//!
//! Errors are reported as `{ "type": "error", "message": "..." }` frames; the
//! socket then closes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use futures::stream::SplitStream;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use catalerum_bus::{conv_input_stream, Bus, TurnId};
use catalerum_core::ask::Question;
use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::{ApprovalDecision, Attachment, MessageRole, UiDefinition};
use catalerum_core::stream::StreamEvent;
use catalerum_core::tool::ToolContext;
use catalerum_core::{ConversationId, MessageId, PendingApprovalId, UiDefinitionId, WorkspaceId};
use catalerum_iam::Principal;
use catalerum_llm::{run_agent_streaming, AgentConfig, CancellationToken, TurnObserver};
use catalerum_store::NewMessage;

use crate::auth::Auth;
use crate::chat_context::{
    inline_image_attachments, patch_dangling_tool_calls, resolve_skill_invocation,
    to_chat_messages, trim_to_turn_boundary, user_seed_content, CHAT_HISTORY_LIMIT,
};
use crate::error::ApiError;
use crate::guidance;
use crate::state::AppState;
use crate::tools::is_ui_authoring_tool;

const DEFAULT_CHAT_PROFILE_SYSTEM_PROMPT: &str = "You are an highly competent assistant.";

fn chat_profile_system_prompt(prompt: Option<&str>) -> &str {
    match prompt {
        Some(prompt) if !prompt.trim().is_empty() => prompt,
        _ => DEFAULT_CHAT_PROFILE_SYSTEM_PROMPT,
    }
}

/// An inbound approval decision on a guard-deferred tool call (SOUL §19). Disjoint
/// from a chat [`ClientFrame`] (no `conversation_id`/`content`), so the two parse
/// apart. Resolving the durable [`PendingApproval`](catalerum_core::model::PendingApproval)
/// and re-running the held call happens in [`resume_approval`].
#[derive(Clone, Debug, Deserialize)]
struct ApprovalFrame {
    /// The pending-approval id from the server's `approval_request` frame (or a
    /// `GET /conversations/{id}/pending_approval` fetch).
    approval_id: String,
    /// The user's decision: `true` = approve (run it), `false` = reject.
    approved: bool,
    /// Stable retry coordinates supplied by resilient clients.
    #[serde(default)]
    conversation_id: Option<ConversationId>,
    #[serde(default)]
    user_message_id: Option<MessageId>,
}

/// Mount the WebSocket chat route.
pub fn router() -> Router<AppState> {
    Router::new().route("/ws/chat", any(ws_chat))
}

/// Inbound client frame: a single user turn. `Clone` because a frame that
/// arrives *mid-turn* is parked in the shared [`TurnIntake`] queue and either
/// drained into the running loop or re-driven as its own turn afterwards.
/// `Serialize` so a cross-pod mid-turn "say" can ride the durable input queue.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ClientFrame {
    conversation_id: ConversationId,
    content: String,
    /// Stable client-generated id for an ordinary user turn. Retrying the same
    /// frame reuses this row/turn key instead of duplicating model work.
    #[serde(default)]
    user_message_id: Option<MessageId>,
    /// Regenerate request (SOUL §12): when set to an existing **user** message in
    /// this conversation, the loop re-answers *that* message instead of persisting
    /// `content` as a new turn — the anchor stays and the transcript tail it
    /// produced (the old answer + any later exchanges) is dropped first. Absent on
    /// an ordinary turn.
    #[serde(default)]
    regenerate_from: Option<MessageId>,
    /// File/image references for this user turn (SOUL §9/§12): the client uploads
    /// each file to its files store first (`PUT /storage/objects/{key}`), then sends
    /// only the reference here — the bytes are never inlined into `content`. Empty
    /// on a turn with no uploads. Ignored on a regenerate (the anchor's attachments
    /// already stand).
    #[serde(default)]
    attachments: Vec<Attachment>,
    /// A `/<skill>` composer invocation (SOUL §12/§23): the skill name whose
    /// runbook the server snapshots onto this user message — the model gets the
    /// runbook attached, the UI keeps only `content`. Gated per skill
    /// (`skill:use@<name>`) under the caller's own authority. Ignored on a
    /// regenerate (the anchor's snapshot already stands).
    #[serde(default)]
    skill: Option<String>,
    /// Structured `ask_user` form answers (SOUL §7/§12), sent when this turn IS the
    /// user's reply to a pending question form. `content` still carries the same
    /// answers flattened to prose (what the model reads and the transcript shows);
    /// this field is the durable structured record, stamped onto the pending
    /// question row the turn resolves. Empty on an ordinary turn.
    #[serde(default)]
    answers: Vec<catalerum_core::ask::Answer>,
    /// The hands-free conversation overlay asks for a short, speech-friendly
    /// plain-text response. Older clients and typed chat turns omit this field.
    #[serde(default)]
    conversation_mode: bool,
}

/// An inbound stop request (SOUL §12): `{ "stop": true, "conversation_id"?: … }`.
/// Disjoint from both a chat [`ClientFrame`] (which has no `stop` field) and an
/// [`ApprovalFrame`], so the three parse apart. Cancels the currently streaming
/// turn. A **stray** stop (no turn streaming on this socket) that names a
/// conversation is relayed over the conversation's cross-pod control channel
/// (SOUL §12/§16 M7) so the pod actually streaming the turn cancels it; without
/// a conversation it stays the historical no-op.
#[derive(Debug, Deserialize)]
struct StopFrame {
    stop: bool,
    /// The conversation whose streaming turn to stop (optional; older clients
    /// omit it).
    #[serde(default)]
    conversation_id: Option<ConversationId>,
}

/// The live-intake side of one streaming turn (SOUL §12): the shared state the
/// socket's mid-turn reads feed and the running turn consumes.
#[derive(Clone, Default)]
struct TurnIntake {
    /// Chat frames that arrived while the turn streamed. The turn's observer
    /// drains the ones it can inject (same conversation, not a regenerate) at
    /// each round boundary; whatever is left when the turn ends is re-driven as
    /// ordinary follow-up turns — unless the turn was stopped, which discards
    /// the queue (the client re-drafts those messages).
    queue: Arc<Mutex<VecDeque<ClientFrame>>>,
    /// The turn's stop signal, cancelled by a [`StopFrame`].
    cancel: CancellationToken,
}

/// Outbound server frame envelope. `type` is one of `token`, `message_done`,
/// `error`.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerFrame {
    /// Transport liveness frame. It is sent directly to the socket (not retained
    /// in the turn buffer) so a mobile client can detect a half-open connection.
    Heartbeat,
    /// A streamed model event (text/tool-call delta or the stream's own done).
    Token { event: StreamEvent },
    /// The turn is complete and the assistant message was persisted.
    MessageDone {
        message_id: MessageId,
        /// The anchoring user message of this exchange — the message the assistant
        /// answered. The chat client backfills it onto the just-sent user line so
        /// the per-message "regenerate" control can target it (it is otherwise not
        /// surfaced to the client). On a regenerate it is the reused anchor's id.
        user_message_id: MessageId,
        conversation_id: ConversationId,
        content: String,
        /// True when this is a synthetic terminal because the replay buffer was
        /// unavailable. The client must replace its partial view from Postgres.
        #[serde(default)]
        reconcile: bool,
        /// `true` iff an iteration/repeated-tool safety cap stopped the agent before
        /// finishing — the reply is a best-effort partial, so the client flags it.
        #[serde(default)]
        truncated: bool,
        /// `true` iff the user stopped the turn (SOUL §12) — the reply is a
        /// deliberate partial, and any queued-but-not-yet-injected user messages
        /// were discarded (the client returns them to its composer).
        #[serde(default)]
        stopped: bool,
        /// The LLM cost (USD) for this turn, when the backend reported one — drives
        /// the chat UI's per-turn cost readout. Omitted when the cost is unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        /// Prompt (input) tokens for the whole exchange — the agent loop's summed
        /// usage across every tool-call turn of this user message. Drives the chat
        /// UI's per-turn token tooltip. Omitted when usage was not reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt_tokens: Option<u32>,
        /// Completion (output) tokens for the whole exchange. Omitted when unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        completion_tokens: Option<u32>,
        /// Total tokens (prompt + completion) for the whole exchange. The chat UI
        /// running-sums this across turns for its "up to here" readout. Omitted when
        /// unknown.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_tokens: Option<u32>,
        /// Prompt tokens served from the provider's cache this exchange (a cache
        /// read/hit). `Some(0)` when usage was reported with no cache reads; omitted
        /// when no usage was reported at all.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cached_tokens: Option<u32>,
        /// Prompt tokens written to the provider's cache this exchange (a cache
        /// write/creation). Same presence semantics as `cached_tokens`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_creation_tokens: Option<u32>,
    },
    /// An emerged UI the assistant created/updated this turn, to mount inline in
    /// the chat (the "emerged UI" feature). Carries the full definition so the
    /// client renders without a follow-up fetch. Additive frame — older clients
    /// decode it to their `#[serde(other)] Unknown` and ignore it.
    UiArtifact {
        ui_id: UiDefinitionId,
        version: i64,
        definition: Box<UiDefinition>,
    },
    /// A guarded tool call (SOUL §19) needs the user's OK before it runs. The turn
    /// is paused; the client shows an approve/reject prompt and replies with an
    /// `{ approval_id, approved }` frame carrying this `id`. Additive — older
    /// clients decode it to their `Unknown` fallback and ignore it (leaving the
    /// call to time out and fail closed).
    ApprovalRequest {
        /// Correlation id the client must echo in its reply.
        id: String,
        /// The tool awaiting approval.
        tool: String,
        /// Its JSON arguments (for the prompt).
        arguments: Value,
        /// Why the guard escalated to the user.
        reason: String,
    },
    /// An `ask_user` question form (SOUL §7/§12) needs the user's answers before the
    /// turn continues. The turn is paused; the client renders the form (choice
    /// buttons / free-text fields, matched to each question) and replies with a
    /// `{ question_id, answers }` frame carrying this `id`. Additive — older clients
    /// decode it to their `Unknown` fallback and ignore it (the call then times out
    /// and reports the questions went unanswered).
    QuestionRequest {
        /// Correlation id the client must echo in its reply.
        id: String,
        /// The questions to put to the user.
        questions: Vec<Question>,
    },
    /// A user message was persisted into the conversation — sent for the turn's
    /// anchoring message and for every mid-turn queued message the instant it is
    /// injected (SOUL §12). The client uses it to stamp the matching optimistic
    /// line with its server id (enabling its regenerate control) and to clear
    /// the line's "queued" styling. Additive — older clients ignore it.
    UserMessage {
        message_id: MessageId,
        conversation_id: ConversationId,
    },
    /// A turn-level error; the socket closes after this frame.
    Error { message: String },
}

/// An inbound request to (re)attach to an in-flight turn's live stream (SOUL
/// §7/§12): `{ "attach": "<conversation_id>", "user_message_id": "<id>",
/// "resume_after": "<stream-id>"? }`. Disjoint from the other inbound frames (it
/// alone carries `attach`). The server forwards the turn's replayable Valkey
/// buffer from `resume_after` (or the start), so a reconnecting client — on any
/// pod — resumes the exact stream with no gap. `user_message_id` is the turn key
/// (the anchoring user message); a fresh reopen learns it from
/// `GET /conversations/{id}/active_turn` first.
#[derive(Debug, Deserialize)]
struct AttachFrame {
    #[serde(rename = "attach")]
    conversation_id: ConversationId,
    user_message_id: MessageId,
    /// The last stream-entry id the client already rendered; resume strictly
    /// after it. Absent → replay the whole retained buffer from the start.
    #[serde(default)]
    resume_after: Option<String>,
}

/// How long a buffer read blocks for new frames before looping (also bounds how
/// often a forwarder re-checks whether its turn is still live).
const TURN_READ_BLOCK_MS: u64 = 20_000;

/// The one short catch-up read a forwarder makes after finding its turn dead on
/// an empty read, so a terminal frame appended in that race window is still
/// delivered before a synthetic one is sent.
const TURN_FINAL_READ_BLOCK_MS: u64 = 2_000;

/// Registry key advertising a conversation's in-flight turn (SOUL §7/§12), so a
/// reconnecting client on any pod discovers which turn to attach to. Value is a
/// JSON `{ "user_message_id": … }`; TTL'd and refreshed on the turn-lock clock.
pub(crate) fn active_turn_key(conversation_id: ConversationId) -> String {
    format!("cat:activeturn:{conversation_id}")
}

/// Value stored under [`active_turn_key`] — the turn's anchoring user message id.
#[derive(Serialize, Deserialize)]
struct ActiveTurnValue {
    user_message_id: MessageId,
}

/// The write side of a detached turn: every [`ServerFrame`] the run produces is
/// appended to the turn's replayable Valkey buffer (SOUL §7/§12), decoupled from
/// any client socket. A forwarder [`read`](TurnBuffer::read)s the buffer and
/// pipes frames to whichever socket currently holds the user; a run with no
/// live reader keeps going regardless (its appends just sit in the buffer).
#[derive(Clone)]
struct TurnSink {
    bus: Bus,
    turn: TurnId,
    /// Set once a terminal frame (`message_done`/`error`) is appended, so the
    /// spawn wrapper can guarantee exactly one terminal even if the run bailed
    /// before emitting its own.
    terminated: Arc<std::sync::atomic::AtomicBool>,
}

impl TurnSink {
    fn new(bus: Bus, turn: TurnId) -> Self {
        Self {
            bus,
            turn,
            terminated: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Append one frame to the turn buffer (best-effort; a buffer write failure
    /// must never abort the run — client delivery is not a source of truth).
    async fn emit(&self, frame: ServerFrame) {
        if matches!(
            frame,
            ServerFrame::MessageDone { .. } | ServerFrame::Error { .. }
        ) {
            self.terminated
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        match serde_json::to_vec(&frame) {
            Ok(bytes) => {
                if let Err(e) = self.bus.turnbuf().append(&self.turn, &bytes).await {
                    tracing::warn!(error = %e, turn = %self.turn, "turn buffer append failed");
                }
            }
            Err(e) => tracing::warn!(error = %e, "serialize server frame"),
        }
    }

    /// Guarantee the buffer carries a terminal frame so every forwarder unwinds,
    /// even when the run ended without emitting one (an early no-op, e.g. an
    /// already-resolved approval). The synthetic terminal carries the turn's own
    /// ids and empty content — the client just finalizes and refetches history.
    async fn finish_if_needed(&self) {
        if self.terminated.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.emit(ServerFrame::MessageDone {
            message_id: self.turn.message_id,
            user_message_id: self.turn.message_id,
            conversation_id: self.turn.conversation_id,
            content: String::new(),
            reconcile: false,
            truncated: false,
            stopped: false,
            cost_usd: None,
            prompt_tokens: None,
            completion_tokens: None,
            total_tokens: None,
            cached_tokens: None,
            cache_creation_tokens: None,
        })
        .await;
    }
}

/// Re-serialize a buffered frame payload with its stream-entry id stamped as a
/// top-level `seq`, so the client can record its resume cursor without any
/// `ServerFrame` variant needing a field. Returns `(json_text, is_terminal)`;
/// a terminal frame (`message_done`/`error`) ends the forward loop. An
/// unparseable payload is forwarded verbatim (never terminal).
fn stamp_seq(payload: &[u8], id: &str) -> (String, bool) {
    match serde_json::from_slice::<Value>(payload) {
        Ok(mut v) => {
            let terminal = matches!(
                v.get("type").and_then(Value::as_str),
                Some("message_done") | Some("error")
            );
            if let Some(obj) = v.as_object_mut() {
                obj.insert("seq".to_string(), Value::String(id.to_string()));
            }
            (v.to_string(), terminal)
        }
        Err(_) => (String::from_utf8_lossy(payload).into_owned(), false),
    }
}

/// Send a raw JSON text frame to the socket. `Err(())` means the client is gone.
async fn send_text(
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    text: String,
) -> Result<(), ()> {
    sink.send(WsMessage::Text(text.into()))
        .await
        .map_err(|_| ())
}

/// The WS upgrade handler. Authentication runs *before* the upgrade via the
/// [`Auth`] extractor (which reads the `access_token`/`token` query param on the
/// handshake URL, or an `Authorization` header), then defers to
/// [`handle_socket`]. An unauthenticated handshake is rejected with `401`.
async fn ws_chat(
    State(state): State<AppState>,
    auth: Auth,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let principal = auth.principal();
    Ok(ws.on_upgrade(move |socket| handle_socket(socket, state, principal)))
}

/// Read the next inbound text payload, skipping ping/pong. `None` on close/EOF.
async fn next_text(stream: &mut SplitStream<WebSocket>) -> Option<String> {
    while let Some(Ok(msg)) = stream.next().await {
        return match msg {
            WsMessage::Text(t) => Some(t.to_string()),
            WsMessage::Binary(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => None,
        };
    }
    None
}

/// Work awaiting its own turn: a raw inbound frame not yet classified, or a chat
/// frame that arrived mid-turn and was never injected (another conversation's
/// turn, a regenerate, or a follow-up that just missed the boundary) — it runs
/// as an ordinary turn once the current one ends.
enum PendingWork {
    Raw(String),
    Chat(ClientFrame),
}

/// Drive the authenticated socket: read client frames, stream the resulting turns.
///
/// Two inbound frames open turns: a chat [`ClientFrame`] (a user turn) and an
/// [`ApprovalFrame`] (a decision on a guard-deferred tool call — no blocking
/// round-trip; it resolves the durable
/// [`PendingApproval`](catalerum_core::model::PendingApproval) and re-runs the
/// held call as a fresh turn, SOUL §19). Each turn's outbound frames funnel
/// through an `mpsc` channel so the streaming observer and the post-turn pushes
/// (emerged UI, approval/question prompts) share one writer. While a turn
/// streams, the socket is still read (see [`TurnIntake`]); whatever the turn
/// didn't consume lands in `pending` and is driven before the socket is read
/// again.
async fn handle_socket(socket: WebSocket, state: AppState, principal: Principal) {
    let (mut sink, mut stream) = socket.split();
    let mut pending: VecDeque<PendingWork> = VecDeque::new();

    loop {
        let work = match pending.pop_front() {
            Some(w) => w,
            None => match next_text(&mut stream).await {
                Some(text) => PendingWork::Raw(text),
                None => break, // socket closed
            },
        };

        let frame = match work {
            PendingWork::Chat(frame) => frame,
            PendingWork::Raw(text) => {
                // An approval decision on a guard-deferred call. (An `ApprovalFrame`
                // and a chat `ClientFrame` have disjoint required fields, so a chat
                // frame falls through to the `ClientFrame` parse below.) Resolve the
                // durable decision here, then re-drive the held call as an ordinary
                // turn; an already-resolved / stray click is a no-op.
                if let Ok(a) = serde_json::from_str::<ApprovalFrame>(&text) {
                    let retry_turn = a
                        .conversation_id
                        .zip(a.user_message_id)
                        .map(|(conversation, message)| TurnId::new(conversation, message));
                    if let Some(chat) = resolve_approval_to_frame(&state, &principal, a).await {
                        if !start_and_forward(
                            &mut sink,
                            &mut stream,
                            &state,
                            &principal,
                            chat,
                            &mut pending,
                        )
                        .await
                        {
                            return; // client gone
                        }
                    } else if let Some(turn) = retry_turn {
                        let visible = state
                            .store()
                            .conversations()
                            .get(principal.workspace_id, turn.conversation_id)
                            .await
                            .is_ok();
                        if visible
                            && !forward_turn(
                                &mut sink,
                                &mut stream,
                                &state,
                                &principal,
                                turn,
                                None,
                                "0",
                                &mut pending,
                            )
                            .await
                        {
                            return;
                        }
                    }
                    continue;
                }
                // A (re)attach to an in-flight turn's live stream: forward its
                // replayable Valkey buffer from the client's cursor. Works on any
                // pod — the buffer lives in Valkey (SOUL §7/§12). Workspace-checked.
                if let Ok(a) = serde_json::from_str::<AttachFrame>(&text) {
                    let visible = state
                        .store()
                        .conversations()
                        .get(principal.workspace_id, a.conversation_id)
                        .await
                        .is_ok();
                    if visible {
                        let turn = TurnId::new(a.conversation_id, a.user_message_id);
                        // A malformed client cursor would error EVERY read (XREAD
                        // rejects it) and stall the forward; fall back to a full
                        // replay instead.
                        let start = a
                            .resume_after
                            .as_deref()
                            .filter(|c| is_valid_stream_cursor(c))
                            .unwrap_or("0")
                            .to_string();
                        if !forward_turn(
                            &mut sink,
                            &mut stream,
                            &state,
                            &principal,
                            turn,
                            None,
                            &start,
                            &mut pending,
                        )
                        .await
                        {
                            return; // client gone
                        }
                    }
                    continue;
                }
                // A stop with no turn streaming on THIS socket: cancel any local
                // run and relay it over the conversation's control channel so the
                // pod actually streaming the turn (possibly a peer) cancels it
                // (SOUL §12/§16 M7). Workspace-checked. A conversation-less stop
                // stays the historical no-op (its turn ended before the frame).
                if let Ok(s) = serde_json::from_str::<StopFrame>(&text) {
                    if s.stop {
                        if let Some(conv) = s.conversation_id {
                            relay_stop(&state, &principal, conv).await;
                        }
                    }
                    continue;
                }
                match serde_json::from_str::<ClientFrame>(&text) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = send_frame(
                            &mut sink,
                            &ServerFrame::Error {
                                message: format!("invalid frame: {e}"),
                            },
                        )
                        .await;
                        continue;
                    }
                }
            }
        };
        if !start_and_forward(
            &mut sink,
            &mut stream,
            &state,
            &principal,
            frame,
            &mut pending,
        )
        .await
        {
            return; // client gone
        }
    }
}

/// Spawn a chat turn as a **detached** task and forward its live buffer to this
/// socket. The run outlives the connection: if the socket dies mid-turn the
/// forwarder returns `false` (the caller abandons the socket) while the spawned
/// run keeps executing to completion, streaming into Valkey and persisting each
/// completed message to Postgres. Returns `false` iff the socket died.
async fn start_and_forward(
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    principal: &Principal,
    frame: ClientFrame,
    pending: &mut VecDeque<PendingWork>,
) -> bool {
    // The turn key: a regenerate reuses its anchor user message; a fresh turn
    // pre-mints the id so the Valkey stream key (`cat:turnbuf:{conv}:{msg}`) and
    // the active-turn advertisement are known before the row persists (SOUL §7/§12).
    let conversation_id = frame.conversation_id;
    let uid = frame
        .regenerate_from
        .or(frame.user_message_id)
        .unwrap_or_default();
    let turn_id = TurnId::new(conversation_id, uid);
    // A retry with the same client id may arrive while the original detached run
    // is still live. Do not register/spawn a second run; simply become another
    // forwarder for the same replayable stream.
    if frame.regenerate_from.is_none()
        && frame.user_message_id.is_some()
        && state
            .store()
            .conversations()
            .get(principal.workspace_id, conversation_id)
            .await
            .is_ok()
        && turn_is_live(state, turn_id).await
    {
        return forward_turn(sink, stream, state, principal, turn_id, None, "0", pending).await;
    }
    // A regenerate REUSES its anchor's turn key, so the prior run's frames may
    // still be retained under it (the buffer TTL outlives a quick regenerate).
    // Clear them before the forwarder below replays from "0", or it would stream
    // the old answer and terminate on its stale `message_done` while the new run
    // streams unseen. Done before the run spawns so no forwarder can race it.
    if frame.regenerate_from.is_some() {
        if let Err(e) = state.bus().turnbuf().reset(&turn_id).await {
            tracing::warn!(error = %e, turn = %turn_id, "turn buffer reset failed");
        }
    }
    // The intake is shared with the spawned run: this socket parks mid-turn
    // "say" frames and stop into it, and the run drains them at round boundaries.
    let intake = TurnIntake::default();
    // Register BEFORE spawning so a stop landing during the run's lock-wait still
    // reaches its cancel token; the guard rides the spawned task and deregisters
    // on every exit path.
    let guard = state
        .active_turns()
        .register_guarded(turn_id, intake.cancel.clone());
    let run_state = state.clone();
    let run_principal = *principal;
    let run_intake = intake.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let sink = TurnSink::new(run_state.bus().clone(), turn_id);
        if let Err(e) = run_turn(
            &sink,
            &run_state,
            &run_principal,
            frame,
            turn_id,
            &run_intake,
        )
        .await
        {
            sink.emit(ServerFrame::Error {
                message: e.to_string(),
            })
            .await;
        }
        // Guarantee a terminal frame so every forwarder unwinds.
        sink.finish_if_needed().await;
    });
    forward_turn(
        sink,
        stream,
        state,
        principal,
        turn_id,
        Some(&intake),
        "0",
        pending,
    )
    .await
}

/// A syntactically valid stream cursor — `{ms}` or `{ms}-{seq}`, digits only —
/// as XREAD accepts (`"0"` included). Anything else would make every read
/// error, so the attach path falls back to a full replay instead.
fn is_valid_stream_cursor(s: &str) -> bool {
    let mut parts = s.split('-');
    let (ms, seq) = (parts.next(), parts.next());
    parts.next().is_none()
        && seq.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        && ms.is_some_and(|p| {
            !p.is_empty() && p.len() <= 20 && p.bytes().all(|b| b.is_ascii_digit())
        })
}

/// Forward one turn's replayable Valkey buffer to this socket, streaming from
/// `start_after` (`"0"` = from the beginning). Returns `false` when the socket
/// died (the caller abandons it; the detached run is untouched and keeps going),
/// `true` on a clean turn end.
///
/// While forwarding, the inbound half stays live (SOUL §12): a stop cancels the
/// run, a same-conversation "say" is injected, everything else defers. For the
/// **originating** socket `intake` is `Some` — it shares the run's in-memory
/// intake, so a say is parked directly (load-bearing, no Valkey). For an
/// **attach** (a reconnect, possibly on another pod) `intake` is `None`: a say
/// rides the durable [`conv_input_stream`] work queue instead, and a stop rides
/// the control channel — so mid-turn input survives a cross-pod hop.
///
/// The loop ends when the buffer yields a terminal `message_done`/`error` frame,
/// or when a read comes back empty and the turn is no longer live (neither
/// registered on this pod nor advertised in the registry) — then one final
/// short catch-up read runs (the terminal may have landed just after the empty
/// read) before a terminal is synthesized so the client finalizes + refetches
/// history. This exit applies to the originating socket too: if the run's own
/// terminal append was lost (a Valkey blip), the forwarder must not block
/// forever. A read **error** (Valkey down, a bad cursor) is backed off and then
/// treated like an empty read, never spun on.
#[allow(clippy::too_many_arguments)] // an internal forwarding seam; bundling would obscure it
async fn forward_turn(
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    principal: &Principal,
    turn: TurnId,
    intake: Option<&TurnIntake>,
    start_after: &str,
    pending: &mut VecDeque<PendingWork>,
) -> bool {
    let mut cursor = start_after.to_string();
    let mut deferred: Vec<String> = Vec::new();
    let mut inbound_open = true;
    let mut socket_alive = true;
    // Set when the turn was found dead on an empty read: grants one final short
    // read to catch a terminal appended between that read and the liveness check.
    let mut last_chance = false;
    let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(10));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;

    'forward: loop {
        let block_ms = if last_chance {
            TURN_FINAL_READ_BLOCK_MS
        } else {
            TURN_READ_BLOCK_MS
        };
        tokio::select! {
            biased;
            // Next batch of buffered frames (blocks up to the read window).
            entries = state.bus().turnbuf().read(&turn, &cursor, block_ms) => {
                let entries = match entries {
                    Ok(entries) => entries,
                    Err(e) => {
                        // Back off instead of spinning hot (an errored read
                        // returns instantly — a down Valkey or a garbage attach
                        // cursor would otherwise busy-loop), then fall through
                        // to the empty-read liveness handling below.
                        tracing::warn!(error = %e, turn = %turn, "turn buffer read failed");
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        Vec::new()
                    }
                };
                if entries.is_empty() {
                    if last_chance {
                        // The turn is over and the catch-up read stayed empty (it
                        // ended before we attached / its buffer lapsed / its
                        // terminal append was lost): synthesize a terminal so the
                        // client finalizes + refetches history.
                        let (text, _) = stamp_seq(&empty_done_bytes(turn), "");
                        let _ = send_text(sink, text).await;
                        break 'forward;
                    }
                    if !turn_is_live(state, turn).await {
                        last_chance = true;
                    }
                    continue;
                }
                last_chance = false;
                for entry in entries {
                    cursor = entry.id.clone();
                    let (text, terminal) = stamp_seq(&entry.payload, &entry.id);
                    if send_text(sink, text).await.is_err() {
                        socket_alive = false;
                        break 'forward; // client gone; the run keeps executing
                    }
                    if terminal {
                        break 'forward;
                    }
                }
            }
            // Live inbound while the turn streams (once forwarding ends, remaining
            // frames stay buffered for the caller's loop).
            maybe = next_text(stream), if inbound_open => match maybe {
                Some(text) => route_inbound(state, principal, turn, intake, text, &mut deferred).await,
                None => inbound_open = false,
            },
            _ = heartbeat.tick() => {
                if send_frame(sink, &ServerFrame::Heartbeat).await.is_err() {
                    socket_alive = false;
                    break 'forward;
                }
            },
        }
    }

    // Post-turn intake handoff (originating socket only — an attach shares no
    // in-memory queue): chat frames the run never injected re-drive as their own
    // turns, then deferred raw frames (approvals and the like).
    if let Some(intake) = intake {
        for f in intake.queue.lock().unwrap().drain(..) {
            pending.push_back(PendingWork::Chat(f));
        }
    }
    pending.extend(deferred.into_iter().map(PendingWork::Raw));
    socket_alive
}

/// Route one inbound frame read while forwarding a turn (SOUL §12). A stop
/// cancels the run (locally + cross-pod) — unless it names a *different*
/// conversation, in which case it is relayed to that conversation's turn
/// instead of killing this one; a same-conversation "say" is injected
/// (in-memory for the originating socket, else over the durable input queue);
/// anything else defers to run as its own turn.
async fn route_inbound(
    state: &AppState,
    principal: &Principal,
    turn: TurnId,
    intake: Option<&TurnIntake>,
    text: String,
    deferred: &mut Vec<String>,
) {
    // An approval decision runs as its own turn afterwards.
    if serde_json::from_str::<ApprovalFrame>(&text).is_ok() {
        deferred.push(text);
        return;
    }
    if let Ok(s) = serde_json::from_str::<StopFrame>(&text) {
        if !s.stop {
            return;
        }
        // A stop naming another conversation targets THAT turn (this socket just
        // happens to be forwarding a different one) — relay it like the stray-stop
        // path, leaving the turn forwarded here untouched.
        if let Some(conv) = s.conversation_id {
            if conv != turn.conversation_id {
                relay_stop(state, principal, conv).await;
                return;
            }
        }
        // This turn (named, or a conversation-less legacy stop): a local run
        // (originating or same-pod attach) cancels instantly; a cross-pod run
        // cancels via its control-channel listener. Both are idempotent, so do both.
        state
            .active_turns()
            .cancel_conversation(turn.conversation_id);
        if let Some(intake) = intake {
            intake.cancel.cancel();
            intake.queue.lock().unwrap().clear();
        }
        let channel = catalerum_bus::conv_ctl_channel(&turn.conversation_id.to_string());
        let _ = state
            .bus()
            .push()
            .publish_raw(&channel, b"{\"stop\":true}".to_vec())
            .await;
        return;
    }
    if let Ok(frame) = serde_json::from_str::<ClientFrame>(&text) {
        // A same-conversation, non-regenerate frame is a mid-turn "say".
        let injectable =
            frame.conversation_id == turn.conversation_id && frame.regenerate_from.is_none();
        match intake {
            // Originating socket: park directly on the shared intake unless the
            // turn was stopped (then it re-drives as its own turn).
            Some(intake) if injectable && !intake.cancel.is_cancelled() => {
                intake.queue.lock().unwrap().push_back(frame);
            }
            Some(_) => deferred.push(text),
            // Attach socket: deliver durably over the conversation input queue so
            // the (possibly cross-pod) run injects it at the next round boundary.
            None if injectable => {
                let stream_name = conv_input_stream(&turn.conversation_id.to_string());
                if let Ok(bytes) = serde_json::to_vec(&frame) {
                    let _ = state.bus().queue().push(&stream_name, &bytes).await;
                }
            }
            None => deferred.push(text),
        }
        return;
    }
    deferred.push(text);
}

/// Cancel whatever turn is streaming for `conversation` — locally and, for a
/// run on a peer pod, over its control channel. Workspace-checked; both cancel
/// paths are idempotent. Shared by the stray-stop path (no turn on this socket)
/// and a mid-forward stop that names a different conversation.
async fn relay_stop(state: &AppState, principal: &Principal, conversation: ConversationId) {
    let visible = state
        .store()
        .conversations()
        .get(principal.workspace_id, conversation)
        .await
        .is_ok();
    if !visible {
        return;
    }
    state.active_turns().cancel_conversation(conversation);
    let channel = catalerum_bus::conv_ctl_channel(&conversation.to_string());
    let _ = state
        .bus()
        .push()
        .publish_raw(&channel, b"{\"stop\":true}".to_vec())
        .await;
}

/// Whether `turn` — this specific turn, not just *some* turn — is still live:
/// registered on this pod, or the one the cross-pod active-turn registry
/// advertises for its conversation. Used by a forwarder to decide whether a
/// drained, silent buffer means "still working" or "over". Comparing the
/// advertised anchor matters: an attach to an already-ended turn must not block
/// behind *newer* turns keeping the conversation's registry key fresh.
async fn turn_is_live(state: &AppState, turn: TurnId) -> bool {
    if state.active_turns().active_turn_for(turn.conversation_id) == Some(turn) {
        return true;
    }
    match state
        .bus()
        .registry()
        .lookup(&active_turn_key(turn.conversation_id))
        .await
    {
        Ok(Some(bytes)) => serde_json::from_slice::<ActiveTurnValue>(&bytes)
            .is_ok_and(|v| v.user_message_id == turn.message_id),
        // Absent, or unreadable (a down Valkey): treat as over — the forwarder
        // synthesizes a terminal and the client refetches history (§6.6).
        _ => false,
    }
}

/// A minimal terminal `message_done` for a turn whose buffer has lapsed, so an
/// attach forwarder can unwind the client (which then refetches history).
fn empty_done_bytes(turn: TurnId) -> Vec<u8> {
    serde_json::to_vec(&ServerFrame::MessageDone {
        message_id: turn.message_id,
        user_message_id: turn.message_id,
        conversation_id: turn.conversation_id,
        content: String::new(),
        reconcile: true,
        truncated: false,
        stopped: false,
        cost_usd: None,
        prompt_tokens: None,
        completion_tokens: None,
        total_tokens: None,
        cached_tokens: None,
        cache_creation_tokens: None,
    })
    .unwrap_or_default()
}

/// Resolve a guard-deferred approval and re-run (or drop) the held tool call as a
/// fresh turn (SOUL §19). The decision is written durably **first**, so this is
/// restart-safe: a dropped/restarted server just re-drives from the client's retry,
/// and the guard consults the durable decision when the agent re-attempts the call
/// (Approve → allow, Reject → deny). A stray/late click (already resolved) is a
/// no-op.
async fn resolve_approval_to_frame(
    state: &AppState,
    principal: &Principal,
    frame: ApprovalFrame,
) -> Option<ClientFrame> {
    let ws_id = principal.workspace_id;
    let id: PendingApprovalId = frame.approval_id.parse().ok()?;
    let decision = if frame.approved {
        ApprovalDecision::Approved
    } else {
        ApprovalDecision::Rejected
    };
    // Record the decision durably. `None` → already resolved / superseded / gone
    // (a stray/late click is a no-op — no turn to re-drive).
    let pending = state
        .store()
        .pending_approvals()
        .resolve(ws_id, id, decision)
        .await
        .ok()??;
    if frame
        .conversation_id
        .is_some_and(|id| id != pending.conversation_id)
    {
        return None;
    }
    // Re-run the held call as an ordinary turn: a synthetic user message records the
    // decision in the thread and prompts the agent to proceed; the guard consults
    // the durable decision when the call is re-attempted.
    let content = if frame.approved {
        format!(
            "✅ Approved — go ahead and run the `{}` tool call now.",
            pending.tool
        )
    } else {
        format!(
            "🚫 Rejected — do not run the `{}` tool call; continue without it.",
            pending.tool
        )
    };
    Some(ClientFrame {
        conversation_id: pending.conversation_id,
        content,
        user_message_id: frame.user_message_id,
        regenerate_from: None,
        attachments: Vec::new(),
        skill: None,
        answers: Vec::new(),
        conversation_mode: false,
    })
}

/// TTL on the per-conversation turn lock (SOUL §12/§16 M7); the streaming pod's
/// control listener refreshes it, so it only lapses when the holder dies.
const CONV_TURN_LOCK_TTL: std::time::Duration = std::time::Duration::from_secs(120);
/// How often the holder refreshes the turn lock (well inside the TTL).
const CONV_TURN_LOCK_REFRESH: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a turn waits for a peer's streaming turn on the same conversation
/// to end before giving up with a clear error.
const CONV_TURN_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Cross-pod turn coordination for one conversation (SOUL §12/§16 M7): a
/// TTL-guarded per-conversation lock **serializes turns across pods** (two
/// sockets — same pod or peers — can no longer stream concurrent answers into
/// one thread; the later turn waits its turn), and a control-channel
/// subscription lets **any** pod stop this turn — the stray-stop path publishes
/// on `cat:conv:ctl:{id}` when the stopping client's socket isn't the one
/// streaming. Dropping the coordination aborts the listener and releases the
/// lock (spawned — `Drop` can't await); the TTL is the crash backstop.
struct TurnCoordination {
    bus: Bus,
    /// The turn this coordination guards — also the active-turn Registry key it
    /// advertises (withdrawn on drop) so a reconnecting client discovers it.
    turn: TurnId,
    guard: Option<catalerum_bus::LockGuard>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

impl TurnCoordination {
    /// Serialize on the conversation, advertise the turn as live, and subscribe
    /// its control channel. Waits for a peer's streaming turn to end — checking
    /// `cancel`, so a stop landing while queued abandons the wait — and degrades
    /// to *uncoordinated* on a bus error (chat must keep working through a Valkey
    /// outage; the bus is a coordination hint, never a correctness oracle, §6.6).
    async fn acquire(bus: Bus, turn: TurnId, cancel: CancellationToken) -> Result<Self, ApiError> {
        let conversation_id = turn.conversation_id;
        let resource = format!("conv-turn:{conversation_id}");
        let deadline = tokio::time::Instant::now() + CONV_TURN_WAIT_MAX;
        let guard = loop {
            match bus.lock().try_acquire(&resource, CONV_TURN_LOCK_TTL).await {
                Ok(Some(guard)) => break Some(guard),
                Ok(None) => {
                    if cancel.is_cancelled() {
                        return Err(ApiError::bad_request(
                            "stopped before the turn started (another turn was streaming)",
                        ));
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(ApiError::bad_request(
                            "another turn on this conversation is still streaming; try again",
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e,
                        "conversation turn-lock unavailable; running uncoordinated");
                    break None;
                }
            }
        };
        // Advertise the in-flight turn for cross-pod attach discovery (SOUL
        // §7/§12); refreshed on the lock clock below, withdrawn on drop, TTL'd as
        // the crash backstop. Best-effort — a miss just costs a Postgres refetch.
        announce_active_turn(&bus, turn).await;
        let listener = tokio::spawn(control_listener(bus.clone(), turn, cancel, guard.clone()));
        Ok(Self {
            bus,
            turn,
            guard,
            listener: Some(listener),
        })
    }
}

impl Drop for TurnCoordination {
    fn drop(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
        let bus = self.bus.clone();
        let key = active_turn_key(self.turn.conversation_id);
        let guard = self.guard.take();
        // Release + withdraw are async and `Drop` isn't — spawn them; the TTLs
        // backstop anything that never lands.
        tokio::spawn(async move {
            let _ = bus.registry().withdraw(&key).await;
            if let Some(guard) = guard {
                let _ = bus.lock().release(&guard).await;
            }
        });
    }
}

/// Advertise a conversation's in-flight turn in the cross-pod active-turn
/// registry (TTL'd on the lock clock). Best-effort.
async fn announce_active_turn(bus: &Bus, turn: TurnId) {
    if let Ok(value) = serde_json::to_vec(&ActiveTurnValue {
        user_message_id: turn.message_id,
    }) {
        let _ = bus
            .registry()
            .announce(
                &active_turn_key(turn.conversation_id),
                value,
                CONV_TURN_LOCK_TTL,
            )
            .await;
    }
}

/// Listen on the conversation's control channel for the turn's lifetime — a
/// published `{"stop":true}` cancels this turn — and refresh the turn lock +
/// re-advertise the active-turn key on a clock so a long turn never lapses them.
async fn control_listener(
    bus: Bus,
    turn: TurnId,
    cancel: CancellationToken,
    guard: Option<catalerum_bus::LockGuard>,
) {
    let conversation_id = turn.conversation_id;
    let channel = catalerum_bus::conv_ctl_channel(&conversation_id.to_string());
    let mut sub = bus.push().subscribe_raw(&channel).await.ok();
    let mut tick = tokio::time::interval(CONV_TURN_LOCK_REFRESH);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if let Some(guard) = &guard {
                    // A failed refresh means the lock lapsed under us; keep
                    // listening for stops, but the refresh result is advisory.
                    let _ = bus.lock().refresh(guard, CONV_TURN_LOCK_TTL).await;
                }
                // Keep the active-turn advertisement alive for the turn's duration.
                announce_active_turn(&bus, turn).await;
            }
            msg = async {
                match sub.as_mut() {
                    Some(s) => s.next().await,
                    None => std::future::pending().await,
                }
            } => match msg {
                Some(Ok(bytes)) => {
                    let stop = serde_json::from_slice::<StopFrame>(&bytes)
                        .map(|s| s.stop)
                        .unwrap_or(false);
                    if stop {
                        cancel.cancel();
                    }
                }
                // Subscription ended/errored — keep refreshing the lock.
                _ => sub = None,
            },
        }
    }
}

/// Persist the user turn, run the streamed agent loop (relaying every delta and
/// dispatching tools server-side), persist the full transcript, then emit
/// `message_done`.
async fn run_turn(
    sink: &TurnSink,
    state: &AppState,
    principal: &Principal,
    frame: ClientFrame,
    turn_id: TurnId,
    intake: &TurnIntake,
) -> Result<(), ApiError> {
    let ws_id = principal.workspace_id;
    let conversation_id = frame.conversation_id;

    // Authorize: the conversation must belong to the caller's workspace. Keep it —
    // its `agent_profile_id` (the chat picker, SOUL §19) may rebind this turn.
    // `mut`: a regenerate below prunes the transcript tail, which can invalidate
    // the thread's compaction summary (the FK nulls `summary_upto`) — the row is
    // re-read there so the seed never trusts a stale coverage anchor.
    let mut conversation = state
        .store()
        .conversations()
        .get(ws_id, conversation_id)
        .await
        .map_err(|_| ApiError::NotFound)?;

    // Cross-pod turn coordination (SOUL §12/§16 M7): serialize turns on this
    // conversation across pods and subscribe its control channel, so a stop from
    // a client whose socket landed on another pod still cancels this turn.
    // Acquired BEFORE the user message persists, so a turn queued behind a
    // peer's streaming turn lands in the transcript after that turn's rows.
    // Held to the end of the turn; `Drop` releases on every exit path.
    let _coordination =
        TurnCoordination::acquire(state.bus().clone(), turn_id, intake.cancel.clone()).await?;

    // Idempotent ordinary-turn retry. This check deliberately precedes every
    // mutable side effect and skill/profile re-resolution: once the original row
    // committed, a retry must only acknowledge/replay it, even if configuration
    // changed while the mobile client was disconnected.
    if frame.regenerate_from.is_none() {
        if let Some(id) = frame.user_message_id {
            if let Ok(existing) = state.store().messages().get(id).await {
                if existing.conversation_id != conversation_id
                    || existing.role != MessageRole::User
                    || existing.content != frame.content
                {
                    return Err(ApiError::bad_request(
                        "user_message_id already belongs to a different message",
                    ));
                }
                sink.emit(ServerFrame::UserMessage {
                    message_id: existing.id,
                    conversation_id,
                })
                .await;
                return Ok(());
            }
        }
    }

    // Effective authority (SOUL §19/§26): a grant-scoped session's capabilities are
    // the grant's, re-resolved fresh so a deleted grant fails the turn closed —
    // mirroring `Auth::capabilities()`; only a grantless session gets role base caps.
    // Resolved before the user message persists: the `/<skill>` gate below (and the
    // profile intersection further down) both need it.
    let user_caps = match principal.grant_id {
        Some(gid) => state.store().grants().get(ws_id, gid).await?.capabilities,
        None => catalerum_iam::base_capabilities(principal.role),
    };

    // A `/<skill>` invocation (SOUL §12/§23): resolve + gate + snapshot BEFORE the
    // user row persists, so the runbook rides the row — this turn's seed reads it
    // back below, and every replay re-renders it (`to_chat_messages`). Gated on the
    // caller's OWN authority (their grant or role, not the profile-intersected turn
    // caps — a bound profile confines the agent's tools, not what the user may hand
    // it), with the same per-skill capability the `use_skill` tool enforces, so a
    // user can never extract a runbook their grant doesn't permit. Ignored on a
    // regenerate (the anchor's persisted snapshot stands).
    let skill_invocation = match (&frame.skill, frame.regenerate_from.is_none()) {
        (Some(name), true) => Some(resolve_skill_invocation(state, ws_id, name, &user_caps).await?),
        _ => None,
    };

    // 1. Resolve the anchoring user message. An ordinary turn persists `content`
    // as a new user message; a regenerate reuses an existing user message after
    // pruning the transcript tail it anchors (the old answer + anything after it),
    // so the loop below re-answers the same message rather than appending a turn.
    let user_msg = match frame.regenerate_from {
        Some(anchor_id) => {
            let anchor = state
                .store()
                .messages()
                .get(anchor_id)
                .await
                .map_err(|_| ApiError::NotFound)?;
            // Guard: the anchor must be a user message in *this* conversation —
            // never another thread's, and never an assistant/tool row.
            if anchor.conversation_id != conversation_id || anchor.role != MessageRole::User {
                return Err(ApiError::bad_request(
                    "can only regenerate from a user message in this conversation",
                ));
            }
            // Drop everything after the anchor (the stale answer + any later
            // exchanges); the anchor stays and is re-answered below.
            state
                .store()
                .messages()
                .delete_after(conversation_id, anchor_id)
                .await?;
            // The prune may have deleted the compaction summary's coverage
            // anchor (`summary_upto`, FK `ON DELETE SET NULL`) — re-read so the
            // seed below sees the invalidation instead of replaying against a
            // pointer to a row that no longer exists.
            conversation = state
                .store()
                .conversations()
                .get(ws_id, conversation_id)
                .await
                .map_err(|_| ApiError::NotFound)?;
            anchor
        }
        None => {
            // Persist the user turn with its attachment references (the bytes live
            // in the files store; only the references ride on the message, SOUL §9)
            // and any `/<skill>` invocation snapshot (SOUL §12/§23).
            state
                .store()
                .messages()
                .insert(&NewMessage {
                    // Persist under the pre-generated id so it matches the turn's
                    // Valkey stream key (chosen before the run spawned, SOUL §7/§12).
                    id: Some(turn_id.message_id),
                    attachments: &frame.attachments,
                    skill: skill_invocation.as_ref(),
                    ..NewMessage::text(conversation_id, MessageRole::User, &frame.content)
                })
                .await?
        }
    };

    // Acknowledge the anchoring user message the moment it exists, so the client
    // stamps its optimistic line with the server id at once (a mid-turn queued
    // message gets the same frame from the observer when it injects). On a
    // regenerate this re-announces the reused anchor — the client's stamp is
    // idempotent.
    sink.emit(ServerFrame::UserMessage {
        message_id: user_msg.id,
        conversation_id,
    })
    .await;

    // A new user turn answers (or supersedes) any pending `ask_user` question for
    // this thread (SOUL §7/§12) — whether it came from the question form or the user
    // just typing a reply. Resolve it now so the form stops showing and a later
    // `get_unresolved` fetch is clean, stamping the form's structured answers onto
    // the row when this turn carries them (a typed reply carries none — superseded).
    // Best-effort: a resolve hiccup must not fail the turn. (Runs before the loop,
    // so a fresh `ask_user` *this* turn still opens a new pending question that
    // survives.)
    let _ = state
        .store()
        .pending_questions()
        .resolve_for_conversation(
            ws_id,
            conversation_id,
            (!frame.answers.is_empty()).then_some(frame.answers.as_slice()),
        )
        .await;
    // Likewise supersede any still-**unresolved** guard-deferred approval (SOUL §19):
    // a new user turn that isn't the Approve/Reject abandons the held call. A resume
    // turn's approval is already *resolved* (with a decision), so this leaves it for
    // the guard to consult when the call is re-attempted below.
    let _ = state
        .store()
        .pending_approvals()
        .resolve_for_conversation(ws_id, conversation_id)
        .await;

    // 2. Build the chat history (replay order) into llm chat messages. This seed
    // includes any prior assistant tool-call turns + tool results, so a
    // multi-turn agent conversation replays faithfully — bounded to the most
    // recent `CHAT_HISTORY_LIMIT` messages so a long conversation can't overflow
    // the model's context window. A thread that has been **auto-compacted**
    // (SOUL §7/§12, [`crate::chat_compact`]) replays only what its rolling
    // summary doesn't cover — the summary (re-injected below) stands in for the
    // folded prefix. Both columns must be set: a half-set pair is the FK's
    // "anchor row deleted" invalidation and means no usable summary.
    let thread_summary = match (conversation.summary.as_deref(), conversation.summary_upto) {
        (Some(summary), Some(upto)) => Some((summary.to_string(), upto)),
        _ => None,
    };
    let recent = match thread_summary.as_ref() {
        Some((_, upto)) => {
            state
                .store()
                .messages()
                .list_recent_after(conversation_id, *upto, CHAT_HISTORY_LIMIT)
                .await?
        }
        None => {
            state
                .store()
                .messages()
                .list_recent(conversation_id, CHAT_HISTORY_LIMIT)
                .await?
        }
    };
    // The bounded window may begin mid-turn; start the replay at the first user
    // message so it never opens with a tool result whose originating assistant
    // tool-call was trimmed away (which some model APIs reject).
    let trimmed = trim_to_turn_boundary(&recent);
    let mut seed = to_chat_messages(trimmed);
    // Inline any user turn's image attachments as multimodal content (SOUL §7/§9),
    // so a vision model actually SEES an uploaded image and not only its text
    // reference. Done here while `seed` is still 1:1 with `trimmed` — before
    // `patch_dangling_tool_calls` inserts synthetic tool answers and shifts
    // positions. A text-only model has these stripped once the model is resolved.
    inline_image_attachments(state, ws_id, principal.user_id, trimmed, &mut seed).await;
    // …and answer any tool call whose result row never landed (a crash or dropped
    // client mid-dispatch), so the replay never dangles an unanswered call.
    patch_dangling_tool_calls(&mut seed);
    // The rolling summary leads the replayed history (right behind the system
    // prefix inserted below), standing in for everything it folded.
    if let Some((summary, _)) = &thread_summary {
        seed.insert(0, catalerum_llm::compact::summary_message(summary));
    }
    // Build the per-turn user context — ephemeral (never persisted; it rides in
    // the skipped `seed_len` prefix) so it personalizes every turn (SOUL §22):
    //   1. the user's profile (always-on structured details), and
    //   2. auto-recall — the memories most semantically relevant to *this*
    //      message, visibility-filtered (a no-op without a vector backend).
    // Read-through the sole-user personalization cache (SOUL §18) — byte-identical
    // to a direct read; multi_user never consults it.
    let profile = state.cached_profile(ws_id, principal.user_id).await?;
    let recalled = state
        .recall_memories(ws_id, Some(principal.user_id), &user_msg.content, 5)
        .await;
    if let Some(system) = guidance::user_context(&profile, &recalled) {
        seed.insert(0, ChatMessage::system(system));
    }

    // Chat picker (SOUL §19): if this thread is bound to an agent profile, run the
    // loop *as* that profile — its model, persona prompt (+ skill runbooks), and
    // tool allow-list — under the user's authority **intersected** with the
    // profile's grant (it can scope the thread down, never escalate the user).
    // Unbound → the default chat (the user's role + the workspace default model).
    // (`user_caps` — the user's own effective authority — was resolved above,
    // before the user message persisted, for the `/<skill>` gate.)
    // The user's runtime model override (SOUL §7/§13): their chosen chat model is
    // the effective default for this turn, falling back to the boot-time `[llm]`
    // config when unset. A bound agent profile's own pinned model still wins
    // (below); an unbound chat — and any profile that doesn't pin a model — runs
    // on this. A settings-read failure degrades to the config default rather than
    // failing the turn.
    // The user's per-user LLM settings, fetched once: the chat-model default below
    // and the force-image-input override consulted when gating inlined images.
    let user_llm_settings = state
        .store()
        .llm_settings()
        .get(ws_id, principal.user_id)
        .await
        .ok();
    let effective_default = user_llm_settings
        .as_ref()
        .and_then(|s| s.chat_model.clone())
        .unwrap_or_else(|| state.config().llm.default_model.clone());
    let chat_profile = match conversation.agent_profile_id {
        Some(pid) => match state.store().agent_profiles().get(ws_id, pid).await {
            Ok(mut p) => {
                // Interactive chat has a neutral persona when a bound profile
                // leaves its own system prompt empty. Set it before resolution so
                // the profile's skill runbooks are still composed after it.
                p.system_prompt =
                    Some(chat_profile_system_prompt(p.system_prompt.as_deref()).to_string());
                Some(
                    crate::profile_agent::resolve_chat_profile(
                        state.store(),
                        &effective_default,
                        &p,
                        &user_caps,
                    )
                    .await,
                )
            }
            // The bound profile vanished (the FK nulls it on delete; be defensive).
            Err(_) => None,
        },
        None => None,
    };
    // Advertise the workspace's skills (SOUL §23): one `name: description` line
    // per skill flagged `advertised` ("visible to agent") that this turn's
    // authority may actually invoke — filtered by the same per-skill
    // `skill:use@<name>` check `use_skill` enforces, so the context never dangles
    // a skill the agent would then be denied. Lets the model reach for a matching
    // runbook without a `list_skills` round-trip. Ephemeral like the rest of the
    // seed prefix; a store failure skips the block rather than failing the turn.
    {
        let turn_caps: &[Capability] = chat_profile
            .as_ref()
            .map(|cp| cp.capabilities.as_slice())
            .unwrap_or(&user_caps);
        let advertised: Vec<(String, String)> = state
            .store()
            .skills()
            .list_advertised(ws_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, _)| {
                let required = Capability::new(Action::Use, Resource::new("skill", name));
                turn_caps.iter().any(|c| c.covers(&required))
            })
            .collect();
        if let Some(block) = guidance::skills_context(&advertised) {
            seed.insert(0, ChatMessage::system(block));
        }
    }
    // Add the profile-owned system prompt ahead of dynamic context so its
    // persona frames the whole turn.
    if let Some(cp) = &chat_profile {
        seed.insert(0, ChatMessage::system(cp.system.clone()));
    }
    // Product guidance is one built-in system message. A profile's own system
    // prompt and the dynamic workspace/user context above remain separate
    // additions, so "system prompt" continues to mean the profile-owned field.
    seed.insert(
        0,
        ChatMessage::system(guidance::chat(frame.conversation_mode)),
    );
    // The seed/system prefix above is ephemeral — it frames the prompt but is
    // never persisted; only the assistant/tool turns the loop *appends* are
    // (incrementally, via the observer's `on_message`).

    // Model resolution, most specific first: the conversation's own model override
    // (the chat Settings "model" picker, SOUL §7/§12) is the most explicit per-thread
    // choice, so it wins over a bound profile's pinned model and the effective default
    // — letting a user run "as profile X but on model Y". Unset (or blank) → fall back
    // to the bound profile's model, then the user/workspace effective default.
    let model = conversation
        .model
        .clone()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| chat_profile.as_ref().map(|cp| cp.model.clone()))
        .unwrap_or_else(|| effective_default.clone());
    // Inlined images (above) only make sense for a model that accepts image input.
    // Effective capability = the gateway catalog OR a force-image override (SOUL
    // §7/§9): the user's own `image_input_models`, or the global
    // `[llm].image_input_models` — the escape hatch for a model whose catalog entry
    // under-reports `input_modalities`. If none of these hold, drop the images (the
    // file still rides as the text reference block for the agent's storage tools).
    let forced_image_input = user_llm_settings
        .as_ref()
        .is_some_and(|s| s.image_input_models.iter().any(|m| m == &model))
        || state
            .config()
            .llm
            .image_input_models
            .iter()
            .any(|m| m == &model);
    let mut input_modalities = state.model_input_modalities(&model).await;
    if forced_image_input {
        input_modalities.insert("image".to_string());
    }
    if !input_modalities.contains("image") {
        for m in &mut seed {
            m.images.clear();
        }
    }
    let mut input_modalities: Vec<String> = input_modalities.into_iter().collect();
    input_modalities.sort();
    // The model's real context window (gateway catalog, cached process-wide)
    // grounds both compaction layers: the in-run compactor's trigger inside the
    // loop, and the persistent thread fold after the turn. `None` (unknown
    // model / catalog unreachable) falls back to the compactor's default.
    let context_window = state.model_context_window(&model).await;
    // Kept past the gate below (which consumes `model`) for the post-turn
    // thread-compaction hook — the fold summarizes with the turn's own model.
    let turn_model = model.clone();

    // 3. Run the streamed agent loop. The observer appends every event to the
    // per-turn Valkey buffer (which a forwarder pipes to whichever socket holds
    // the user, cross-pod); tool calls are dispatched server-side against the
    // workspace-scoped registry. `turn` == the pre-generated turn id (its message
    // id is this anchoring user message).
    let turn = turn_id;
    // Ensure the durable cross-pod input group exists from the turn's start, so a
    // mid-turn "say" pushed by a socket forwarding this turn from another pod is
    // delivered at the next round boundary rather than missed (SOUL §12).
    let _ = state
        .bus()
        .queue()
        .ensure_group(
            &conv_input_stream(&conversation_id.to_string()),
            CONV_INPUT_GROUP,
        )
        .await;
    // Capabilities: a bound profile's grant ∩ the user's role (never escalating);
    // unbound → the user's role. Either way enforced deny-by-default at dispatch
    // (SOUL §19). The profile's tool allow-list (if any) confines the loop.
    // `invoker_caps` keeps the user's OWN authority for the observer's mid-turn
    // `/<skill>` gate (same rationale as the pre-persist gate above).
    let invoker_caps = user_caps.clone();
    let capabilities = chat_profile
        .as_ref()
        .map(|cp| cp.capabilities.clone())
        .unwrap_or(user_caps);
    // Per-run registry: the base registry + the always-on `delegate` tool, scoped to
    // this caller's authority + model + (a bound profile's) subagent allow-list
    // (empty = any workspace profile). An ephemeral worker inherits the caller's
    // model + caps; everything stays ⊆ the caller (SOUL §19).
    let allowed_subagents = chat_profile
        .as_ref()
        .map(|cp| cp.subagents.clone())
        .unwrap_or_default();
    let registry = crate::profile_agent::registry_with_delegate(
        state.registry(),
        state.store(),
        state.llm(),
        ws_id,
        &state.config().llm.default_model,
        &model,
        capabilities.clone(),
        allowed_subagents,
        // The thread a delegated subagent inherits, so its guard can defer a durable
        // approval onto the same conversation (SOUL §19).
        Some(conversation_id),
        state.subagent_runs(),
    );
    let mut request = ChatRequest::new(model.clone(), seed);
    // The thread's "thinking" picker (SOUL §7/§12): a non-empty effort is requested
    // for this turn's loop; unset/blank leaves reasoning to the provider default.
    request.reasoning_effort = conversation
        .reasoning_effort
        .clone()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    let mut ctx = ToolContext {
        workspace_id: Some(ws_id),
        user_id: Some(principal.user_id),
        // The thread this turn runs in — the `ask_user` tool (SOUL §7/§12) and the
        // tool guard's require-user-feedback path (SOUL §19) both need it to persist
        // a durable pending question / approval the client can re-fetch after a
        // reload / reconnect / restart.
        conversation_id: Some(conversation_id),
        capabilities: Some(capabilities),
        ..Default::default()
    };
    // Tool guard (SOUL §19): a bound profile's guard classifies each tool call this
    // turn. A "require-user-feedback" outcome DEFERS the call (records a durable
    // pending approval + ends the turn); the client shows an Approve/Reject prompt,
    // and approving re-runs the held call. Unbound / unguarded → no gate.
    if let Some(cp) = &chat_profile {
        ctx.gate = crate::tool_gate::build_gate(
            cp.guard.as_ref(),
            registry.clone(),
            state.store().clone(),
            ctx.clone(),
            state.llm().clone(),
            model,
        );
    }
    // Advertise `delegate` plus its model lookup and lifecycle controls:
    // a `None` allow-list already includes them; a bound profile's explicit
    // allow-list gets them appended.
    let allowed_tools = chat_profile.as_ref().and_then(|cp| {
        cp.allowed_tools.clone().map(|mut t| {
            crate::profile_agent::append_delegate_support_tools(&mut t);
            t
        })
    });

    // The transcript tail is persisted **incrementally** by the observer's
    // `on_message` (SOUL §7/§12): every assistant turn and tool result lands as
    // its own `messages` row the instant the loop produces it, so a long
    // multi-round turn is durable round-by-round and a crash / dropped socket
    // mid-loop keeps everything saved so far — rather than batching the whole
    // tail after the loop. This threads back the id of the final assistant turn
    // (the summed-usage back-fill target + the `message_done` id below).
    let mut final_assistant_id: Option<MessageId> = None;
    let outcome = {
        let mut observer = StreamObserver {
            sink,
            turn,
            state,
            conversation_id,
            ws_id,
            invoker_caps: &invoker_caps,
            intake,
            final_assistant_id: &mut final_assistant_id,
        };
        // The turn's live controls (SOUL §12): the intake's cancel token is the
        // user's Stop button, and the observer's `poll_user_input` drains the
        // intake queue at each round boundary.
        //
        // Deferred tool advertising (SOUL §7): an unconfined chat (no profile
        // allow-list) is seeded with only the discovery tools plus the tools the
        // standing nudges promise the model — instead of shipping every spec on
        // every request — and loads the rest via `search_tools`/`list_tools`. A
        // bound profile's explicit allow-list keeps full advertising of that set.
        let discovery_tools = if allowed_tools.is_none() {
            let mut seed = crate::tools::discovery_seed();
            crate::profile_agent::append_delegate_support_tools(&mut seed);
            seed.extend(["remember", "update_memory", "forget", "ask_user"].map(str::to_string));
            seed
        } else {
            Vec::new()
        };
        let config = AgentConfig {
            cancel: intake.cancel.clone(),
            discovery_tools,
            input_modalities,
            // In-run auto-compaction (SOUL §7) on the model's real window, so a
            // single long tool-heavy turn folds itself rather than overflowing.
            compaction: catalerum_llm::CompactionConfig {
                context_window,
                ..catalerum_llm::CompactionConfig::default()
            },
            ..AgentConfig::default()
        };
        run_agent_streaming(
            state.llm(),
            request,
            &registry,
            &ctx,
            &config,
            allowed_tools.as_deref(),
            &mut observer,
        )
        .await?
    };

    // 4. The transcript tail (each assistant turn + each tool result, with its
    // dispatch error flag + duration) was already persisted row-by-row by the
    // observer as the loop produced it. All that remains is the exchange's summed
    // token/cost usage, which isn't known until the loop ends: back-fill it onto
    // the **final** assistant turn so a reopened conversation replays the same
    // token info-icon / cost readout the live `message_done` frame (below) shows.
    // Every other row keeps no usage. (No final assistant turn — e.g. the loop
    // appended nothing — or no usage reported → nothing to stamp.)
    if let (Some(final_id), Some(usage)) = (final_assistant_id, outcome.usage) {
        state
            .store()
            .messages()
            .set_usage(final_id, Some(usage))
            .await?;
    }

    // 4b. Emerged-UI artifacts: if the assistant called an App authoring tool
    // this turn, surface each affected UI so the chat client mounts the
    // interpreter inline (the "emerged UI" feature). The tool already persisted
    // the row (single source of truth); we re-read it to ship the full, validated
    // definition with the frame. Best-effort — a fetch miss just omits the frame.
    for inv in &outcome.tool_invocations {
        if inv.is_error || !is_ui_authoring_tool(&inv.call.name) {
            continue;
        }
        let Some(ui_id) = serde_json::from_str::<serde_json::Value>(&inv.result)
            .ok()
            .and_then(|v| v.get("ui_id").and_then(|x| x.as_str()).map(str::to_string))
            .and_then(|s| s.parse::<UiDefinitionId>().ok())
        else {
            continue;
        };
        if let Ok(def) = state.store().ui_definitions().get(ws_id, ui_id).await {
            // Append to the turn buffer for whichever socket forwards it. A run
            // with no live reader just leaves it in the buffer — the turn completes.
            sink.emit(ServerFrame::UiArtifact {
                ui_id,
                version: def.version,
                definition: Box::new(def),
            })
            .await;
        }
    }

    // 4c. `ask_user` question forms (SOUL §7/§12): if the assistant asked this turn,
    // push the form to the client so it shows at once (the durable pending question
    // is also persisted by the tool, so a reload re-fetches it — this frame is just
    // the live fast-path). Parse the questions out of the tool result. Best-effort.
    for inv in &outcome.tool_invocations {
        if inv.is_error || inv.call.name != "ask_user" {
            continue;
        }
        let Ok(result) = serde_json::from_str::<serde_json::Value>(&inv.result) else {
            continue;
        };
        let Some(id) = result
            .get("pending_question_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let Ok(questions) = serde_json::from_value::<Vec<Question>>(
            result.get("questions").cloned().unwrap_or_default(),
        ) else {
            continue;
        };
        sink.emit(ServerFrame::QuestionRequest { id, questions })
            .await;
    }

    // 4d. Guard-deferred tool calls (SOUL §19): if a profile's tool guard held a
    // call for the user's approval this turn, push the Approve/Reject prompt so it
    // shows at once. The durable pending approval is also persisted by the gate, so
    // a reload / reconnect / restart re-fetches it — this frame is just the live
    // fast-path. The deferred call's result is the `awaiting_approval` marker.
    for inv in &outcome.tool_invocations {
        if inv.is_error {
            continue;
        }
        let Ok(result) = serde_json::from_str::<serde_json::Value>(&inv.result) else {
            continue;
        };
        if result.get("status").and_then(|v| v.as_str()) != Some("awaiting_approval") {
            continue;
        }
        let Some(id) = result
            .get("pending_approval_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        sink.emit(ServerFrame::ApprovalRequest {
            id,
            tool: result
                .get("tool")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            arguments: result.get("arguments").cloned().unwrap_or(Value::Null),
            reason: result
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        })
        .await;
    }

    // 5. Terminal message_done frame — the last entry in the turn buffer, which
    // is how every forwarder (originating or reconnecting) knows the turn ended.
    // The id is the final assistant turn; if the loop somehow appended nothing,
    // fall back to the user message id so the client still gets a well-formed
    // terminal frame.
    sink.emit(ServerFrame::MessageDone {
        message_id: final_assistant_id.unwrap_or(user_msg.id),
        user_message_id: user_msg.id,
        conversation_id,
        truncated: outcome.hit_iteration_cap || outcome.hit_tool_loop_cap,
        stopped: outcome.stopped,
        cost_usd: outcome.usage.as_ref().and_then(|u| u.cost_usd),
        prompt_tokens: outcome.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens: outcome.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens: outcome.usage.as_ref().map(|u| u.total_tokens),
        cached_tokens: outcome.usage.as_ref().map(|u| u.cached_tokens),
        cache_creation_tokens: outcome.usage.as_ref().map(|u| u.cache_creation_tokens),
        content: outcome.content,
        reconcile: false,
    })
    .await;

    // 6. Best-effort background auto-curation: mine this exchange for durable
    // memories about the user (SOUL §22). A no-op unless `[curation].enabled`;
    // never affects the turn the client just received.
    state
        .enqueue_memory_extraction(ws_id, conversation_id, principal.user_id)
        .await;

    // 7. Persistent thread auto-compaction (SOUL §7/§12): if this exchange ended
    // with its context near the model's window, fold the older transcript into
    // the conversation's rolling summary in the background, so the NEXT turn
    // seeds `[summary] + recent tail` instead of the whole history. Best-effort
    // and off the hot path — the client already has its `message_done`.
    crate::chat_compact::maybe_compact_thread(
        state,
        ws_id,
        conversation_id,
        turn_model.clone(),
        outcome.context_tokens,
        catalerum_llm::compact::estimate_tokens(&outcome.messages),
    )
    .await;

    // 8. Background auto-title/auto-tag: generate a concise title + topic tags
    // from the transcript and persist them (the client-side optimistic
    // first-message title is replaced live on its next refresh/poll). Off the
    // hot path like 6-7; a user rename pins the title and wins over the
    // generator by construction.
    crate::chat_meta::maybe_generate_meta(
        state,
        ws_id,
        conversation_id,
        turn_model,
    )
    .await;


    Ok(())
}

/// Consumer group draining a conversation's durable mid-turn input queue.
const CONV_INPUT_GROUP: &str = "run";

/// Appends each agent-loop [`StreamEvent`] to the turn's replayable Valkey buffer
/// — the streaming side of [`run_agent_streaming`], decoupled from any client
/// socket — and persists each completed transcript message the instant the loop
/// hands it over (SOUL §7/§12). A buffer-append failure never aborts the loop
/// (client delivery isn't a source of truth); a persist failure does, unwinding
/// cleanly with everything saved so far still durable.
struct StreamObserver<'a> {
    /// The turn buffer sink every frame is appended to.
    sink: &'a TurnSink,
    /// This turn's id — also the durable-input consumer name.
    turn: TurnId,
    /// Store + conversation the completed messages are persisted into.
    state: &'a AppState,
    conversation_id: ConversationId,
    /// The workspace the turn runs in — resolves a mid-turn `/<skill>` invocation.
    ws_id: WorkspaceId,
    /// The invoking user's own capabilities (their grant or role, not the
    /// profile-intersected turn caps) — gates a mid-turn `/<skill>` invocation
    /// exactly like the pre-persist gate in [`run_turn`].
    invoker_caps: &'a [Capability],
    /// The turn's live intake (SOUL §12): `poll_user_input` drains its queue of
    /// mid-turn user messages (from the originating socket) at each round boundary.
    intake: &'a TurnIntake,
    /// Threads the id of the most-recently persisted assistant message back to
    /// [`run_turn`]: once the loop ends it is the **final** assistant turn — the
    /// target of the summed-usage back-fill and the id the terminal `message_done`
    /// frame carries.
    final_assistant_id: &'a mut Option<MessageId>,
}

impl StreamObserver<'_> {
    /// Persist one injected mid-turn user message, acknowledge it to the client,
    /// and return the seed content the model sees. Shared by the local-intake and
    /// durable-input (cross-pod) paths.
    async fn inject_user_frame(
        &self,
        frame: &ClientFrame,
    ) -> catalerum_core::error::Result<Option<ChatMessage>> {
        // A queued `/<skill>` invocation resolves best-effort: on an unknown name
        // or a denied `skill:use@<name>` the message injects without the runbook —
        // the slash text still names the skill, so the agent's own `use_skill`
        // path (and its proper error) takes over.
        let skill = match frame.skill.as_deref() {
            Some(name) => resolve_skill_invocation(self.state, self.ws_id, name, self.invoker_caps)
                .await
                .ok(),
            None => None,
        };
        // A mid-turn *form reply* resolves the pending `ask_user` question exactly
        // like a turn-starting one (SOUL §7/§12) — without this, a form answered
        // while another turn streams would leave its row unresolved and re-render
        // the form on the next reload. Only a frame that actually carries answers
        // touches the row: ordinary mid-turn chatter must not supersede a question
        // the streaming turn may just have asked. Best-effort, like `run_turn`'s.
        if !frame.answers.is_empty() {
            let _ = self
                .state
                .store()
                .pending_questions()
                .resolve_for_conversation(self.ws_id, self.conversation_id, Some(&frame.answers))
                .await;
        }
        // Persist first (the durable transcript row lands exactly at this
        // boundary, matching what the model sees), then acknowledge, so the client
        // only unmarks a queued line the server has safely stored.
        let (stored, duplicate) = if let Some(id) = frame.user_message_id {
            match self.state.store().messages().get(id).await {
                Ok(existing)
                    if existing.conversation_id == self.conversation_id
                        && existing.role == MessageRole::User
                        && existing.content == frame.content =>
                {
                    (existing, true)
                }
                Ok(_) => {
                    return Err(catalerum_core::error::Error::invalid(
                        "user_message_id already belongs to a different message",
                    ));
                }
                Err(catalerum_store::StoreError::NotFound) => (
                    self.state
                        .store()
                        .messages()
                        .insert(&NewMessage {
                            id: Some(id),
                            attachments: &frame.attachments,
                            skill: skill.as_ref(),
                            ..NewMessage::text(
                                self.conversation_id,
                                MessageRole::User,
                                &frame.content,
                            )
                        })
                        .await
                        .map_err(|e| {
                            catalerum_core::error::Error::other(format!(
                                "persist queued message: {e}"
                            ))
                        })?,
                    false,
                ),
                Err(e) => {
                    return Err(catalerum_core::error::Error::other(format!(
                        "read queued message: {e}"
                    )));
                }
            }
        } else {
            (
                self.state
                    .store()
                    .messages()
                    .insert(&NewMessage {
                        attachments: &frame.attachments,
                        skill: skill.as_ref(),
                        ..NewMessage::text(self.conversation_id, MessageRole::User, &frame.content)
                    })
                    .await
                    .map_err(|e| {
                        catalerum_core::error::Error::other(format!("persist queued message: {e}"))
                    })?,
                false,
            )
        };
        self.sink
            .emit(ServerFrame::UserMessage {
                message_id: stored.id,
                conversation_id: self.conversation_id,
            })
            .await;
        if duplicate {
            return Ok(None);
        }
        // Mirror `to_chat_messages`: the runbook + attachment references ride on
        // the ephemeral seed the model sees, never inlined into the stored row.
        Ok(Some(ChatMessage::user(user_seed_content(
            &frame.content,
            skill.as_ref(),
            &frame.attachments,
        ))))
    }

    /// Drain the durable per-conversation input queue (SOUL §12) — mid-turn
    /// "say" frames pushed by a socket forwarding this turn from another pod.
    /// At-least-once: each item is acked only after it is persisted + injected.
    async fn drain_durable_input(&self) -> Vec<ChatMessage> {
        let stream_name = conv_input_stream(&self.conversation_id.to_string());
        let consumer = self.turn.message_id.to_string();
        let items = match self
            .state
            .bus()
            .queue()
            .pull(&stream_name, CONV_INPUT_GROUP, &consumer, 32, 0)
            .await
        {
            Ok(items) => items,
            Err(_) => return Vec::new(),
        };
        let mut injected = Vec::new();
        for item in items {
            if let Ok(frame) = item.json::<ClientFrame>() {
                if frame.conversation_id == self.conversation_id && frame.regenerate_from.is_none()
                {
                    if let Ok(Some(msg)) = self.inject_user_frame(&frame).await {
                        injected.push(msg);
                    }
                }
            }
            // Ack regardless — an undecodable / off-conversation item must not be
            // redelivered forever.
            let _ = self
                .state
                .bus()
                .queue()
                .ack(&stream_name, CONV_INPUT_GROUP, &item.id)
                .await;
        }
        injected
    }
}

#[async_trait]
impl TurnObserver for StreamObserver<'_> {
    async fn on_event(&mut self, event: &StreamEvent) -> catalerum_core::error::Result<()> {
        // Append to the turn buffer for whichever socket is forwarding. Crucially
        // this NEVER errors on a delivery miss (the inverse of the old direct
        // socket send): the run's liveness is decoupled from any client being
        // connected, so a dropped socket no longer cancels the turn.
        self.sink
            .emit(ServerFrame::Token {
                event: event.clone(),
            })
            .await;
        Ok(())
    }

    async fn on_message(
        &mut self,
        message: &catalerum_llm::CompletedMessage<'_>,
    ) -> catalerum_core::error::Result<()> {
        // Persist the completed transcript row immediately (SOUL §7/§12) so the
        // turn is durable round-by-round. Assistant/tool rows carry no user
        // attachments; the exchange's summed usage is back-filled onto the final
        // assistant row by `run_turn` after the loop (the total isn't known until
        // then), so each row inserts with none.
        let stored = self
            .state
            .store()
            .messages()
            .insert(&NewMessage {
                conversation_id: self.conversation_id,
                id: None,
                role: message.role,
                content: message.content,
                attachments: &[],
                skill: None,
                tool_calls: message.tool_calls,
                tool_call_id: message.tool_call_id,
                tool_is_error: message.tool_is_error,
                tool_duration_ms: message.tool_duration_ms,
                usage: None,
            })
            .await
            .map_err(|e| catalerum_core::error::Error::other(format!("persist message: {e}")))?;
        // Remember the latest assistant row; after the loop it is the final one.
        if stored.role == MessageRole::Assistant {
            *self.final_assistant_id = Some(stored.id);
        }
        Ok(())
    }

    async fn poll_user_input(&mut self) -> catalerum_core::error::Result<Vec<ChatMessage>> {
        // Take the injectable frames from the local intake (the originating
        // socket's mid-turn says): this conversation's ordinary turns, in arrival
        // order. A regenerate or another conversation's frame stays queued and
        // runs as its own turn after this one.
        let drained: Vec<ClientFrame> = {
            let mut q = self.intake.queue.lock().unwrap();
            let mut taken = Vec::new();
            q.retain(|f| {
                if f.conversation_id == self.conversation_id && f.regenerate_from.is_none() {
                    taken.push(f.clone());
                    false
                } else {
                    true
                }
            });
            taken
        };
        let mut injected = Vec::with_capacity(drained.len());
        for frame in drained {
            if let Some(message) = self.inject_user_frame(&frame).await? {
                injected.push(message);
            }
        }
        // Then the durable cross-pod says (a reconnect on another pod).
        injected.extend(self.drain_durable_input().await);
        Ok(injected)
    }
}

/// Serialize and send a server frame as a WS text frame. A send failure means
/// the client went away; we surface it as an internal error so the loop unwinds.
async fn send_frame(
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    frame: &ServerFrame,
) -> Result<(), ApiError> {
    let json = serde_json::to_string(frame).map_err(ApiError::internal)?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| ApiError::internal(format!("ws send failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chat_profile_system_prompt_uses_competent_assistant_fallback() {
        assert_eq!(
            chat_profile_system_prompt(None),
            "You are an highly competent assistant."
        );
        assert_eq!(
            chat_profile_system_prompt(Some("   \n")),
            "You are an highly competent assistant."
        );
        assert_eq!(
            chat_profile_system_prompt(Some("You are a specialist.")),
            "You are a specialist."
        );
    }

    /// A `{"stop":true}` published on the conversation's control channel cancels
    /// the streaming turn's token — the cross-pod Stop path (SOUL §12/§16 M7).
    #[tokio::test]
    async fn control_channel_stop_cancels_a_streaming_turn() {
        let bus = Bus::in_process();
        let cancel = CancellationToken::default();
        let conv = ConversationId::new();
        let turn = TurnId::new(conv, MessageId::new());
        let listener = tokio::spawn(control_listener(bus.clone(), turn, cancel.clone(), None));
        // Subscribe-before-publish: give the listener a beat (pub/sub keeps no
        // backlog).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        bus.push()
            .publish_raw(
                &catalerum_bus::conv_ctl_channel(&conv.to_string()),
                b"{\"stop\":true}".to_vec(),
            )
            .await
            .unwrap();
        for _ in 0..200 {
            if cancel.is_cancelled() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(cancel.is_cancelled(), "published stop must cancel the turn");
        listener.abort();
    }

    /// Turns on one conversation are serialized across sockets/pods: while one
    /// coordination holds the conversation, a second acquire waits (here: aborts
    /// via its cancel token), and proceeds once the first releases.
    #[tokio::test]
    async fn turn_coordination_serializes_a_conversation() {
        let bus = Bus::in_process();
        let conv = ConversationId::new();
        let turn = |mid| TurnId::new(conv, mid);
        let first = TurnCoordination::acquire(
            bus.clone(),
            turn(MessageId::new()),
            CancellationToken::default(),
        )
        .await
        .expect("first turn acquires");
        // A stop landing while queued abandons the wait with a clear error.
        let cancelled = CancellationToken::default();
        cancelled.cancel();
        assert!(
            TurnCoordination::acquire(bus.clone(), turn(MessageId::new()), cancelled)
                .await
                .is_err(),
            "a queued turn whose user stopped must not start"
        );
        drop(first); // spawned release; the next acquire polls until it lands
        let second =
            TurnCoordination::acquire(bus, turn(MessageId::new()), CancellationToken::default())
                .await
                .expect("second turn acquires after the first releases");
        drop(second);
    }

    /// A running turn advertises itself in the active-turn registry (so a
    /// reconnecting client discovers it), and the advertisement is withdrawn when
    /// the coordination drops (SOUL §7/§12).
    #[tokio::test]
    async fn coordination_advertises_and_withdraws_the_active_turn() {
        let bus = Bus::in_process();
        let turn = TurnId::new(ConversationId::new(), MessageId::new());
        let key = active_turn_key(turn.conversation_id);
        let coord = TurnCoordination::acquire(bus.clone(), turn, CancellationToken::default())
            .await
            .expect("acquire");
        let advertised = bus
            .registry()
            .lookup(&key)
            .await
            .unwrap()
            .expect("advertised");
        let parsed: ActiveTurnValue = serde_json::from_slice(&advertised).unwrap();
        assert_eq!(parsed.user_message_id, turn.message_id);

        drop(coord); // withdraw is spawned async
        let mut gone = false;
        for _ in 0..200 {
            if bus.registry().lookup(&key).await.unwrap().is_none() {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            gone,
            "dropping the coordination withdraws the advertisement"
        );
    }

    /// End-to-end of the decoupled transport (SOUL §7/§12): a run appends frames
    /// to the turn buffer via [`TurnSink`] with NO live reader; a forwarder later
    /// replays them in order from the start, each stamped with its resume cursor,
    /// terminating on the synthesized `message_done`. This is the property that
    /// lets a run outlive the client connection.
    #[tokio::test]
    async fn run_frames_buffer_without_a_reader_then_replay_in_order() {
        let bus = Bus::in_process();
        let turn = TurnId::new(ConversationId::new(), MessageId::new());
        let sink = TurnSink::new(bus.clone(), turn);
        // Simulate a run that no client is watching: anchor ack, a token, then end.
        sink.emit(ServerFrame::UserMessage {
            message_id: turn.message_id,
            conversation_id: turn.conversation_id,
        })
        .await;
        sink.emit(ServerFrame::Token {
            event: StreamEvent::TextDelta { text: "hi".into() },
        })
        .await;
        sink.finish_if_needed().await; // appends the terminal message_done

        // A forwarder attaches afterwards and replays everything from the start.
        let entries = bus.turnbuf().read(&turn, "0", 100).await.unwrap();
        assert_eq!(entries.len(), 3, "all buffered frames replay");
        let (_, t0) = stamp_seq(&entries[0].payload, &entries[0].id);
        let (token_text, t1) = stamp_seq(&entries[1].payload, &entries[1].id);
        let (_, t2) = stamp_seq(&entries[2].payload, &entries[2].id);
        assert!(!t0 && !t1 && t2, "only the terminal frame ends the forward");
        let v: Value = serde_json::from_str(&token_text).unwrap();
        assert_eq!(v["type"], "token");
        assert_eq!(
            v["seq"], entries[1].id,
            "each frame carries its resume cursor"
        );

        // A reconnecting reader resumes strictly after its last-seen cursor.
        let tail = bus
            .turnbuf()
            .read(&turn, &entries[1].id, 100)
            .await
            .unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(
            tail[0].id, entries[2].id,
            "resume skips already-seen frames"
        );
    }

    /// The forwarder stamps each buffered frame with its stream-entry id as `seq`
    /// and flags `message_done`/`error` as terminal (ends the forward loop).
    #[test]
    fn stamp_seq_adds_cursor_and_flags_terminal() {
        let (text, terminal) = stamp_seq(br#"{"type":"token","event":{}}"#, "7-0");
        assert!(!terminal);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["seq"], "7-0");
        assert_eq!(v["type"], "token");

        let (_, terminal) = stamp_seq(br#"{"type":"message_done"}"#, "9-0");
        assert!(terminal, "message_done ends the forward");
        let (_, terminal) = stamp_seq(br#"{"type":"error","message":"x"}"#, "9-0");
        assert!(terminal, "error ends the forward");
    }

    /// An attach's `resume_after` must be a well-formed stream cursor; anything
    /// else falls back to `"0"` rather than erroring every XREAD (which would
    /// stall the forward loop in its error backoff).
    #[test]
    fn stream_cursor_validation() {
        for ok in ["0", "7", "1718200000000-0", "1718200000000-12"] {
            assert!(is_valid_stream_cursor(ok), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "-",
            "1-",
            "-1",
            "1-2-3",
            "abc",
            "1718200000000-x",
            "$",
            "0x1",
            " 1",
        ] {
            assert!(!is_valid_stream_cursor(bad), "{bad:?} should be rejected");
        }
    }

    /// A chat frame, an approval frame, a stop frame, and an attach frame parse
    /// apart: each fails to decode as the others, so the multi-step parse in
    /// `handle_socket` / `route_inbound` routes them correctly.
    #[test]
    fn chat_approval_stop_and_attach_frames_are_disjoint() {
        let chat = r#"{"conversation_id":"00000000-0000-0000-0000-000000000001","content":"hi","user_message_id":"00000000-0000-0000-0000-000000000003"}"#;
        assert!(serde_json::from_str::<ApprovalFrame>(chat).is_err());
        assert!(serde_json::from_str::<StopFrame>(chat).is_err());
        assert!(serde_json::from_str::<AttachFrame>(chat).is_err());
        let chat: ClientFrame = serde_json::from_str(chat).unwrap();
        assert_eq!(
            chat.user_message_id,
            Some("00000000-0000-0000-0000-000000000003".parse().unwrap())
        );
        assert!(!chat.conversation_mode);

        let spoken = r#"{"conversation_id":"00000000-0000-0000-0000-000000000001","content":"hi","conversation_mode":true}"#;
        assert!(
            serde_json::from_str::<ClientFrame>(spoken)
                .unwrap()
                .conversation_mode
        );

        let approval = r#"{"approval_id":"ap-7","approved":true}"#;
        assert!(serde_json::from_str::<ClientFrame>(approval).is_err());
        assert!(serde_json::from_str::<StopFrame>(approval).is_err());
        assert!(serde_json::from_str::<AttachFrame>(approval).is_err());
        let a: ApprovalFrame = serde_json::from_str(approval).unwrap();
        assert_eq!(a.approval_id, "ap-7");
        assert!(a.approved);

        let stop = r#"{"stop":true}"#;
        assert!(serde_json::from_str::<ClientFrame>(stop).is_err());
        assert!(serde_json::from_str::<ApprovalFrame>(stop).is_err());
        assert!(serde_json::from_str::<AttachFrame>(stop).is_err());
        assert!(serde_json::from_str::<StopFrame>(stop).unwrap().stop);

        let attach = r#"{"attach":"00000000-0000-0000-0000-000000000001","user_message_id":"00000000-0000-0000-0000-000000000002","resume_after":"5-0"}"#;
        assert!(serde_json::from_str::<ClientFrame>(attach).is_err());
        assert!(serde_json::from_str::<StopFrame>(attach).is_err());
        let f: AttachFrame = serde_json::from_str(attach).unwrap();
        assert_eq!(f.resume_after.as_deref(), Some("5-0"));
    }

    /// The `UserMessage` ack frame serializes to the `user_message` tag the web
    /// client matches on.
    #[test]
    fn user_message_frame_tag() {
        let frame = ServerFrame::UserMessage {
            message_id: MessageId::new(),
            conversation_id: ConversationId::new(),
        };
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "user_message");
        assert!(v["message_id"].is_string());
    }

    #[test]
    fn heartbeat_is_a_distinct_transport_frame() {
        let v: serde_json::Value = serde_json::to_value(ServerFrame::Heartbeat).unwrap();
        assert_eq!(v["type"], "heartbeat");
    }

    /// The `ApprovalRequest` server frame serializes to the `approval_request` tag
    /// the web client matches on.
    #[test]
    fn approval_request_frame_tag() {
        let frame = ServerFrame::ApprovalRequest {
            id: "ap-3".into(),
            tool: "delete_object".into(),
            arguments: serde_json::json!({ "key": "x" }),
            reason: "a delete".into(),
        };
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "approval_request");
        assert_eq!(v["id"], "ap-3");
        assert_eq!(v["tool"], "delete_object");
    }

    /// The `QuestionRequest` server frame serializes to the `question_request` tag
    /// the web client matches on, carrying the questions array.
    #[test]
    fn question_request_frame_tag() {
        let frame = ServerFrame::QuestionRequest {
            id: "q-3".into(),
            questions: vec![Question {
                id: "tone".into(),
                text: "Which tone?".into(),
                options: vec!["formal".into(), "casual".into()],
                multiple: false,
                allow_text: true,
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["type"], "question_request");
        assert_eq!(v["id"], "q-3");
        assert_eq!(v["questions"][0]["text"], "Which tone?");
        assert_eq!(v["questions"][0]["options"][1], "casual");
    }
}
