use super::*;

/// Result of probing one half of the browser's speech UI.  `Checking` is a real
/// state rather than `false`: the controls render immediately in that state (but
/// stay disabled), so a slow gateway catalog cannot make the composer reflow
/// several seconds after first paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SpeechCapability {
    Checking,
    Available,
    Unavailable,
}

impl SpeechCapability {
    pub(super) fn from_model_count(count: usize) -> Self {
        if count == 0 {
            Self::Unavailable
        } else {
            Self::Available
        }
    }

    pub(super) fn ready(self) -> bool {
        self == Self::Available
    }
}

// `ChatPanel` is remounted whenever the user navigates away and back. Keep the
// last successful catalog answer for this page lifetime so those remounts do not
// temporarily disable controls that were already known to work. Each mount still
// revalidates in the background, so a changed gateway catalog is picked up.
thread_local! {
    pub(super) static STT_CAPABILITY_CACHE: Cell<SpeechCapability> =
        const { Cell::new(SpeechCapability::Checking) };
    pub(super) static TTS_CAPABILITY_CACHE: Cell<SpeechCapability> =
        const { Cell::new(SpeechCapability::Checking) };
}

/// Where a finished mic take's transcript goes (SOUL §7/§12): the composer's
/// dictation appends it to the draft; the voice overlay sends it as a chat turn
/// immediately. One recorder serves both — the destination is fixed when the
/// take starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RecordDest {
    /// 🎙 dictation: append the transcript to the composer draft.
    Composer,
    /// The voice-conversation overlay: send the transcript as a turn.
    Voice,
}

/// Which tab the right workbench sidebar is showing (SOUL §12). "Output" tails the
/// thread's terminal; "Settings" holds the profile / folder / model pickers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SideTab {
    /// The read-only terminal output pane (moved out of the old inline toggle).
    Output,
    /// The per-conversation pickers (run-as profile, terminal folder, model).
    Settings,
}

/// Who authored a chat line in the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The local user.
    User,
    /// The assistant (streamed).
    Assistant,
    /// A transport / contract error surfaced inline.
    Error,
}

impl Role {
    pub(super) fn label(self) -> &'static str {
        match self {
            Role::User => "you",
            Role::Assistant => "catalerum",
            Role::Error => "error",
        }
    }

    pub(super) fn css(self) -> &'static str {
        match self {
            Role::User => "msg msg-user",
            Role::Assistant => "msg msg-assistant",
            Role::Error => "msg msg-error",
        }
    }
}

/// Whether a chat row needs its prose bubble. Tool-only assistant rounds keep
/// their role label and cards, but must not leave an empty bubble above them.
/// The one intentional empty bubble is the initial streaming placeholder, which
/// disappears as soon as a tool card or inline UI supplies visible output.
pub(super) fn should_render_message_text(
    text: &str,
    streaming_assistant: bool,
    has_non_text_output: bool,
) -> bool {
    !text.trim().is_empty() || (streaming_assistant && !has_non_text_output)
}

/// Lifecycle state of a single tool call shown in the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    /// Dispatch is in flight (live spinner); no result yet.
    Running,
    /// The call returned successfully.
    Ok,
    /// The call failed (its `result` holds the error payload).
    Err,
}

/// One tool call the assistant made this turn, as rendered on its line.
///
/// Populated live (from the `ToolCallStarted`/`ToolResult` stream events) and on
/// replay (reconstructed from the persisted assistant `tool_calls` + the matching
/// `tool` result rows). Correlated by [`call_id`](Self::call_id), so it is robust
/// to parallel calls and arrival order.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallView {
    /// Provider-assigned call id — the live/replay correlation key.
    pub call_id: String,
    /// Tool/function name dispatched.
    pub name: String,
    /// JSON-encoded arguments (raw string, as on the wire).
    pub arguments: String,
    /// The tool's result string (raw JSON / text); `None` while `Running`.
    pub result: Option<String>,
    /// Current lifecycle state.
    pub status: ToolStatus,
    /// Wall-clock duration in milliseconds, when known (live, or persisted).
    pub duration_ms: Option<u64>,
    /// True when a live result was byte-capped (the full text is on reload).
    pub truncated: bool,
}

/// One rendered chat line.
#[derive(Clone, Debug, PartialEq)]
pub struct ChatLine {
    /// Stable per-render key.
    pub id: usize,
    /// The server message id (UUID string) this line came from, when known: set on
    /// replay for every transcript row, and backfilled onto a live user line from
    /// the terminal `message_done` frame. `None` until then (a streaming assistant
    /// line, or a just-pushed user line before its turn completes). A user line
    /// with `Some(_)` can be regenerated from (the id anchors the server-side
    /// re-run).
    pub message_id: Option<String>,
    /// Author of the line.
    pub role: Role,
    /// Current text (grows while an assistant turn streams).
    pub text: String,
    /// The model's reasoning ("thinking") trace, grown from `reasoning_delta`
    /// frames. Shown as a muted, collapsible block above the answer; empty for
    /// non-reasoning models and replayed history.
    pub reasoning: String,
    /// True while this assistant line is still receiving deltas.
    pub streaming: bool,
    /// True for a user line sent *while a turn was streaming* (SOUL §12), until
    /// the server's `user_message` ack confirms it was placed into the
    /// conversation — rendered dimmed with a "queued" tag meanwhile. Always
    /// `false` for assistant lines and replayed history.
    pub queued: bool,
    /// The LLM cost (USD) for this turn, set from the terminal `message_done`
    /// frame when the backend reported one, or rehydrated from the persisted
    /// assistant message on replay. Shown as a muted per-turn readout; `None` for
    /// the user's own lines, errors, and turns where no cost was reported.
    pub cost_usd: Option<f64>,
    /// The token + cache accounting for this exchange, set from the terminal
    /// `message_done` frame when the backend reported usage, or rehydrated from
    /// the persisted assistant message on replay (the usage is stored on the
    /// exchange's final assistant turn). Surfaced as a hover info-icon (with the
    /// conversation's running token total); `None` for the user's own lines,
    /// errors, and turns where no usage was reported.
    pub tokens: Option<TurnTokens>,
    /// An emerged UI to mount inline on this line, set when the assistant called
    /// an App-authoring tool this turn (live, from a `UiArtifact` frame) or on
    /// replay (correlated from the turn's tool calls). `None` for ordinary lines.
    pub ui_id: Option<String>,
    /// The mounted UI's definition version. Keyed into the inline-mount `Memo`
    /// alongside `ui_id` so a re-present/edit (same id, bumped version) re-mounts
    /// and re-fetches the fresh definition. `None` when `ui_id` is `None`.
    pub ui_version: Option<i64>,
    /// The tool calls this assistant turn made, rendered as collapsible cards
    /// under the bubble. Grows live (from tool stream events) and is rebuilt on
    /// replay from the persisted tool rows. Empty for user/error lines and turns
    /// that called no tools. App-authoring tools are excluded (they mount as an
    /// inline [`EmergedUi`] via [`ui_id`](Self::ui_id) instead).
    pub tool_calls: Vec<ToolCallView>,
    /// File / image references the user attached to this turn (SOUL §9/§12), shown
    /// as chips/thumbnails at the **top** of the bubble — above the text. Set on a
    /// live user send and rebuilt on replay from the persisted row. Empty for
    /// assistant/error lines and turns with no uploads. (The model is told about
    /// these separately, as a reference block appended to the seed it sees.)
    pub attachments: Vec<Attachment>,
}

/// A guarded tool call paused awaiting the user's approval (SOUL §19). Only one is
/// ever pending at a time (the turn blocks in the guard until it's answered), so
/// the panel holds a single `Option<PendingApproval>` rather than a per-line field.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingApproval {
    /// Correlation id to echo back in the reply.
    pub(super) id: String,
    /// The tool awaiting approval.
    pub(super) tool: String,
    /// Its arguments, rendered compactly for the prompt.
    pub(super) arguments: String,
    /// Why the guard escalated (the classifier's reason).
    pub(super) reason: String,
}

/// Turn a question form's collected [`Answer`]s into the message the user "sends"
/// to answer them (SOUL §7/§12) — one `question: answer` line per question, in
/// order, joining a multi-select and any free text. Unanswered questions read
/// `(no answer)`. This message becomes an ordinary user turn, so the assistant
/// (which sees its own `ask_user` call in the transcript) can map replies to
/// questions by their text.
pub(super) fn format_answers_message(questions: &[Question], answers: &[Answer]) -> String {
    let mut out = String::new();
    for q in questions {
        let mut parts: Vec<String> = Vec::new();
        if let Some(a) = answers.iter().find(|a| a.id == q.id) {
            parts.extend(a.selected.iter().cloned());
            if let Some(t) = a.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                parts.push(t.to_string());
            }
        }
        let value = if parts.is_empty() {
            "(no answer)".to_string()
        } else {
            parts.join(", ")
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{}: {value}", q.text));
    }
    out
}

/// Render tool arguments compactly for the approval prompt: a single-line JSON
/// string, capped so a large payload can't blow out the banner.
pub(super) fn compact_json(v: &serde_json::Value) -> String {
    let s = if v.is_null() {
        String::new()
    } else {
        v.to_string()
    };
    const MAX: usize = 300;
    if s.chars().count() > MAX {
        let mut t: String = s.chars().take(MAX).collect();
        t.push('…');
        t
    } else {
        s
    }
}

/// A recency bucket a conversation falls into, given "now". The Chat sidebar
/// renders one labelled group per distinct bucket, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Bucket {
    /// Started on the current local calendar day.
    Today,
    /// Started on the previous local calendar day.
    Yesterday,
    /// Started 2–6 local calendar days ago.
    Last7,
    /// 1–8 weeks old, keyed by the Unix day-number of its week's Monday.
    Week(i64),
    /// Older than 8 weeks, keyed by `(year, month)`.
    Month(i64, i64),
    /// `created_at` could not be parsed.
    Unknown,
}

/// A labelled run of sessions sharing a [`Bucket`], for the sidebar.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionGroup {
    /// The group heading (e.g. "Today", "Yesterday", "Week of Jun 1").
    pub(super) label: String,
    /// The conversations in this group, in list (newest-first) order.
    pub(super) items: Vec<Conversation>,
}

/// A command the composer sends into the *currently streaming* turn (SOUL §12),
/// over the per-turn channel `drive_turn` opens. The turn's read loop races these
/// against inbound frames and relays them on the same socket.
pub(super) enum MidTurnCmd {
    /// Queue another user message into the running turn; the server places it at
    /// the next round boundary and acks with a `user_message` frame.
    Say(ClientChatMessage),
    /// Stop generating: the server cancels the agent loop and ends the turn with
    /// a `message_done` flagged `stopped`.
    Stop,
}

/// An optimistic user line sent while a turn streamed, awaiting its server
/// `user_message` ack. If the turn ends before the ack (stopped / errored /
/// socket dropped), the line is removed and its content returns to the composer
/// so nothing the user typed is lost.
pub(super) struct QueuedSend {
    /// The optimistic [`ChatLine`] id.
    pub(super) line_id: usize,
    /// Stable client idempotency id, removed from the durable outbox on ack.
    pub(super) message_id: String,
    /// The composer text as typed (re-drafted on discard).
    pub(super) content: String,
    /// The staged attachment references (re-staged on discard).
    pub(super) attachments: Vec<Attachment>,
}

/// What a driven turn sends to open it: an ordinary chat message, or a decision on
/// a guard-deferred tool call (SOUL §19) — which resolves the durable approval and
/// re-runs the held call, its response streaming back like any turn.
pub(super) enum Outbound {
    /// A user turn ([`ClientChatMessage::new`]) or a regenerate.
    Chat(ClientChatMessage),
    /// Approve (`true`) / reject (`false`) a pending approval by id.
    Approval {
        approval_id: String,
        approved: bool,
        conversation_id: String,
        user_message_id: String,
    },
    /// (Re)attach to an in-flight turn's live stream (SOUL §7/§12): resume the
    /// turn's Valkey buffer from `resume_after` (or the start when `None`). Used
    /// when opening a conversation that has a turn streaming (e.g. after a reload,
    /// or a turn started elsewhere).
    Attach {
        conversation_id: String,
        user_message_id: String,
        resume_after: Option<String>,
    },
}
