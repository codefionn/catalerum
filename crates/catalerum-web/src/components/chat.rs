//! The Chat panel (SOUL §12: the M1 ChatPanel).
//!
//! A two-pane workbench: a left **sessions sidebar** and the streaming chat
//! itself. The sidebar lists this workspace's conversations grouped by recency —
//! "Last 7 days", then by week, then by month — with a "New" button that starts a
//! fresh thread. Selecting a session replays its transcript; sending a turn opens
//! (or reuses) the `/ws/chat` WebSocket via [`ChatSocket`], folds the inbound
//! [`StreamUpdate`](crate::api::StreamUpdate) stream into an incrementally growing
//! assistant message, and finalizes on `done`.
//!
//! A brand-new chat has no server id yet; on its first turn the panel
//! `POST`s `/conversations` to mint one (the WS handler rejects an unknown id),
//! then drives that id thereafter and refreshes the sidebar so the thread appears.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::channel::oneshot;
use futures::{FutureExt, StreamExt};
use gloo_timers::callback::Interval;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::calendar::read_file;
use super::emerged::EmergedUi;
use super::markdown::markdown_html;
use super::question_form::QuestionForm;
use super::tool_render::{render_tool_body, tool_summary};
use crate::api::{
    AgentProfile, Answer, Attachment, ClientChatMessage, Conversation, CreateConversation,
    ModelInfo, Question, RenameConversation, Skill, StreamUpdate, TurnTokens,
};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::components::terminal::TerminalPane;
use crate::components::voice::{self, SpeechPlayback, SpeechReqId, VoiceOverlay, VoiceState};
use crate::components::widgets::{
    attachment_href, attachment_is_image, attachment_label, copy_button, copy_to_clipboard,
    is_safe_href, model_autocomplete, model_options, row_action,
};
use crate::rest;
use crate::ws::{ChatSocket, ChatSocketError, SpeechEvent, SpeechSocket};

/// Result of probing one half of the browser's speech UI.  `Checking` is a real
/// state rather than `false`: the controls render immediately in that state (but
/// stay disabled), so a slow gateway catalog cannot make the composer reflow
/// several seconds after first paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpeechCapability {
    Checking,
    Available,
    Unavailable,
}

impl SpeechCapability {
    fn from_model_count(count: usize) -> Self {
        if count == 0 {
            Self::Unavailable
        } else {
            Self::Available
        }
    }

    fn ready(self) -> bool {
        self == Self::Available
    }
}

// `ChatPanel` is remounted whenever the user navigates away and back. Keep the
// last successful catalog answer for this page lifetime so those remounts do not
// temporarily disable controls that were already known to work. Each mount still
// revalidates in the background, so a changed gateway catalog is picked up.
thread_local! {
    static STT_CAPABILITY_CACHE: Cell<SpeechCapability> =
        const { Cell::new(SpeechCapability::Checking) };
    static TTS_CAPABILITY_CACHE: Cell<SpeechCapability> =
        const { Cell::new(SpeechCapability::Checking) };
}

/// Where a finished mic take's transcript goes (SOUL §7/§12): the composer's
/// dictation appends it to the draft; the voice overlay sends it as a chat turn
/// immediately. One recorder serves both — the destination is fixed when the
/// take starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordDest {
    /// 🎙 dictation: append the transcript to the composer draft.
    Composer,
    /// The voice-conversation overlay: send the transcript as a turn.
    Voice,
}

/// Which tab the right workbench sidebar is showing (SOUL §12). "Output" tails the
/// thread's terminal; "Settings" holds the profile / folder / model pickers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SideTab {
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
    fn label(self) -> &'static str {
        match self {
            Role::User => "you",
            Role::Assistant => "catalerum",
            Role::Error => "error",
        }
    }

    fn css(self) -> &'static str {
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
fn should_render_message_text(
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
struct PendingApproval {
    /// Correlation id to echo back in the reply.
    id: String,
    /// The tool awaiting approval.
    tool: String,
    /// Its arguments, rendered compactly for the prompt.
    arguments: String,
    /// Why the guard escalated (the classifier's reason).
    reason: String,
}

/// Turn a question form's collected [`Answer`]s into the message the user "sends"
/// to answer them (SOUL §7/§12) — one `question: answer` line per question, in
/// order, joining a multi-select and any free text. Unanswered questions read
/// `(no answer)`. This message becomes an ordinary user turn, so the assistant
/// (which sees its own `ask_user` call in the transcript) can map replies to
/// questions by their text.
fn format_answers_message(questions: &[Question], answers: &[Answer]) -> String {
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
fn compact_json(v: &serde_json::Value) -> String {
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
enum Bucket {
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
struct SessionGroup {
    /// The group heading (e.g. "Today", "Yesterday", "Week of Jun 1").
    label: String,
    /// The conversations in this group, in list (newest-first) order.
    items: Vec<Conversation>,
}

/// A command the composer sends into the *currently streaming* turn (SOUL §12),
/// over the per-turn channel `drive_turn` opens. The turn's read loop races these
/// against inbound frames and relays them on the same socket.
enum MidTurnCmd {
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
struct QueuedSend {
    /// The optimistic [`ChatLine`] id.
    line_id: usize,
    /// Stable client idempotency id, removed from the durable outbox on ack.
    message_id: String,
    /// The composer text as typed (re-drafted on discard).
    content: String,
    /// The staged attachment references (re-staged on discard).
    attachments: Vec<Attachment>,
}

/// What a driven turn sends to open it: an ordinary chat message, or a decision on
/// a guard-deferred tool call (SOUL §19) — which resolves the durable approval and
/// re-runs the held call, its response streaming back like any turn.
enum Outbound {
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

/// Await roughly `ms` milliseconds (a wasm-friendly sleep for reconnect backoff).
async fn sleep_ms(ms: u32) {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    gloo_timers::callback::Timeout::new(ms, move || {
        let _ = tx.send(());
    })
    .forget();
    let _ = rx.await;
}

fn browser_online() -> bool {
    web_sys::window().is_none_or(|w| w.navigator().on_line())
}

async fn reconnect_pause(delay_ms: u32) {
    while !browser_online() {
        sleep_ms(1_000).await;
    }
    sleep_ms(delay_ms).await;
}

/// Send a turn-opening [`Outbound`] frame on `sock`. Factored so the initial
/// send can retry once on a fresh connection: a held-open socket that idled
/// through a network drop / proxy timeout looks open until the first send
/// fails, and the failed frame never reached the server, so resending opens
/// no duplicate turn.
async fn send_outbound(sock: &mut ChatSocket, outbound: &Outbound) -> Result<(), ChatSocketError> {
    match outbound {
        Outbound::Chat(msg) => sock.send(msg).await,
        Outbound::Approval {
            approval_id,
            approved,
            conversation_id,
            user_message_id,
        } => {
            sock.send_approval(approval_id, *approved, conversation_id, user_message_id)
                .await
        }
        Outbound::Attach {
            conversation_id,
            user_message_id,
            resume_after,
        } => {
            sock.send_attach(conversation_id, user_message_id, resume_after.as_deref())
                .await
        }
    }
}

/// Reconnect the chat socket and re-send a turn-opening frame that (almost
/// certainly) never reached the server: the socket died before any server ack
/// arrived AND the caller's active-turn probe found nothing running, so
/// re-sending opens the turn rather than duplicating it. (The residual race —
/// the frame WAS delivered and its turn already finished before the probe —
/// would duplicate the message; that window is one whole turn completing
/// against a same-instant connection drop.) Bounded backoff like
/// [`reconnect_and_attach`].
async fn reconnect_and_resend(token: Option<&str>, outbound: &Outbound) -> Option<ChatSocket> {
    let mut delay_ms = 400u32;
    for _ in 0..8 {
        reconnect_pause(delay_ms).await;
        if let Ok(mut sock) = ChatSocket::connect(token) {
            if send_outbound(&mut sock, outbound).await.is_ok() {
                return Some(sock);
            }
        }
        delay_ms = (delay_ms * 2).min(10_000);
    }
    None
}

/// Reconnect the chat socket and reattach to an in-flight turn's live stream,
/// resuming from `resume_after` with bounded backoff (SOUL §7/§12). Returns the
/// reattached socket, or `None` if every attempt failed (the caller then surfaces
/// the drop). The run keeps executing server-side regardless, so a later reopen
/// still shows the completed turn.
async fn reconnect_and_attach(
    token: Option<&str>,
    conversation_id: &str,
    user_message_id: &str,
    resume_after: Option<String>,
) -> Option<ChatSocket> {
    let mut delay_ms = 400u32;
    for _ in 0..8 {
        reconnect_pause(delay_ms).await;
        if let Ok(mut sock) = ChatSocket::connect(token) {
            if sock
                .send_attach(conversation_id, user_message_id, resume_after.as_deref())
                .await
                .is_ok()
            {
                return Some(sock);
            }
        }
        delay_ms = (delay_ms * 2).min(10_000);
    }
    None
}

/// The inputs to one streamed turn over the chat socket, handed to the shared
/// `drive_turn` driver. Both the input sender ([`send_turn`]) and the per-message
/// regenerate build one and call `drive_turn.run(..)` with it.
struct DriveTurnArgs {
    /// The frame to send to open the turn.
    outbound: Outbound,
    /// The user line whose server message id is backfilled when the turn
    /// completes, so its regenerate control can target it.
    user_line_id: usize,
    /// The (empty, streaming) assistant line to fold the first round into.
    assistant_id: usize,
}

/// The Chat panel's base frontend route. An open conversation is deep-linkable
/// at `<CHAT_ROUTE>/<id>`; a fresh, not-yet-persisted chat sits at the bare
/// route.
const CHAT_ROUTE: &str = "/app/chat";
const CHAT_OUTBOX_STORAGE_KEY: &str = "catalerum_chat_outbox_v1";
/// Treat the final few pixels as the bottom. Fractional layout pixels and touch
/// momentum can otherwise leave the pane one pixel short and unexpectedly stop
/// following a streamed reply.
const CHAT_BOTTOM_SLOP_PX: i32 = 24;

fn chat_is_at_bottom(scroll_top: i32, scroll_height: i32, client_height: i32) -> bool {
    scroll_height - client_height - scroll_top <= CHAT_BOTTOM_SLOP_PX
}

#[cfg(target_arch = "wasm32")]
fn fresh_uuid() -> String {
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .map(|c| c.random_uuid())
        .unwrap_or_else(|| {
            let mut hex = String::with_capacity(32);
            for _ in 0..32 {
                let n = (js_sys::Math::random() * 16.0) as u8;
                hex.push(char::from_digit(u32::from(n), 16).unwrap_or('0'));
            }
            format!(
                "{}-{}-4{}-a{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[13..16],
                &hex[17..20],
                &hex[20..32]
            )
        })
}

#[cfg(not(target_arch = "wasm32"))]
fn fresh_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "00000000-0000-4000-a000-{:012x}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn read_chat_outbox() -> Vec<ClientChatMessage> {
    let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    else {
        return Vec::new();
    };
    storage
        .get_item(CHAT_OUTBOX_STORAGE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_chat_outbox(entries: &[ClientChatMessage]) {
    let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    else {
        return;
    };
    if entries.is_empty() {
        let _ = storage.remove_item(CHAT_OUTBOX_STORAGE_KEY);
    } else if let Ok(raw) = serde_json::to_string(entries) {
        let _ = storage.set_item(CHAT_OUTBOX_STORAGE_KEY, &raw);
    }
}

fn put_chat_outbox(message: &ClientChatMessage) {
    let Some(id) = message.user_message_id.as_deref() else {
        return;
    };
    let mut entries = read_chat_outbox();
    if let Some(existing) = entries
        .iter_mut()
        .find(|m| m.user_message_id.as_deref() == Some(id))
    {
        *existing = message.clone();
    } else {
        entries.push(message.clone());
    }
    write_chat_outbox(&entries);
}

fn remove_chat_outbox(message_id: &str) {
    let mut entries = read_chat_outbox();
    entries.retain(|m| m.user_message_id.as_deref() != Some(message_id));
    write_chat_outbox(&entries);
}

fn chat_outbox_contains(message_id: &str) -> bool {
    read_chat_outbox()
        .iter()
        .any(|m| m.user_message_id.as_deref() == Some(message_id))
}

/// The reasoning ("thinking") effort a brand-new chat starts at. It seeds the
/// pre-send picker, so the first send binds it onto the fresh conversation like
/// any other pick — choosing "Off" before sending still wins (nothing binds and
/// the thread stays at the provider default). Existing threads are untouched.
const DEFAULT_REASONING_EFFORT: &str = "low";

/// The conversation id encoded in the current browser URL (`/app/chat/<id>`),
/// if the path carries one. Seeds the open thread from a deep link or reload; a
/// bare `/app/chat` yields `None` (a fresh chat).
fn session_from_location() -> Option<String> {
    let path = web_sys::window()?.location().pathname().ok()?;
    let id = path
        .trim_end_matches('/')
        .strip_prefix(CHAT_ROUTE)?
        .trim_start_matches('/');
    (!id.is_empty()).then(|| id.to_string())
}

/// Reflect the open conversation in the browser URL: `/app/chat/<id>` for a
/// saved thread, or the bare `/app/chat` for a fresh/unsaved one. Uses
/// `replace_state` (not push) so switching threads tracks the address bar
/// without stacking per-thread history entries — browser Back leaves Chat
/// rather than cycling through visited threads. No-op when already at the URL.
fn sync_location_to_session(id: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match id {
        Some(id) => format!("{CHAT_ROUTE}/{id}"),
        None => CHAT_ROUTE.to_string(),
    };
    if let Ok(current) = window.location().pathname() {
        if current.trim_end_matches('/') == target {
            return;
        }
    }
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&target));
    }
}

/// A kept-alive `MediaRecorder.ondataavailable` handler (parked so it outlives the
/// recording that installed it).
type BlobEventClosure = Closure<dyn FnMut(web_sys::BlobEvent)>;
/// A kept-alive `MediaRecorder.onstop` handler.
type StopClosure = Closure<dyn FnMut()>;

/// Stop every track of `stream`, releasing the microphone (and the browser's
/// recording indicator). Used both on the mic setup-failure paths and by
/// `release_media` once a take has been handed off.
fn stop_stream_tracks(stream: &web_sys::MediaStream) {
    let tracks = stream.get_tracks();
    for i in 0..tracks.length() {
        if let Ok(track) = tracks.get(i).dyn_into::<web_sys::MediaStreamTrack>() {
            track.stop();
        }
    }
}

/// Arm a voice-activity auto-stop for the live recording (SOUL §7): analyse the
/// mic level every ~120 ms and, once speech has been heard, end the take after
/// ~2.5 s of near-silence — so the composer "records as long as something is
/// happening". A hard cap bounds a stuck stream. Best-effort: if the
/// `AudioContext`/analyser can't be built the manual stop still works, and the
/// speech-first guard means a silent/suspended analyser degrades to the cap rather
/// than cutting a take short. The `AudioContext` + `Interval` are parked in the
/// caller's cells so `release_media` tears them down on finish.
fn spawn_vad(
    stream: &web_sys::MediaStream,
    audio_ctx_slot: &Rc<RefCell<Option<web_sys::AudioContext>>>,
    vad_slot: &Rc<RefCell<Option<Interval>>>,
    stop_now: Rc<dyn Fn()>,
    // Voice-overlay taps (both `None` for composer dictation): the live 0..1
    // mic level for the sound-reactive orb, and a "speech was actually heard"
    // flag so an all-silence take (the 60 s cap firing) skips transcription.
    level_out: Option<RwSignal<f32>>,
    heard_out: Option<Rc<Cell<bool>>>,
) {
    let Ok(ctx) = web_sys::AudioContext::new() else {
        return;
    };
    let Ok(source) = ctx.create_media_stream_source(stream) else {
        let _ = ctx.close();
        return;
    };
    let Ok(analyser) = ctx.create_analyser() else {
        let _ = ctx.close();
        return;
    };
    analyser.set_fft_size(1024);
    if source.connect_with_audio_node(&analyser).is_err() {
        let _ = ctx.close();
        return;
    }
    let bins = analyser.frequency_bin_count() as usize;

    // Poll cadence + thresholds. Silence is judged on the waveform's RMS deviation
    // from the 128 zero-line (normalised to [0, 1]); ordinary speech clears it.
    const TICK_MS: u32 = 120;
    const SILENCE_STOP_TICKS: u32 = 21; // ~2.5 s of quiet ends the take
    const MAX_TICKS: u32 = 500; // ~60 s hard cap
    const SPEECH_RMS: f64 = 0.02;

    // `nodes.0` (the source) stays owned so the graph edge isn't GC'd mid-take;
    // `nodes.1` (the analyser) is tapped each tick.
    let nodes = (source, analyser);
    let mut heard_speech = false;
    let mut silent_ticks = 0u32;
    let mut elapsed_ticks = 0u32;
    let mut prev_level = 0f32;
    let interval = Interval::new(TICK_MS, move || {
        elapsed_ticks += 1;
        let mut buf = vec![0u8; bins];
        nodes.1.get_byte_time_domain_data(&mut buf);
        let mut sum = 0f64;
        for &s in &buf {
            let v = (f64::from(s) - 128.0) / 128.0;
            sum += v * v;
        }
        let rms = (sum / buf.len().max(1) as f64).sqrt();
        if let Some(level) = level_out {
            prev_level = voice::level_from_rms(rms, prev_level);
            level.set(prev_level);
        }
        if rms >= SPEECH_RMS {
            heard_speech = true;
            silent_ticks = 0;
            if let Some(heard) = &heard_out {
                heard.set(true);
            }
        } else if heard_speech {
            silent_ticks += 1;
        }
        if (heard_speech && silent_ticks >= SILENCE_STOP_TICKS) || elapsed_ticks >= MAX_TICKS {
            (*stop_now)();
        }
    });
    *audio_ctx_slot.borrow_mut() = Some(ctx);
    *vad_slot.borrow_mut() = Some(interval);
}

/// How many fresh-connection attempts one paragraph's synthesis gets before
/// its error surfaces on the overlay. Synthesis is stateless server-side, so
/// re-speaking the same text on a new socket is always safe.
const SPEECH_FETCH_ATTEMPTS: u32 = 3;
/// The longest silence tolerated between `/ws/speech` events before the
/// socket is presumed half-open (a dropped network keeps a browser WS "open"
/// for minutes with no close event) and the attempt retries on a fresh one.
const SPEECH_EVENT_TIMEOUT_MS: u32 = 20_000;

/// How one synthesis attempt over an open speech socket ended.
enum SpeechAttempt {
    /// The reply's audio, complete.
    Done(Vec<u8>),
    /// The server answered an error frame for this request — a synthesis
    /// failure (no TTS model, bad input), not a transport problem: retrying
    /// the same text would fail the same way.
    Refused(String),
    /// The socket closed or went silent mid-reply; worth a fresh connection.
    Dropped(String),
    /// A newer request took over the channel; abandon quietly.
    Superseded,
}

/// Collect one speak request's reply off the socket: binary chunks bracketed
/// by `speech_start`/`speech_end`, keyed to `req` (frames tagged with another
/// id — an abandoned earlier request — are skipped, never returned). Every
/// wait is capped by [`SPEECH_EVENT_TIMEOUT_MS`] so a half-open socket reads
/// as [`SpeechAttempt::Dropped`] instead of hanging the voice loop.
async fn collect_speech_reply(
    sock: &mut SpeechSocket,
    req: u64,
    speech_ids: &SpeechReqId,
) -> SpeechAttempt {
    let mut bytes = Vec::new();
    let mut collecting = false;
    loop {
        let ev = {
            let next = sock.next_event();
            futures::pin_mut!(next);
            let mut deadline = Box::pin(sleep_ms(SPEECH_EVENT_TIMEOUT_MS).fuse());
            futures::select! {
                ev = next.fuse() => Some(ev),
                () = deadline => None,
            }
        };
        match ev {
            Some(Some(SpeechEvent::Start { id, .. })) => {
                collecting = id == Some(req);
                if collecting {
                    bytes.clear();
                }
            }
            Some(Some(SpeechEvent::Chunk(chunk))) => {
                if collecting {
                    bytes.extend_from_slice(&chunk);
                }
            }
            Some(Some(SpeechEvent::End { id })) => {
                if id == Some(req) {
                    return SpeechAttempt::Done(bytes);
                }
                collecting = false;
            }
            Some(Some(SpeechEvent::Error { id, message })) => {
                if id == Some(req) || id.is_none() {
                    return SpeechAttempt::Refused(message);
                }
            }
            Some(None) => return SpeechAttempt::Dropped("the speech channel closed".to_string()),
            None => return SpeechAttempt::Dropped("speech synthesis timed out".to_string()),
        }
        if speech_ids.current() != req {
            return SpeechAttempt::Superseded;
        }
    }
}

/// Whether a REST failure is worth retrying: a transport error (network
/// drop, offline, CORS-refused during a server restart) or a transient
/// server status. A definitive 4xx (no STT model configured, bad audio)
/// would fail identically, so it surfaces at once.
fn transient_rest_error(e: &rest::RestError) -> bool {
    match e {
        rest::RestError::Transport(_) => true,
        rest::RestError::Status { status, .. } => *status == 429 || *status >= 500,
        rest::RestError::Decode(_) => false,
    }
}

/// Linearly resample and down-mix decoded microphone PCM so its wall-clock
/// duration is divided by `speed`. Keeping the original sample rate means the
/// resulting WAV really is shorter to a duration-billed STT provider (changing a
/// playback-rate flag would not affect the uploaded recording).
fn compress_pcm(channels: &[Vec<f32>], speed: f32) -> Vec<f32> {
    let input_len = channels.iter().map(Vec::len).min().unwrap_or(0);
    if input_len == 0 || channels.is_empty() {
        return Vec::new();
    }
    let speed = speed.clamp(1.0, 2.0);
    let output_len = ((input_len as f64) / f64::from(speed)).ceil() as usize;
    let mut output = Vec::with_capacity(output_len);
    for frame in 0..output_len {
        let source = (frame as f64 * f64::from(speed)).min((input_len - 1) as f64);
        let left = source.floor() as usize;
        let right = (left + 1).min(input_len - 1);
        let fraction = (source - left as f64) as f32;
        let sample = channels
            .iter()
            .map(|channel| channel[left] + (channel[right] - channel[left]) * fraction)
            .sum::<f32>()
            / channels.len() as f32;
        output.push(sample.clamp(-1.0, 1.0));
    }
    output
}

/// Encode mono floating-point PCM as a provider-friendly 16-bit WAV.
fn pcm16_wav(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>, String> {
    let data_len = samples
        .len()
        .checked_mul(2)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| "the compressed recording is too large".to_string())?;
    let riff_len = 36u32
        .checked_add(data_len)
        .ok_or_else(|| "the compressed recording is too large".to_string())?;
    let byte_rate = sample_rate
        .checked_mul(2)
        .ok_or_else(|| "invalid recording sample rate".to_string())?;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk length
    wav.extend_from_slice(&1u16.to_le_bytes()); // integer PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for &sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    Ok(wav)
}

/// Decode the browser's MediaRecorder container, time-compress it, down-mix to
/// mono, and return a self-describing WAV for `/audio/transcriptions`.
async fn compress_recording(bytes: &[u8], speed: f32) -> Result<Vec<u8>, String> {
    let ctx = web_sys::AudioContext::new()
        .map_err(|_| "could not initialize recording compression".to_string())?;
    let array = js_sys::Uint8Array::from(bytes).buffer();
    let decoded = match ctx.decode_audio_data(&array) {
        Ok(promise) => JsFuture::from(promise).await,
        Err(error) => Err(error),
    };
    let _ = ctx.close();
    let buffer: web_sys::AudioBuffer = decoded
        .map_err(|_| "could not decode the microphone recording".to_string())?
        .dyn_into()
        .map_err(|_| "recording decode returned no audio".to_string())?;
    let mut channels = Vec::with_capacity(buffer.number_of_channels() as usize);
    for channel in 0..buffer.number_of_channels() {
        channels.push(
            buffer
                .get_channel_data(channel)
                .map_err(|_| "could not read decoded microphone audio".to_string())?
                .to_vec(),
        );
    }
    let samples = compress_pcm(&channels, speed);
    if samples.is_empty() {
        return Err("the decoded microphone recording was empty".to_string());
    }
    pcm16_wav(&samples, buffer.sample_rate().round() as u32)
}

/// POST a recorded take for transcription, retrying transient failures with
/// a short backoff. The recording only exists client-side — if the POST is
/// given up, the utterance is gone and the user has to say it again — so a
/// connection blip is worth two more tries before that.
async fn transcribe_with_retry(
    token: Option<&str>,
    request_id: &str,
    bytes: &[u8],
    content_type: Option<&str>,
) -> Result<rest::Transcription, rest::RestError> {
    let mut delay = 400u32;
    let mut upload_again = true;
    for _ in 0..8 {
        let payload = if upload_again { bytes } else { &[] };
        match rest::transcribe_audio(token, request_id, payload, content_type).await {
            Err(rest::RestError::Status { status: 404, .. }) if !upload_again => {
                // The short Valkey cache lapsed or the original upload never
                // arrived. We still retain the browser recording, so re-upload it.
                upload_again = true;
                continue;
            }
            Err(e) if transient_rest_error(&e) => {
                upload_again = false;
                reconnect_pause(delay).await;
                delay = (delay * 2).min(10_000);
            }
            other => return other,
        }
    }
    rest::transcribe_audio(token, request_id, bytes, content_type).await
}

async fn create_conversation_with_retry(
    token: Option<&str>,
    body: &CreateConversation,
) -> Result<Conversation, rest::RestError> {
    let mut delay = 400u32;
    for _ in 0..8 {
        match rest::create_conversation(token, body).await {
            Err(e) if transient_rest_error(&e) => {
                reconnect_pause(delay).await;
                delay = (delay * 2).min(10_000);
            }
            other => return other,
        }
    }
    rest::create_conversation(token, body).await
}

/// Fetch one spoken paragraph's audio over the held-open `/ws/speech` socket
/// (SOUL §7/§12): take the session socket (or open one), send the speak
/// request, collect its binary chunks by request id, and hand the socket back
/// for the next paragraph. A connection drop — a stale held-open socket, a
/// failed send, a mid-reply close, or an event timeout — retries the SAME
/// paragraph on a fresh socket up to [`SPEECH_FETCH_ATTEMPTS`] times with a
/// short backoff, so a network blip costs a pause, not the rest of the reply.
/// A server-side synthesis error never retries (it would fail identically).
async fn fetch_speech_bytes(
    speech_sock: &Rc<RefCell<Option<SpeechSocket>>>,
    speech_ids: &SpeechReqId,
    voice_state: RwSignal<VoiceState>,
    text: &str,
) -> Result<Vec<u8>, String> {
    let req = speech_ids.next();
    let token = auth::resolve_token();
    let mut last_err = "could not reach speech synthesis".to_string();
    for attempt in 0..SPEECH_FETCH_ATTEMPTS {
        if attempt > 0 {
            sleep_ms(400 * attempt).await;
        }
        // Superseded (a newer paragraph/turn) or closed while backing off:
        // abandon quietly — the caller's generation check swallows this error.
        if speech_ids.current() != req || voice_state.get_untracked() == VoiceState::Off {
            return Err("superseded".to_string());
        }
        let mut sock = match speech_sock.borrow_mut().take() {
            Some(s) => s,
            None => match SpeechSocket::connect(token.as_deref()) {
                Ok(s) => s,
                Err(e) => {
                    last_err = format!("speech channel failed: {e}");
                    continue;
                }
            },
        };
        if sock.speak(req, text).await.is_err() {
            // The held-open socket was stale (or the fresh one died at once);
            // drop it and retry on a new connection.
            last_err = "could not reach speech synthesis".to_string();
            continue;
        }
        match collect_speech_reply(&mut sock, req, speech_ids).await {
            SpeechAttempt::Done(bytes) => {
                if voice_state.get_untracked() != VoiceState::Off {
                    *speech_sock.borrow_mut() = Some(sock);
                }
                if bytes.is_empty() {
                    return Err("speech synthesis returned no audio".to_string());
                }
                return Ok(bytes);
            }
            // The socket survives a refused request (the protocol keeps it
            // usable); the failure itself is permanent, so surface it.
            SpeechAttempt::Refused(message) => {
                if voice_state.get_untracked() != VoiceState::Off {
                    *speech_sock.borrow_mut() = Some(sock);
                }
                return Err(message);
            }
            SpeechAttempt::Superseded => return Err("superseded".to_string()),
            SpeechAttempt::Dropped(message) => last_err = message,
        }
    }
    Err(last_err)
}

/// The Chat panel component: a sessions sidebar beside the streaming chat. Each
/// turn carries its conversation id; a fresh chat mints one server-side on its
/// first send (the WS handler rejects an unknown id).
///
/// `resume` is a one-shot request from the History panel to open a specific
/// conversation: when this panel mounts with a `Some(id)` target it opens that
/// thread (replaying its transcript so the next turn continues it) and clears the
/// signal so a later panel switch doesn't reopen it.
#[component]
pub fn ChatPanel(resume: RwSignal<Option<String>>) -> impl IntoView {
    // The active conversation id, or `None` for a not-yet-persisted new chat.
    let active_id = RwSignal::new(Option::<String>::None);

    // The rendered transcript.
    let lines = RwSignal::new(Vec::<ChatLine>::new());
    // Follow a growing reply only while the reader is already at the bottom.
    // Scrolling up opts out until they return; opening a thread starts at its
    // latest message. The RAF flag coalesces fast token deltas into one DOM write
    // per paint and ensures the keyed message views have grown before measuring.
    let chat_log: NodeRef<leptos::html::Div> = NodeRef::new();
    let follow_chat_bottom = RwSignal::new(true);
    let chat_scroll_frame_pending = StoredValue::new(false);
    // Monotonic id source for `ChatLine`s.
    let next_id = StoredValue::new(0usize);
    // The current input box contents.
    let draft = RwSignal::new(String::new());
    // Slash commands (SOUL §12/§23): the workspace's skills feed the composer's
    // `/` menu (best-effort — empty without `skill:read`, leaving `/new` as the
    // only command), `slash_idx` is the menu's highlighted row, and
    // `slash_dismissed` hides the menu after Esc/blur until the draft changes.
    let skills = RwSignal::new(Vec::<Skill>::new());
    let slash_idx = RwSignal::new(0usize);
    let slash_dismissed = RwSignal::new(false);
    // Tab-completion session (shell-style): while `Some`, the draft is showing a
    // completion candidate and this holds the stem the user had actually typed
    // after the `/` when cycling began — the menu keeps filtering by the stem
    // (not the candidate on display), and closing the session restores it.
    let slash_stem = RwSignal::new(Option::<String>::None);
    // Per-submit handoff from `submit` to `send_turn`: the skill a parsed
    // `/<skill>` command invokes. `send_turn` consumes (and clears) it on entry,
    // so a declined send can't leak a stale invocation into an unrelated later
    // send (the question form and the emerged-UI sink never set it).
    let invoke_skill = RwSignal::new(Option::<String>::None);
    // Per-submit handoff from the `ask_user` form to `send_turn` (SOUL §7/§12):
    // the structured answers riding the reply turn, so the server stamps them
    // onto the pending question row it resolves (the durable Q&A record — the
    // formatted prose alone loses which options were picked). Same
    // consume-and-clear discipline as `invoke_skill`; only the form sets it.
    let form_answers = RwSignal::new(Option::<Vec<Answer>>::None);
    // Per-submit handoff from the hands-free overlay to `send_turn`. Typed chat
    // leaves this false; a voice transcript sets it immediately before sending,
    // and `send_turn` consumes it so the mode cannot leak into a later turn.
    let conversation_mode = RwSignal::new(false);
    // True while a turn is in flight (disables the send button).
    let sending = RwSignal::new(false);
    // Files staged for the next turn (SOUL §9/§12): each is uploaded to the user's
    // default files store and carried as a reference, never inlined. `uploads`
    // counts in-flight uploads so send waits for them; `attach_error` surfaces a
    // failed upload.
    let attachments = RwSignal::new(Vec::<Attachment>::new());
    let uploads = RwSignal::new(0usize);
    let attach_error = RwSignal::new(Option::<String>::None);
    // Microphone dictation (SOUL §7): the composer enables the always-present 🎙
    // button when the gateway offers speech-to-text models (`stt_capability`,
    // probed on mount). Tapping it records from the mic and drops the transcript
    // into `draft`.
    // `recording` is true while capturing, `transcribing` while the blob is in
    // flight to `/audio/transcriptions`, and `stt_error` surfaces a permission or
    // transcription failure.
    let stt_capability = RwSignal::new(STT_CAPABILITY_CACHE.with(std::cell::Cell::get));
    let recording = RwSignal::new(false);
    let transcribing = RwSignal::new(false);
    let stt_error = RwSignal::new(Option::<String>::None);
    let voice_input_speed = RwSignal::new(crate::api::default_voice_input_speed());
    // The voice-conversation overlay (SOUL §7/§12): the always-present 🎧 button
    // enables only when the gateway offers BOTH directions (STT for the ear, TTS
    // for the mouth).
    // `voice_state` is the hands-free loop's position, `voice_level` the live
    // 0..1 audio level driving the orb (mic while listening, playback while
    // speaking), `voice_heard` the last transcript, `voice_error` the last
    // failure worth showing.
    let tts_capability = RwSignal::new(TTS_CAPABILITY_CACHE.with(std::cell::Cell::get));
    let voice_state = RwSignal::new(VoiceState::Off);
    let voice_level = RwSignal::new(0f32);
    let voice_heard = RwSignal::new(String::new());
    let voice_error = RwSignal::new(Option::<String>::None);
    // Mic re-arm requests for the voice loop: a bump counter (not the state
    // signal) so every "listen again" re-runs the arming effect even when the
    // state already was `Listening`.
    let voice_arm = RwSignal::new(0u64);
    // Streamed reading (SOUL §7/§12): the reply is spoken **paragraph by
    // paragraph while the model streams**, not after the turn. The segmenter
    // cuts completed paragraphs out of the delta stream (fence-aware, so code
    // blocks stay whole for the "(code omitted)" elision); `voice_feed` is the
    // live channel into the current spoken turn's pump task — `None` when no
    // turn is being read aloud. Both must exist before `drive_turn` (whose
    // Append arm taps them).
    let voice_seg: Rc<RefCell<voice::ParagraphSegmenter>> =
        Rc::new(RefCell::new(voice::ParagraphSegmenter::default()));
    let voice_feed: Rc<RefCell<Option<UnboundedSender<String>>>> = Rc::new(RefCell::new(None));
    // The overlay's held-open `/ws/speech` channel (also tapped by `drive_turn`:
    // a chat-stream reconnect drops it, since the same network blip almost
    // certainly killed it too — cheaper than the next paragraph's timeout).
    let speech_sock: Rc<RefCell<Option<SpeechSocket>>> = Rc::new(RefCell::new(None));
    // Probe STT + TTS in separate tasks so the catalog requests overlap. The
    // controls already occupy their final layout slots while these are pending;
    // only interactivity waits for the answers. A failed refresh preserves a
    // previously successful page-lifetime answer, while a first-load failure
    // resolves to unavailable rather than leaving a permanent busy state.
    let token = auth::resolve_token();
    let stt_token = token.clone();
    spawn_local(async move {
        match rest::list_llm_models(stt_token.as_deref(), "stt").await {
            Ok(models) => {
                let capability = SpeechCapability::from_model_count(models.len());
                STT_CAPABILITY_CACHE.with(|cached| cached.set(capability));
                stt_capability.set(capability);
            }
            Err(_) if stt_capability.get_untracked() == SpeechCapability::Checking => {
                stt_capability.set(SpeechCapability::Unavailable);
            }
            Err(_) => {}
        }
    });
    let settings_token = auth::resolve_token();
    spawn_local(async move {
        if let Ok(settings) = rest::get_llm_settings(settings_token.as_deref()).await {
            voice_input_speed.set(settings.voice_input_speed.clamp(1.0, 2.0));
        }
    });
    spawn_local(async move {
        match rest::list_llm_models(token.as_deref(), "tts").await {
            Ok(models) => {
                let capability = SpeechCapability::from_model_count(models.len());
                TTS_CAPABILITY_CACHE.with(|cached| cached.set(capability));
                tts_capability.set(capability);
            }
            Err(_) if tts_capability.get_untracked() == SpeechCapability::Checking => {
                tts_capability.set(SpeechCapability::Unavailable);
            }
            Err(_) => {}
        }
    });

    // The sessions sidebar's data.
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let sessions_loading = RwSignal::new(true);
    let sessions_error = RwSignal::new(Option::<String>::None);
    // Sidebar search: filters the session list by a case-insensitive title match.
    let session_query = RwSignal::new(String::new());

    // The "run this chat as a profile" picker (SOUL §19): the workspace's agent
    // profiles (empty for a user without `agent_profile:read`), an in-flight flag,
    // and the last bind error. Binding scopes the thread to the profile's
    // model/tools under the user's own authority (never escalating).
    let profiles = RwSignal::new(Vec::<AgentProfile>::new());
    let binding_profile = RwSignal::new(false);
    let bind_error = RwSignal::new(Option::<String>::None);
    // Model picker (SOUL §7): the gateway's chat models that feed the autocomplete,
    // the in-flight rebind flag, and the last rebind error.
    let models = RwSignal::new(Vec::<ModelInfo>::new());
    let setting_model = RwSignal::new(false);
    let model_error = RwSignal::new(Option::<String>::None);
    // Force-image-input override (SOUL §7/§9): the user's per-user list of model ids
    // treated as image-capable regardless of the catalog. Loaded once on mount; the
    // "Model capabilities" chip reflects it and the sidebar toggle below edits it.
    let forced_image_models = RwSignal::new(Vec::<String>::new());
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(s) = rest::get_llm_settings(token.as_deref()).await {
            forced_image_models.set(s.image_input_models);
        }
    });
    // Thinking picker (SOUL §7): the reasoning effort this thread requests ("" = off /
    // provider default), an in-flight rebind flag, and the last rebind error.
    let setting_reasoning = RwSignal::new(false);
    let reasoning_error = RwSignal::new(Option::<String>::None);
    // Pre-conversation picks. A brand-new chat has no server id yet, so the three
    // pickers can't bind server-side until its first send. These hold the user's
    // choices in the meantime ("" = default/none); they're applied to the freshly
    // created conversation — before the opening turn runs — so the first turn
    // already honours them, then cleared. Once a chat is persisted the live
    // bindings take over and these go unread. Reasoning alone seeds non-empty:
    // new chats request low thinking by default.
    let pending_profile = RwSignal::new(String::new());
    let pending_model = RwSignal::new(String::new());
    let pending_reasoning = RwSignal::new(DEFAULT_REASONING_EFFORT.to_string());
    // The right workbench sidebar (SOUL §12/§20): whether it's open, and which tab
    // is showing — "Output" tails the bound terminal (the old "Show output"), and
    // "Settings" holds the per-conversation pickers.
    let sidebar_open = RwSignal::new(false);
    let sidebar_tab = RwSignal::new(SideTab::Output);
    // The Settings tab's Debug section: the "Copy chat as JSON" fetch-then-copy.
    // `copying` while the transcript fetch is in flight, `copied` for the ~1.2s
    // "Copied ✓" flash (mirrors `widgets::copy_button`, which can't be reused
    // here because the text is fetched async at click time, not read sync).
    let debug_copying = RwSignal::new(false);
    let debug_copied = RwSignal::new(false);
    let debug_copy_error = RwSignal::new(Option::<String>::None);
    // Whether the sessions sidebar is open as a drawer. Only meaningful on
    // narrow viewports where CSS turns .chat-sidebar off-canvas; the desktop
    // sidebar is a static column that ignores the class.
    let sessions_open = RwSignal::new(false);

    // The names of the workspace's **built-in** registry tools (the global `/tools`
    // catalog). A tool-call card whose name is not here — and isn't the always-on
    // `delegate` — is an external MCP tool (SOUL §26), so it gets an "MCP" badge.
    // Loaded once on mount; empty until then (cards just render without a badge).
    let builtin_tools = RwSignal::new(std::collections::HashSet::<String>::new());

    // The live socket, lazily opened on first send and reused thereafter. Held
    // in a non-reactive Rc<RefCell<…>> because `ChatSocket` is !Send/!Sync and
    // only ever touched from the single-threaded wasm task.
    let socket: Rc<RefCell<Option<ChatSocket>>> = Rc::new(RefCell::new(None));
    // The running turn's command channel (SOUL §12): set by `drive_turn` while a
    // turn streams, so the composer can queue further messages into it (the
    // server places them at the next round boundary) and the Stop button can
    // cancel it. `None` between turns. Non-reactive for the same reason as
    // `socket`.
    let turn_cmds: Rc<RefCell<Option<UnboundedSender<MidTurnCmd>>>> = Rc::new(RefCell::new(None));
    // Mid-turn sends awaiting their server `user_message` ack, oldest first.
    let queued_sends: Rc<RefCell<VecDeque<QueuedSend>>> = Rc::new(RefCell::new(VecDeque::new()));
    // True between pressing Stop and the stopped turn's terminal frame — gates
    // both controls (no double-stop, no queueing into a dying turn).
    let stopping = RwSignal::new(false);

    // Tool-guard approval (SOUL §19), durable: holds the guard-deferred tool call
    // awaiting the user's Approve/Reject. Set live from an `ApprovalRequest` frame
    // when a call is deferred, and refreshed from the server on open (so an approval
    // that outlived a reload / reconnect / restart re-renders). Cleared when the
    // user acts (Approve/Reject re-runs the held call) or submits any other turn.
    let pending_approval = RwSignal::new(Option::<PendingApproval>::None);
    // `ask_user` question form (SOUL §7/§12), durable: holds the unresolved
    // question's `questions` for this thread. Set live from a `QuestionRequest`
    // frame when the assistant asks, and refreshed from the server on open (so a
    // question that outlived a reload/reconnect re-renders). Cleared when the user
    // submits any turn (form or composer) — that turn resolves it server-side.
    let pending_questions = RwSignal::new(Option::<Vec<Question>>::None);
    // A live turn to (re)attach to after `open_session` finishes loading (SOUL
    // §7/§12): `(conversation_id, user_message_id)`. Set when opening a
    // conversation whose turn is still streaming (e.g. after a reload, or a turn
    // started in another tab); a deferred effect drives the attach once `drive_turn`
    // is in scope, resuming the live token stream from the Valkey buffer.
    let attach_on_open = RwSignal::new(Option::<(String, String)>::None);
    // Unacknowledged, locally persisted turns to resume after a reload. The
    // effect defined after `drive_turn` serially replays them with their original
    // idempotency ids once any active-turn attach has finished.
    let outbox_on_open = RwSignal::new(Vec::<ClientChatMessage>::new());
    // Generation fence for transcript loads: mobile latency can complete an old
    // conversation fetch after a newer selection; only the latest generation may
    // mutate the visible transcript or attach signals.
    let session_load_gen = StoredValue::new(0u64);

    let push_line = move |role: Role, text: String, streaming: bool| -> usize {
        let id = next_id.get_value();
        next_id.set_value(id + 1);
        lines.update(|v| {
            v.push(ChatLine {
                id,
                message_id: None,
                role,
                text,
                reasoning: String::new(),
                streaming,
                queued: false,
                cost_usd: None,
                tokens: None,
                ui_id: None,
                ui_version: None,
                tool_calls: Vec::new(),
                attachments: Vec::new(),
            });
        });
        id
    };

    let append_to = move |id: usize, frag: &str| {
        lines.update(|v| {
            if let Some(line) = v.iter_mut().find(|l| l.id == id) {
                line.text.push_str(frag);
            }
        });
    };

    let append_reasoning_to = move |id: usize, frag: &str| {
        lines.update(|v| {
            if let Some(line) = v.iter_mut().find(|l| l.id == id) {
                line.reasoning.push_str(frag);
            }
        });
    };

    let finalize = move |id: usize| {
        lines.update(|v| {
            if let Some(line) = v.iter_mut().find(|l| l.id == id) {
                line.streaming = false;
            }
        });
    };

    // Reload the workspace's conversations (newest first) into the sidebar.
    let load_sessions = move || {
        sessions_loading.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_conversations(token.as_deref()).await {
                Ok(list) => {
                    conversations.set(list);
                    sessions_error.set(None);
                }
                Err(e) => sessions_error.set(Some(e.to_string())),
            }
            sessions_loading.set(false);
        });
    };

    // Start a fresh, unsaved chat (it gets a server id on its first send).
    let new_chat = move || {
        session_load_gen.update_value(|n| *n = n.wrapping_add(1));
        active_id.set(None);
        follow_chat_bottom.set(true);
        lines.set(Vec::new());
        draft.set(String::new());
        // A durable `ask_user` form / approval prompt belongs to the thread it
        // was asked on — never carry it into a fresh chat.
        pending_questions.set(None);
        pending_approval.set(None);
        attach_on_open.set(None);
        outbox_on_open.set(Vec::new());
        // Drop the previous chat's pre-send picks so the new one starts at its
        // defaults (and any stale bind error clears with them).
        pending_profile.set(String::new());
        pending_model.set(String::new());
        pending_reasoning.set(DEFAULT_REASONING_EFFORT.to_string());
        bind_error.set(None);
        model_error.set(None);
        reasoning_error.set(None);
    };

    // Open a past session: select it and replay its transcript into the log.
    let open_session = move |id: String| {
        // Don't clobber a turn that is mid-stream.
        if sending.get_untracked() {
            return;
        }
        session_load_gen.update_value(|n| *n = n.wrapping_add(1));
        let load_generation = session_load_gen.get_value();
        active_id.set(Some(id.clone()));
        follow_chat_bottom.set(true);
        lines.set(Vec::new());
        // Clear the previous thread's `ask_user` form / approval prompt NOW —
        // the re-fetch below re-populates them for this thread, but until it
        // resolves the stale form would keep rendering over the new transcript.
        pending_questions.set(None);
        pending_approval.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let still_open = || {
                active_id.get_untracked().as_deref() == Some(id.as_str())
                    && session_load_gen.get_value() == load_generation
            };
            let messages_result = rest::list_messages(token.as_deref(), &id).await;
            if !still_open() {
                return;
            }
            match messages_result {
                Ok(msgs) => {
                    let persisted_ids: std::collections::HashSet<&str> =
                        msgs.iter().map(|m| m.id.as_str()).collect();
                    let mut pending_local: Vec<ClientChatMessage> = read_chat_outbox()
                        .into_iter()
                        .filter(|m| m.conversation_id == id)
                        .collect();
                    for mid in &persisted_ids {
                        remove_chat_outbox(mid);
                    }
                    pending_local.retain(|m| {
                        m.user_message_id
                            .as_deref()
                            .is_some_and(|mid| !persisted_ids.contains(mid))
                    });
                    // Correlate App-authoring calls with their result rows so a
                    // persisted UI re-mounts inline on reopen. A tool result arrives
                    // as a `tool` message whose content carries the `ui_id`, keyed by
                    // the call id it answers.
                    let mut ui_by_call: std::collections::HashMap<String, (String, i64)> =
                        std::collections::HashMap::new();
                    // Every tool result, keyed by the call id it answers, so each
                    // assistant turn's tool calls can be paired with their output
                    // when rebuilt below: (content, is_error, duration_ms). The flag
                    // and duration are persisted; `is_error` defaults `false` and
                    // duration `None` for transcripts recorded before they were.
                    let mut results_by_call: std::collections::HashMap<
                        String,
                        (String, bool, Option<i64>),
                    > = std::collections::HashMap::new();
                    for m in &msgs {
                        if m.role != "tool" {
                            continue;
                        }
                        if let Some(call_id) = &m.tool_call_id {
                            results_by_call.insert(
                                call_id.clone(),
                                (m.content.clone(), m.tool_is_error, m.tool_duration_ms),
                            );
                            if let Some(pair) =
                                serde_json::from_str::<serde_json::Value>(&m.content)
                                    .ok()
                                    .and_then(|v| {
                                        let uid =
                                            v.get("ui_id").and_then(|x| x.as_str())?.to_string();
                                        let ver =
                                            v.get("version").and_then(serde_json::Value::as_i64);
                                        Some((uid, ver.unwrap_or(0)))
                                    })
                            {
                                ui_by_call.insert(call_id.clone(), pair);
                            }
                        }
                    }
                    // `ask_user` Q&A replay (SOUL §7/§12): the structured answers
                    // live on the durable question rows, not in the transcript.
                    // When the thread asked anything, fetch them once and graft
                    // each row's answers into its tool call's result JSON (keyed
                    // by the `pending_question_id` the result carries), so the
                    // card below re-renders what the user actually picked.
                    // Best-effort — a fetch miss just leaves the plain card.
                    let asked = msgs.iter().any(|m| {
                        m.role == "assistant" && m.tool_calls.iter().any(|tc| tc.name == "ask_user")
                    });
                    if asked {
                        if let Ok(questions) = rest::list_questions(token.as_deref(), &id).await {
                            if !still_open() {
                                return;
                            }
                            let answers_by_id: std::collections::HashMap<String, Vec<Answer>> =
                                questions
                                    .into_iter()
                                    .filter_map(|q| q.answers.map(|a| (q.id, a)))
                                    .collect();
                            let ask_calls = msgs
                                .iter()
                                .filter(|m| m.role == "assistant")
                                .flat_map(|m| m.tool_calls.iter())
                                .filter(|tc| tc.name == "ask_user");
                            for tc in ask_calls {
                                let Some((content, _, _)) = results_by_call.get_mut(&tc.id) else {
                                    continue;
                                };
                                let Ok(mut val) =
                                    serde_json::from_str::<serde_json::Value>(content)
                                else {
                                    continue;
                                };
                                let Some(answers) = val
                                    .get("pending_question_id")
                                    .and_then(serde_json::Value::as_str)
                                    .and_then(|pqid| answers_by_id.get(pqid))
                                else {
                                    continue;
                                };
                                if let (Ok(ans), Some(obj)) =
                                    (serde_json::to_value(answers), val.as_object_mut())
                                {
                                    obj.insert("answers".into(), ans);
                                    *content = val.to_string();
                                }
                            }
                        }
                    }
                    let mut replay = Vec::new();
                    for m in &msgs {
                        // Show user/assistant turns; pure tool/system rows are noise.
                        if !matches!(m.role.as_str(), "user" | "assistant") {
                            continue;
                        }
                        // An assistant turn that only called an App-authoring tool
                        // has empty text but still mounts a UI — keep its artifact.
                        let ui = if m.role == "assistant" {
                            m.tool_calls
                                .iter()
                                .rev()
                                .find(|tc| is_ui_authoring_tool(&tc.name))
                                .and_then(|tc| ui_by_call.get(&tc.id).cloned())
                        } else {
                            None
                        };
                        let (ui_id, ui_version) = match ui {
                            Some((id, ver)) => (Some(id), Some(ver)),
                            None => (None, None),
                        };
                        // Rebuild the turn's tool calls as cards, pairing each with
                        // its result row. App-authoring tools are excluded — they
                        // mount inline as the UI above, not as a card.
                        let tool_calls: Vec<ToolCallView> = if m.role == "assistant" {
                            m.tool_calls
                                .iter()
                                .filter(|tc| !is_ui_authoring_tool(&tc.name))
                                .map(|tc| {
                                    let meta = results_by_call.get(&tc.id);
                                    let result = meta.map(|(c, _, _)| c.clone());
                                    // Prefer the persisted error flag; fall back to the
                                    // content heuristic for legacy rows (flag = false).
                                    let is_error =
                                        meta.is_some_and(|(c, e, _)| *e || looks_like_error(c));
                                    let duration_ms = meta
                                        .and_then(|(_, _, d)| *d)
                                        .and_then(|d| u64::try_from(d).ok());
                                    ToolCallView {
                                        call_id: tc.id.clone(),
                                        name: tc.name.clone(),
                                        arguments: tc.arguments.clone(),
                                        result,
                                        status: if is_error {
                                            ToolStatus::Err
                                        } else {
                                            ToolStatus::Ok
                                        },
                                        duration_ms,
                                        truncated: false,
                                    }
                                })
                                .collect()
                        } else {
                            Vec::new()
                        };
                        // Keep a row that carries anything visible: text, attachments,
                        // a mounted UI, or tool cards. (A pure tool-call turn used to
                        // vanish; an attachments-only user turn would too.)
                        if m.content.trim().is_empty()
                            && m.attachments.is_empty()
                            && ui_id.is_none()
                            && tool_calls.is_empty()
                        {
                            continue;
                        }
                        let lid = next_id.get_value();
                        next_id.set_value(lid + 1);
                        let role = if m.role == "user" {
                            Role::User
                        } else {
                            Role::Assistant
                        };
                        // Per-turn cost + token usage persisted on the assistant
                        // message (the final turn of its exchange). Replays the
                        // same cost readout + token info-icon the live turn showed;
                        // `None` on user rows / pre-feature transcripts.
                        let cost_usd = m.usage.as_ref().and_then(|u| u.cost_usd);
                        let tokens = m.usage.as_ref().map(|u| TurnTokens {
                            prompt_tokens: u.prompt_tokens,
                            completion_tokens: u.completion_tokens,
                            total_tokens: u.total_tokens,
                            cached_tokens: u.cached_tokens,
                            cache_creation_tokens: u.cache_creation_tokens,
                        });
                        replay.push(ChatLine {
                            id: lid,
                            message_id: Some(m.id.clone()),
                            role,
                            text: m.content.clone(),
                            reasoning: String::new(),
                            streaming: false,
                            queued: false,
                            cost_usd,
                            tokens,
                            ui_id,
                            ui_version,
                            tool_calls,
                            attachments: m.attachments.clone(),
                        });
                    }
                    // If a turn is still streaming for this thread (SOUL §7/§12),
                    // (re)attach to its live Valkey buffer instead of only showing
                    // persisted history. Truncate the replay at the anchoring user
                    // message so the buffer — which replays the whole assistant
                    // response — doesn't double-render the rounds already persisted;
                    // a deferred effect drives the attach.
                    match rest::get_active_turn(token.as_deref(), &id).await {
                        Ok(Some(active)) => {
                            if !still_open() {
                                return;
                            }
                            let uid = active.user_message_id;
                            if let Some(idx) = replay
                                .iter()
                                .position(|l| l.message_id.as_deref() == Some(uid.as_str()))
                            {
                                replay.truncate(idx + 1);
                            }
                            lines.set(replay);
                            attach_on_open.set(Some((id.clone(), uid)));
                            pending_local.retain(|m| {
                                m.user_message_id.as_deref()
                                    != attach_on_open
                                        .get_untracked()
                                        .as_ref()
                                        .map(|(_, uid)| uid.as_str())
                            });
                            outbox_on_open.set(pending_local);
                        }
                        _ => {
                            if !still_open() {
                                return;
                            }
                            lines.set(replay);
                            outbox_on_open.set(pending_local);
                        }
                    }
                }
                Err(e) => {
                    if !still_open() {
                        return;
                    }
                    let lid = next_id.get_value();
                    next_id.set_value(lid + 1);
                    lines.set(vec![ChatLine {
                        id: lid,
                        message_id: None,
                        role: Role::Error,
                        text: format!("Could not load conversation: {e}"),
                        reasoning: String::new(),
                        streaming: false,
                        queued: false,
                        cost_usd: None,
                        tokens: None,
                        ui_id: None,
                        ui_version: None,
                        tool_calls: Vec::new(),
                        attachments: Vec::new(),
                    }]);
                }
            }

            // Re-render any durable `ask_user` form for this thread (SOUL §7/§12): a
            // question asked before a reload / reconnect / panel-switch survives
            // server-side, so fetch and show it. Anything else (none, or an error)
            // clears the form — also drops a stale form when switching threads.
            // Skipped if the user switched threads again while this load was in
            // flight — the newer open owns these signals now.
            match rest::get_pending_question(token.as_deref(), &id).await {
                Ok(Some(pq)) if still_open() => pending_questions.set(Some(pq.questions)),
                _ if still_open() => pending_questions.set(None),
                _ => {}
            }

            // Likewise re-render any durable guard-deferred approval (SOUL §19): an
            // Approve/Reject prompt that outlived a reload / reconnect / restart is
            // persisted server-side, so fetch and show it (else clear a stale one).
            match rest::get_pending_approval(token.as_deref(), &id).await {
                Ok(Some(pa)) if still_open() => pending_approval.set(Some(PendingApproval {
                    id: pa.id,
                    tool: pa.tool,
                    arguments: compact_json(&pa.arguments),
                    reason: pa.reason,
                })),
                _ if still_open() => pending_approval.set(None),
                _ => {}
            }
        });
    };

    // Delete a conversation (and its messages, server-cascaded). If it's the open
    // one, reset to a fresh chat. No confirm — matches the Notes/Tasks delete.
    let delete_session = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_conversation(token.as_deref(), &id).await {
                Ok(()) => {
                    if active_id.get_untracked().as_deref() == Some(id.as_str()) {
                        new_chat();
                    }
                    load_sessions();
                }
                Err(e) => sessions_error.set(Some(e.to_string())),
            }
        });
    };

    // Inline rename: the conversation id being renamed (None = no editor) + its
    // draft title, seeded when the ✎ on a session is clicked.
    let renaming = RwSignal::new(Option::<String>::None);
    let rename_title = RwSignal::new(String::new());
    let submit_rename = move || {
        let Some(id) = renaming.get_untracked() else {
            return;
        };
        let title = rename_title.get_untracked();
        if title.trim().is_empty() {
            return;
        }
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = RenameConversation { title };
            match rest::rename_conversation(token.as_deref(), &id, &body).await {
                Ok(_) => {
                    renaming.set(None);
                    load_sessions();
                }
                Err(e) => sessions_error.set(Some(e.to_string())),
            }
        });
    };

    // Load the workspace's agent profiles for the picker (best-effort: a 403 for a
    // non-admin just leaves the list empty, so only "Default" is offered).
    let load_profiles = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_agent_profiles(token.as_deref()).await {
                profiles.set(list);
            }
        });
    };

    // The active conversation's currently-bound profile id ("" = default), read
    // from the loaded session list so the <select> reflects the binding.
    let current_profile = move || -> String {
        let Some(aid) = active_id.get() else {
            // No server id yet: reflect the pre-send pick.
            return pending_profile.get();
        };
        conversations
            .get()
            .iter()
            .find(|c| c.id == aid)
            .and_then(|c| c.agent_profile_id.clone())
            .unwrap_or_default()
    };

    // Bind (or unbind, on "") the active conversation to a profile, then mirror the
    // server's updated conversation into the local list so the picker stays in sync.
    let set_profile = move |value: String| {
        let Some(id) = active_id.get_untracked() else {
            // No server id yet: stash the pick; it binds on the first send.
            pending_profile.set(value);
            return;
        };
        if binding_profile.get_untracked() {
            return;
        }
        binding_profile.set(true);
        bind_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let chosen = if value.is_empty() { None } else { Some(value) };
            match rest::set_conversation_profile(token.as_deref(), &id, chosen.as_deref()).await {
                Ok(updated) => conversations.update(|list| {
                    if let Some(c) = list.iter_mut().find(|c| c.id == updated.id) {
                        c.agent_profile_id = updated.agent_profile_id.clone();
                    }
                }),
                Err(e) => bind_error.set(Some(e.to_string())),
            }
            binding_profile.set(false);
        });
    };

    // Load the workspace's skills for the composer's slash-command menu
    // (best-effort: a 403 for a user without `skill:read` just leaves the list
    // empty, so `/new` is the only command offered).
    let load_skills = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_skills(token.as_deref()).await {
                skills.set(list);
            }
        });
    };

    // Load the built-in tool catalog so tool cards can flag external MCP tools.
    // Best-effort: on failure the set stays empty (no badges) rather than failing.
    let load_builtin_tools = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_tools(token.as_deref()).await {
                let mut names: std::collections::HashSet<String> =
                    list.into_iter().map(|t| t.name).collect();
                // `delegate` is an always-on framework tool added per-run (not in the
                // global catalog), so seed it explicitly as built-in.
                names.insert("delegate".to_string());
                builtin_tools.set(names);
            }
        });
    };

    // Load the gateway's chat models for the autocomplete (best-effort: on failure
    // the field stays a free-text model-id input with no suggestions).
    let load_models = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_llm_models(token.as_deref(), "llm").await {
                models.set(list);
            }
        });
    };

    // The active conversation's pinned model ("" = no override), read from the
    // loaded session list so the picker reflects the binding.
    let current_model = move || -> String {
        let Some(aid) = active_id.get() else {
            // No server id yet: reflect the pre-send pick.
            return pending_model.get();
        };
        conversations
            .get()
            .iter()
            .find(|c| c.id == aid)
            .and_then(|c| c.model.clone())
            .unwrap_or_default()
    };

    // Pin (or clear, on "") the active conversation's model, then mirror the
    // server's updated conversation into the local list so the picker stays in sync.
    let set_model = move |value: String| {
        let Some(id) = active_id.get_untracked() else {
            // No server id yet: stash the pick; it binds on the first send.
            pending_model.set(value);
            return;
        };
        if setting_model.get_untracked() {
            return;
        }
        setting_model.set(true);
        model_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let chosen = if value.is_empty() { None } else { Some(value) };
            match rest::set_conversation_model(token.as_deref(), &id, chosen.as_deref()).await {
                Ok(updated) => conversations.update(|list| {
                    if let Some(c) = list.iter_mut().find(|c| c.id == updated.id) {
                        c.model = updated.model.clone();
                    }
                }),
                Err(e) => model_error.set(Some(e.to_string())),
            }
            setting_model.set(false);
        });
    };

    // Add/remove the current model from the per-user force-image-input list, saving
    // immediately (SOUL §7/§9) — the sidebar shortcut for the same list the Settings
    // panel manages. Lets a vision model whose catalog entry under-reports
    // `input_modalities` still receive inlined image attachments.
    let toggle_force_image = move || {
        let id = current_model();
        if id.trim().is_empty() {
            return;
        }
        let mut list = forced_image_models.get_untracked();
        if let Some(pos) = list.iter().position(|m| m == &id) {
            list.remove(pos);
        } else {
            list.push(id);
        }
        forced_image_models.set(list.clone());
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(s) = rest::set_image_input_models(token.as_deref(), &list).await {
                forced_image_models.set(s.image_input_models);
            }
        });
    };

    // The active conversation's requested reasoning effort ("" = off), read from the
    // loaded session list so the picker reflects the binding.
    let current_reasoning = move || -> String {
        let Some(aid) = active_id.get() else {
            // No server id yet: reflect the pre-send pick.
            return pending_reasoning.get();
        };
        conversations
            .get()
            .iter()
            .find(|c| c.id == aid)
            .and_then(|c| c.reasoning_effort.clone())
            .unwrap_or_default()
    };

    // Set (or clear, on "") the active conversation's reasoning effort, then mirror the
    // server's updated conversation into the local list so the picker stays in sync.
    let set_reasoning = move |value: String| {
        let Some(id) = active_id.get_untracked() else {
            // No server id yet: stash the pick; it binds on the first send.
            pending_reasoning.set(value);
            return;
        };
        if setting_reasoning.get_untracked() {
            return;
        }
        setting_reasoning.set(true);
        reasoning_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let chosen = if value.is_empty() { None } else { Some(value) };
            match rest::set_conversation_reasoning(token.as_deref(), &id, chosen.as_deref()).await {
                Ok(updated) => conversations.update(|list| {
                    if let Some(c) = list.iter_mut().find(|c| c.id == updated.id) {
                        c.reasoning_effort = updated.reasoning_effort.clone();
                    }
                }),
                Err(e) => reasoning_error.set(Some(e.to_string())),
            }
            setting_reasoning.set(false);
        });
    };

    // The Debug section's "Copy chat as JSON": fetch the persisted transcript as
    // raw JSON (every field the server stores — tool calls, errors, usage) and put
    // it on the clipboard, so a broken thread can be pasted into a bug report or
    // handed to an LLM to diagnose. Fetch-then-copy stays inside the click's
    // transient user activation as long as the fetch is quick — the browser may
    // deny the clipboard write after a very slow fetch, which fails silently
    // (same fire-and-forget contract as `copy_to_clipboard` everywhere else).
    let copy_debug_json = move || {
        let Some(id) = active_id.get_untracked() else {
            return;
        };
        if debug_copying.get_untracked() {
            return;
        }
        debug_copying.set(true);
        debug_copy_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::conversation_debug_json(token.as_deref(), &id).await {
                Ok(json) => {
                    copy_to_clipboard(&json);
                    debug_copied.set(true);
                    set_timeout(
                        move || debug_copied.set(false),
                        std::time::Duration::from_millis(1200),
                    );
                }
                Err(e) => debug_copy_error.set(Some(e.to_string())),
            }
            debug_copying.set(false);
        });
    };

    // The shared turn driver: open (or reuse) the socket, send the frame, and fold
    // the inbound stream into the assistant line — splitting a multi-round turn
    // into one bubble per round, attaching/resolving tool cards, mounting emerged
    // UIs, and recording the per-turn cost/tokens on `done`. On `done` it also
    // backfills the anchoring user line's server id (the only place the client
    // learns a live user message's id), so that line's regenerate control can
    // target it, and reloads the sidebar for a brand-new thread. Held as an
    // `UnsyncCallback` (it captures the `!Send` socket `Rc`); it is `Copy`, so both
    // the input sender and the per-message regenerate share one driver.
    let drive_turn: UnsyncCallback<DriveTurnArgs> = {
        let socket = socket.clone();
        let turn_cmds = turn_cmds.clone();
        let queued_sends = queued_sends.clone();
        let voice_seg = voice_seg.clone();
        let voice_feed = voice_feed.clone();
        let speech_sock = speech_sock.clone();
        UnsyncCallback::new(move |args: DriveTurnArgs| {
            let DriveTurnArgs {
                outbound,
                user_line_id,
                assistant_id,
            } = args;
            let socket = socket.clone();
            let turn_cmds = turn_cmds.clone();
            let queued_sends = queued_sends.clone();
            let voice_seg = voice_seg.clone();
            let voice_feed = voice_feed.clone();
            let speech_sock = speech_sock.clone();
            spawn_local(async move {
                let token = auth::resolve_token();

                // Open lazily / reuse. A failed initial open follows the same
                // connectivity-aware retry path as a mid-turn reconnect; the
                // idempotency id makes an ambiguous resend safe.
                let mut already_sent = false;
                let held_socket = { socket.borrow_mut().take() };
                let mut sock = match held_socket {
                    Some(s) => s,
                    None => match ChatSocket::connect(token.as_deref()) {
                        Ok(s) => s,
                        Err(_) => match reconnect_and_resend(token.as_deref(), &outbound).await {
                            Some(s) => {
                                already_sent = true;
                                s
                            }
                            None => {
                                mark_error(lines, assistant_id);
                                append_to(assistant_id, "[could not reconnect to chat]");
                                finalize(assistant_id);
                                sending.set(false);
                                return;
                            }
                        },
                    },
                };

                // The conversation a Stop should target — carried on the stop
                // frame so it still lands when this socket isn't the one
                // streaming (SOUL §16 M7). An approval resume knows no
                // conversation here; its stop degrades to the socket-local form.
                let stop_conversation = match &outbound {
                    Outbound::Chat(msg) => Some(msg.conversation_id.clone()),
                    Outbound::Attach {
                        conversation_id, ..
                    } => Some(conversation_id.clone()),
                    Outbound::Approval {
                        conversation_id, ..
                    } => Some(conversation_id.clone()),
                };
                // The anchoring user message id, when known — the turn key a
                // reconnect reattaches with (SOUL §7/§12). An attach carries it
                // directly; a chat/regenerate learns it from the server `user_message`
                // ack, so it is read off the anchor line lazily at reconnect time.
                let attach_anchor = match &outbound {
                    Outbound::Attach {
                        user_message_id, ..
                    } => Some(user_message_id.clone()),
                    _ => None,
                };
                let outbound_message_id = match &outbound {
                    Outbound::Chat(msg) => msg
                        .regenerate_from
                        .clone()
                        .or_else(|| msg.user_message_id.clone()),
                    Outbound::Attach {
                        user_message_id, ..
                    } => Some(user_message_id.clone()),
                    Outbound::Approval {
                        user_message_id, ..
                    } => Some(user_message_id.clone()),
                };
                let mut anchor_acked = match &outbound {
                    Outbound::Chat(msg) => msg
                        .user_message_id
                        .as_deref()
                        .is_none_or(|id| !chat_outbox_contains(id)),
                    Outbound::Approval { .. } => false,
                    Outbound::Attach { .. } => true,
                };
                let mut send_result = if already_sent {
                    Ok(())
                } else {
                    send_outbound(&mut sock, &outbound).await
                };
                if send_result.is_err() {
                    // The reused socket was stale (idled through a drop): the
                    // frame never left, so one fresh connection retries it.
                    if let Ok(fresh) = ChatSocket::connect(token.as_deref()) {
                        sock = fresh;
                        send_result = send_outbound(&mut sock, &outbound).await;
                    }
                }
                if send_result.is_err() {
                    match reconnect_and_resend(token.as_deref(), &outbound).await {
                        Some(fresh) => sock = fresh,
                        None => {
                            mark_error(lines, assistant_id);
                            append_to(assistant_id, "[could not reconnect to chat]");
                            finalize(assistant_id);
                            sending.set(false);
                            return;
                        }
                    }
                }

                // The turn is live: open its command channel so the composer can
                // queue further messages into it and the Stop button can cancel
                // it (SOUL §12). Cleared when the turn ends.
                let (cmd_tx, mut cmd_rx) = futures::channel::mpsc::unbounded::<MidTurnCmd>();
                *turn_cmds.borrow_mut() = Some(cmd_tx);
                let mut cmds_open = true;

                let mut got_text = false;
                let mut socket_alive = true;
                // Bounds how many times a mid-turn drop is transparently recovered
                // by reconnecting + reattaching to the live Valkey buffer (SOUL
                // §7/§12) before giving up and surfacing the drop.
                let mut reconnects = 0u32;
                const MAX_RECONNECTS: u32 = 20;
                const CHAT_EVENT_TIMEOUT_MS: u32 = 25_000;
                let mut resume_cursor: Option<String> = None;
                let mut stop_requested = false;
                let mut retry_says = VecDeque::<ClientChatMessage>::new();
                let mut needs_reconcile = false;
                // The assistant line currently being streamed into. A multi-round
                // turn (model text → tools → more text → …) is persisted as one
                // assistant row *per round*, so a reload renders each round's text in
                // its own bubble. Mirror that live instead of piling every round's
                // text into the single trailing text box: once a line has dispatched
                // tool calls, the next run of model prose belongs to a new round, so
                // `split_after_tools` seals that line and opens a fresh one. (Within a
                // round all text precedes the tool calls, so "prose after this line
                // already ran tools" reliably marks the round boundary; parallel tool
                // calls share one round — and one persisted row — so they don't.)
                let mut cur_id = assistant_id;
                let split_after_tools = move |id: usize| -> usize {
                    // A line "dispatched" once it carries tool cards or mounted a UI
                    // (App-authoring tools add no card but set `ui_id`); either
                    // ends the round, so the next prose opens a fresh line.
                    let dispatched = lines.with_untracked(|v| {
                        v.iter()
                            .find(|l| l.id == id)
                            .is_some_and(|l| !l.tool_calls.is_empty() || l.ui_id.is_some())
                    });
                    if dispatched {
                        finalize(id);
                        push_line(Role::Assistant, String::new(), true)
                    } else {
                        id
                    }
                };
                // One pumped event: an inbound server frame, or a composer command
                // to relay out (a queued message / a stop).
                enum TurnEv {
                    Upd(Option<StreamUpdate>),
                    Cmd(Option<MidTurnCmd>),
                    TimedOut,
                }
                loop {
                    // Race the next inbound frame against a composer command. The
                    // in-flight `next_update` future is safely dropped when a
                    // command wins (frames stay buffered in the socket until the
                    // next poll).
                    let ev = if cmds_open {
                        let upd = sock.next_update();
                        futures::pin_mut!(upd);
                        let mut deadline = Box::pin(sleep_ms(CHAT_EVENT_TIMEOUT_MS).fuse());
                        futures::select! {
                            u = upd.fuse() => TurnEv::Upd(u),
                            c = cmd_rx.next() => TurnEv::Cmd(c),
                            () = deadline => TurnEv::TimedOut,
                        }
                    } else {
                        let upd = sock.next_update();
                        futures::pin_mut!(upd);
                        let mut deadline = Box::pin(sleep_ms(CHAT_EVENT_TIMEOUT_MS).fuse());
                        futures::select! {
                            u = upd.fuse() => TurnEv::Upd(u),
                            () = deadline => TurnEv::TimedOut,
                        }
                    };
                    let update = match ev {
                        TurnEv::Cmd(Some(MidTurnCmd::Say(msg))) => {
                            if sock.send(&msg).await.is_err() {
                                retry_says.push_back(msg);
                                None
                            } else {
                                continue;
                            }
                        }
                        TurnEv::Cmd(Some(MidTurnCmd::Stop)) => {
                            stop_requested = true;
                            if sock.send_stop(stop_conversation.as_deref()).await.is_err() {
                                None
                            } else {
                                continue;
                            }
                        }
                        TurnEv::Cmd(None) => {
                            // Channel gone (shouldn't happen mid-turn) — stop
                            // selecting on it so a terminated stream can't spin.
                            cmds_open = false;
                            continue;
                        }
                        TurnEv::Upd(u) => u,
                        TurnEv::TimedOut => None,
                    };
                    if let Some(seq) = sock.last_seq() {
                        resume_cursor = Some(seq);
                    }
                    match update {
                        Some(StreamUpdate::Append(frag)) => {
                            got_text = true;
                            // Streamed voice reading (SOUL §7/§12): feed the
                            // delta into the paragraph segmenter; completed
                            // paragraphs go straight to the speech pump, so
                            // the overlay speaks while the model still writes.
                            if let Some(tx) = voice_feed.borrow().as_ref() {
                                for para in voice_seg.borrow_mut().push(&frag) {
                                    let _ = tx.unbounded_send(para);
                                }
                            }
                            cur_id = split_after_tools(cur_id);
                            append_to(cur_id, &frag);
                        }
                        Some(StreamUpdate::Reasoning(frag)) => {
                            // Thinking trace: shown apart from the answer, and not
                            // counted as answer text (so an all-reasoning turn that
                            // then drops the socket is still flagged as empty). It
                            // leads a round (before any text), so it opens the next
                            // round's bubble too.
                            cur_id = split_after_tools(cur_id);
                            append_reasoning_to(cur_id, &frag);
                        }
                        Some(StreamUpdate::ToolStarted {
                            id,
                            name,
                            arguments,
                        }) => {
                            // A tool call is a hard boundary for the spoken
                            // stream: the text before it never gets its blank
                            // line, and the next round's text must not fuse
                            // onto it mid-sentence.
                            if let Some(tx) = voice_feed.borrow().as_ref() {
                                for rest in voice_seg.borrow_mut().flush() {
                                    let _ = tx.unbounded_send(rest);
                                }
                            }
                            // A tool started running: attach a live "running" card to
                            // this turn's line. App-authoring tools mount inline as
                            // the UI instead (the `Ui` arm below), so skip their cards.
                            if !is_ui_authoring_tool(&name) {
                                lines.update(|v| {
                                    if let Some(line) = v.iter_mut().find(|l| l.id == cur_id) {
                                        line.tool_calls.push(ToolCallView {
                                            call_id: id,
                                            name,
                                            arguments,
                                            result: None,
                                            status: ToolStatus::Running,
                                            duration_ms: None,
                                            truncated: false,
                                        });
                                    }
                                });
                            }
                        }
                        Some(StreamUpdate::ToolResult {
                            id,
                            name: _,
                            result,
                            is_error,
                            duration_ms,
                            truncated,
                        }) => {
                            // Resolve the matching running card in place (keyed by
                            // call id). A result for a skipped UI tool finds nothing
                            // and is a no-op.
                            lines.update(|v| {
                                if let Some(line) = v.iter_mut().find(|l| l.id == cur_id) {
                                    if let Some(tc) =
                                        line.tool_calls.iter_mut().find(|t| t.call_id == id)
                                    {
                                        tc.result = Some(result);
                                        tc.status = if is_error {
                                            ToolStatus::Err
                                        } else {
                                            ToolStatus::Ok
                                        };
                                        tc.duration_ms = duration_ms;
                                        tc.truncated = truncated;
                                    }
                                }
                            });
                        }
                        Some(StreamUpdate::Done {
                            truncated,
                            stopped,
                            reconcile,
                            cost_usd,
                            tokens,
                            user_message_id,
                            // The voice overlay reads the *streamed* deltas
                            // paragraph by paragraph (the segmenter above), so
                            // the terminal frame's full text goes unused here.
                            content: _,
                        }) => {
                            needs_reconcile |= reconcile;
                            // The agent hit its tool-use iteration cap — flag the
                            // reply as a partial so the user doesn't read it as final.
                            if truncated {
                                append_to(
                                    cur_id,
                                    "\n\n_(response cut off — the assistant reached its tool-use limit)_",
                                );
                            }
                            // The user pressed Stop — mark the deliberate partial.
                            if stopped {
                                append_to(cur_id, "\n\n_(stopped)_");
                            }
                            // Record the turn's cost + token usage (when the backend
                            // reported them) for the per-turn readouts under the
                            // bubble. The usage is the whole exchange's summed total,
                            // so it lands on the final streaming bubble — like cost.
                            if cost_usd.is_some() || tokens.is_some() {
                                lines.update(|v| {
                                    if let Some(line) = v.iter_mut().find(|l| l.id == cur_id) {
                                        if cost_usd.is_some() {
                                            line.cost_usd = cost_usd;
                                        }
                                        if tokens.is_some() {
                                            line.tokens = tokens;
                                        }
                                    }
                                });
                            }
                            // Backfill the anchoring user line's server message id so
                            // its regenerate control can target it — kept for older
                            // servers; the `user_message` ack usually beat us to it.
                            if let Some(uid) = user_message_id {
                                remove_chat_outbox(&uid);
                                anchor_acked = true;
                                lines.update(|v| {
                                    if let Some(line) = v.iter_mut().find(|l| l.id == user_line_id)
                                    {
                                        if line.message_id.is_none() {
                                            line.message_id = Some(uid);
                                        }
                                    }
                                });
                            }
                            // Queued sends the server never placed: a stopped turn
                            // discarded them (the post-loop reclaim re-drafts
                            // them); otherwise the server answers each as its own
                            // follow-up turn on this same socket — keep reading,
                            // streaming into a fresh bubble.
                            if stopped || queued_sends.borrow().is_empty() {
                                break;
                            }
                            finalize(cur_id);
                            cur_id = push_line(Role::Assistant, String::new(), true);
                        }
                        Some(StreamUpdate::UserPlaced { message_id }) => {
                            remove_chat_outbox(&message_id);
                            // A user message was persisted server-side. The first
                            // ack of a turn is its anchor (the line `send_turn` /
                            // regenerate pushed); after that, acks resolve the
                            // mid-turn queued sends, oldest first — stamping the
                            // line's server id and clearing its "queued" dimming.
                            let is_anchor =
                                outbound_message_id.as_deref() == Some(message_id.as_str());
                            let placed = if is_anchor {
                                anchor_acked = true;
                                Some((user_line_id, false))
                            } else {
                                let mut queued = queued_sends.borrow_mut();
                                let matching = queued
                                    .iter()
                                    .position(|q| q.message_id == message_id)
                                    .and_then(|idx| queued.remove(idx));
                                matching.map(|q| (q.line_id, true)).or_else(|| {
                                    // Backward-compatible fallback for a server that
                                    // minted ids instead of echoing the client's.
                                    let unstamped = lines.with_untracked(|v| {
                                        v.iter()
                                            .find(|l| l.id == user_line_id)
                                            .is_some_and(|l| l.message_id.is_none())
                                    });
                                    if unstamped {
                                        anchor_acked = true;
                                        Some((user_line_id, false))
                                    } else {
                                        None
                                    }
                                })
                            };
                            let Some((target, was_queued)) = placed else {
                                continue;
                            };
                            lines.update(|v| {
                                if let Some(line) = v.iter_mut().find(|l| l.id == target) {
                                    if line.message_id.is_none() {
                                        line.message_id = Some(message_id);
                                    }
                                    line.queued = false;
                                }
                            });
                            if was_queued {
                                // Everything the model says next follows the placed
                                // message, so it belongs in a bubble BELOW the user
                                // line: seal the current bubble (or drop it while
                                // still empty — it would render as a stray blank
                                // above) and stream on into a fresh one at the tail.
                                let cur_empty = lines.with_untracked(|v| {
                                    v.iter().find(|l| l.id == cur_id).is_none_or(|l| {
                                        l.text.is_empty()
                                            && l.reasoning.is_empty()
                                            && l.tool_calls.is_empty()
                                            && l.ui_id.is_none()
                                    })
                                });
                                if cur_empty {
                                    lines.update(|v| v.retain(|l| l.id != cur_id));
                                } else {
                                    finalize(cur_id);
                                }
                                cur_id = push_line(Role::Assistant, String::new(), true);
                            }
                        }
                        None => {
                            // Socket closed without a Done/Error frame (server restart,
                            // network drop, or a proxy idle-timeout mid-turn). The run
                            // keeps executing server-side and streams into its Valkey
                            // buffer (SOUL §7/§12), so try to reconnect + reattach and
                            // resume the live stream exactly where it left off, rather
                            // than erroring. The anchor turn key is known for an attach,
                            // else read off the anchor line's server id.
                            let anchor = attach_anchor
                                .clone()
                                .or_else(|| {
                                    anchor_acked.then(|| outbound_message_id.clone()).flatten()
                                })
                                .or_else(|| {
                                    lines.with_untracked(|v| {
                                        v.iter()
                                            .find(|l| l.id == user_line_id)
                                            .and_then(|l| l.message_id.clone())
                                    })
                                });
                            let resume = resume_cursor.clone();
                            if reconnects < MAX_RECONNECTS {
                                if let Some(conv) = stop_conversation.clone() {
                                    // The held-open speech socket rode the same
                                    // connection that just died — drop it now so
                                    // the voice pump's next paragraph reconnects
                                    // at once instead of waiting out its event
                                    // timeout. (Idle-cheap when voice is off.)
                                    speech_sock.borrow_mut().take();
                                    // No ack ever arrived? The open frame was
                                    // probably swallowed by a zombie socket (a
                                    // buffered send into a connection that was
                                    // already dead reports no error). Ask the
                                    // server whether the turn IS running —
                                    // delivered but its ack lost — and attach;
                                    // otherwise re-send the opening frame.
                                    let recovered = match (anchor, &outbound) {
                                        (Some(uid), _) if anchor_acked => {
                                            reconnect_and_attach(
                                                token.as_deref(),
                                                &conv,
                                                &uid,
                                                resume,
                                            )
                                            .await
                                        }
                                        (_, Outbound::Chat(_)) => {
                                            resume_cursor = None;
                                            reconnect_and_resend(token.as_deref(), &outbound).await
                                        }
                                        (_, Outbound::Approval { .. }) => {
                                            reconnect_and_resend(token.as_deref(), &outbound).await
                                        }
                                        _ => None,
                                    };
                                    if let Some(new_sock) = recovered {
                                        reconnects += 1;
                                        sock = new_sock;
                                        if stop_requested
                                            && sock
                                                .send_stop(stop_conversation.as_deref())
                                                .await
                                                .is_err()
                                        {
                                            continue;
                                        }
                                        while let Some(msg) = retry_says.pop_front() {
                                            if sock.send(&msg).await.is_err() {
                                                retry_says.push_front(msg);
                                                break;
                                            }
                                        }
                                        continue; // resume the loop on the new socket
                                    }
                                }
                            }
                            // Reconnect exhausted / not possible: surface the drop.
                            // (The turn may still finish server-side; a later reopen
                            // shows it whole via persisted history.)
                            if !got_text {
                                mark_error(lines, cur_id);
                            }
                            append_to(cur_id, "\n[connection closed before the response finished]");
                            socket_alive = false;
                            needs_reconcile = true;
                            break;
                        }
                        Some(StreamUpdate::Error(msg)) => {
                            if !got_text {
                                mark_error(lines, cur_id);
                            }
                            append_to(cur_id, &format!("\n[stream error: {msg}]"));
                            needs_reconcile = true;
                            break;
                        }
                        Some(StreamUpdate::Ui { id, version }) => {
                            // The assistant created/updated an emerged UI this turn;
                            // mount it on the in-flight assistant line. The turn
                            // continues (more text/tool frames may follow). A bumped
                            // `version` for the same id re-mounts (re-fetches) it.
                            lines.update(|v| {
                                if let Some(line) = v.iter_mut().find(|l| l.id == cur_id) {
                                    line.ui_id = Some(id);
                                    line.ui_version = Some(version);
                                }
                            });
                        }
                        Some(StreamUpdate::ApprovalRequested {
                            id,
                            tool,
                            arguments,
                            reason,
                        }) => {
                            // A guarded tool call was DEFERRED for the user's approval
                            // (SOUL §19). The turn does NOT block: the pending approval is
                            // persisted server-side (durable → survives reload / reconnect
                            // / restart), so we just show the Approve/Reject prompt and
                            // keep reading (the turn ends normally). The user's decision
                            // resolves it + re-runs the held call as a fresh turn.
                            pending_approval.set(Some(PendingApproval {
                                id,
                                tool,
                                arguments: compact_json(&arguments),
                                reason,
                            }));
                        }
                        Some(StreamUpdate::QuestionsRequested { id: _, questions }) => {
                            // The assistant asked the user something (SOUL §7/§12). The
                            // turn does NOT block: the question is persisted server-side,
                            // so we just show the durable form and keep reading (the turn
                            // ends normally). The user's answer arrives as their next turn
                            // (form submit or a typed reply), which resolves it server-side.
                            pending_questions.set(Some(questions));
                        }
                        Some(StreamUpdate::Ignore) => {}
                    }
                }

                // The approval prompt and the `ask_user` form are BOTH durable — they
                // must persist after the asking turn ends, until the user acts. So
                // neither is cleared here (a durable one that outlived a reload is
                // re-fetched on load).
                finalize(cur_id);
                // The turn is over: close its command channel and reclaim any
                // never-placed queued sends — a stop discarded them server-side,
                // an error / dropped socket never answered them. Their text (and
                // attachments) return to the composer so nothing the user typed
                // is lost; the optimistic lines disappear.
                *turn_cmds.borrow_mut() = None;
                stopping.set(false);
                let leftover: Vec<QueuedSend> = queued_sends.borrow_mut().drain(..).collect();
                if !leftover.is_empty() {
                    lines.update(|v| v.retain(|l| !leftover.iter().any(|q| q.line_id == l.id)));
                    if !needs_reconcile {
                        for queued in &leftover {
                            remove_chat_outbox(&queued.message_id);
                        }
                    }
                    if !needs_reconcile {
                        // A deliberate stop discarded these server-side, so return
                        // them to the user. A connection failure instead leaves them
                        // in the durable outbox for idempotent replay after reconcile.
                        let texts: Vec<&str> = leftover
                            .iter()
                            .map(|q| q.content.as_str())
                            .filter(|s| !s.is_empty())
                            .collect();
                        if !texts.is_empty() {
                            draft.update(|d| {
                                let reclaimed = texts.join("\n");
                                *d = if d.trim().is_empty() {
                                    reclaimed
                                } else {
                                    format!("{reclaimed}\n{d}")
                                };
                            });
                        }
                        let atts: Vec<Attachment> =
                            leftover.into_iter().flat_map(|q| q.attachments).collect();
                        if !atts.is_empty() {
                            attachments.update(|list| list.extend(atts));
                        }
                    }
                }
                // Reuse the socket next turn only if it's still open; a closed one is
                // dropped here so the next send reconnects cleanly.
                if socket_alive {
                    *socket.borrow_mut() = Some(sock);
                }
                sending.set(false);

                // Keep the completed turn's id, not the current selection. The
                // user can switch chats while the metadata worker is running.
                let completed_conversation_id = stop_conversation.clone();

                if needs_reconcile {
                    if let Some(conv) = stop_conversation {
                        open_session(conv);
                    }
                }

                // Every completed turn can change the generated title and tags,
                // and every one changes the thread's recency. Reload the list now
                // and then briefly watch this specific thread for the background
                // metadata worker. A manual title ends the watch early.
                load_sessions();
                if let Some(conv_id) = completed_conversation_id {
                    spawn_local(async move {
                        for _ in 0..6 {
                            sleep_ms(5_000).await;
                            if conversations.with_untracked(|list| {
                                list.iter()
                                    .find(|conversation| conversation.id == conv_id)
                                    .is_some_and(|conversation| conversation.title_manual)
                            }) {
                                break;
                            }
                            let token = auth::resolve_token();
                            let Ok(conv) = rest::get_conversation(token.as_deref(), &conv_id).await
                            else {
                                continue;
                            };
                            let metadata_changed = conversations.with_untracked(|list| {
                                list.iter()
                                    .find(|conversation| conversation.id == conv.id)
                                    .is_some_and(|conversation| {
                                        conversation.title != conv.title
                                            || conversation.tags != conv.tags
                                            || conversation.title_manual != conv.title_manual
                                    })
                            });
                            conversations.update(|list| {
                                if let Some(c) = list.iter_mut().find(|c| c.id == conv.id) {
                                    c.title = conv.title.clone();
                                    c.tags = conv.tags.clone();
                                    c.title_manual = conv.title_manual;
                                }
                            });
                            // The metadata worker has landed. Keeping the old
                            // polling window after this only spends requests.
                            if metadata_changed || conv.title_manual {
                                break;
                            }
                        }
                    });
                }
            });
        })
    };

    // Drive a (re)attach requested by `open_session` when a conversation is opened
    // mid-turn (SOUL §7/§12): open a fresh streaming assistant bubble under the
    // anchoring user line and resume the live token stream from the Valkey buffer.
    // Deferred to an effect because `open_session` is defined before `drive_turn`.
    Effect::new(move |_| {
        let Some((conv, uid)) = attach_on_open.get() else {
            return;
        };
        attach_on_open.set(None);
        // Don't stomp a turn already streaming on this socket.
        if sending.get_untracked() {
            return;
        }
        let anchor_line = lines.with_untracked(|v| {
            v.iter()
                .find(|l| l.message_id.as_deref() == Some(uid.as_str()))
                .map(|l| l.id)
        });
        let Some(user_line_id) = anchor_line else {
            return;
        };
        sending.set(true);
        let assistant_id = push_line(Role::Assistant, String::new(), true);
        drive_turn.run(DriveTurnArgs {
            outbound: Outbound::Attach {
                conversation_id: conv,
                user_message_id: uid,
                resume_after: None,
            },
            user_line_id,
            assistant_id,
        });
    });

    // Replay locally durable, unacknowledged turns after a reload. The effect is
    // gated by `sending`, so an active-turn attach wins first and additional
    // queued messages follow serially. Every replay keeps its original message id.
    Effect::new(move |_| {
        let mut pending = outbox_on_open.get();
        if sending.get() || pending.is_empty() {
            return;
        }
        let message = pending.remove(0);
        outbox_on_open.set(pending);
        if active_id.get_untracked().as_deref() != Some(message.conversation_id.as_str()) {
            return;
        }
        let user_line_id = push_line(Role::User, message.content.clone(), false);
        lines.update(|v| {
            if let Some(line) = v.iter_mut().find(|l| l.id == user_line_id) {
                line.queued = true;
                line.attachments = message.attachments.clone();
            }
        });
        let assistant_id = push_line(Role::Assistant, String::new(), true);
        sending.set(true);
        drive_turn.run(DriveTurnArgs {
            outbound: Outbound::Chat(message),
            user_line_id,
            assistant_id,
        });
    });

    // The turn sender, parameterized by message content so both the input box and
    // a UI `ai` handler (an [`EmergedUi`] control) can start a turn. It resolves
    // (or lazily creates) the thread, then hands off to `drive_turn`. While a turn
    // is already streaming it instead *queues* the message into it (SOUL §12) —
    // the server places it at the next round boundary. Returns whether the send
    // was taken (so the composer only clears its draft for an accepted send). An
    // `UnsyncCallback` (not the `Send + Sync` `Callback`) because it drives the
    // `!Send` socket via `drive_turn`; it is still `Copy`.
    let send_turn: UnsyncCallback<String, bool> = {
        let turn_cmds = turn_cmds.clone();
        let queued_sends = queued_sends.clone();
        UnsyncCallback::new(move |content: String| {
            let content = content.trim().to_string();
            let spoken = conversation_mode.get_untracked();
            conversation_mode.set(false);
            // Consume the `/<skill>` invocation handoff unconditionally (SOUL
            // §12/§23) — even a declined send clears it, so it can never leak
            // into a later unrelated send; a re-submit re-parses the (uncleared)
            // draft and sets it again.
            let skill = invoke_skill.get_untracked();
            invoke_skill.set(None);
            // Same discipline for the `ask_user` form's structured answers
            // (SOUL §7/§12): consumed-and-cleared on entry so a declined send
            // can't stamp them onto a later unrelated turn.
            let answers = form_answers.get_untracked().unwrap_or_default();
            form_answers.set(None);
            // Snapshot + clear the staged attachments (sent as references, SOUL §9).
            // A turn may be attachments-only (empty text) — the server renders the
            // references into the prompt regardless.
            let atts = attachments.get_untracked();
            if content.is_empty() && atts.is_empty() {
                return false;
            }
            // A turn is streaming: queue this message into it. The optimistic
            // line renders dimmed until the server's `user_message` ack confirms
            // placement. Not possible while a stop is pending (the dying turn
            // discards its queue), before the first turn has minted the
            // conversation, or in the instant before the command channel opens —
            // then the composer is left untouched, so nothing is lost.
            if sending.get_untracked() {
                if stopping.get_untracked() {
                    return false;
                }
                let Some(cid) = active_id.get_untracked() else {
                    return false;
                };
                let Some(tx) = turn_cmds.borrow().clone() else {
                    return false;
                };
                // The bubble shows the typed text (empty for an attachments-only
                // turn) with the uploaded files as chips on top — no placeholder text.
                attachments.set(Vec::new());
                let line_id = push_line(Role::User, content.clone(), false);
                lines.update(|v| {
                    if let Some(line) = v.iter_mut().find(|l| l.id == line_id) {
                        line.queued = true;
                        line.attachments = atts.clone();
                    }
                });
                let message_id = fresh_uuid();
                let msg = ClientChatMessage::new(cid, content.clone())
                    .with_user_message_id(message_id.clone())
                    .with_attachments(atts.clone())
                    .with_skill(skill)
                    .with_answers(answers)
                    .with_conversation_mode(spoken);
                put_chat_outbox(&msg);
                queued_sends.borrow_mut().push_back(QueuedSend {
                    line_id,
                    message_id,
                    content: content.clone(),
                    attachments: atts.clone(),
                });
                let _ = tx.unbounded_send(MidTurnCmd::Say(msg));
                return true;
            }
            // Any submitted turn — from the question form or the composer — answers /
            // supersedes a pending `ask_user` form AND a guard-deferred approval (the
            // server resolves both on this turn), so close them here too. This also
            // covers "acted somewhere else": typing a reply instead of using the form /
            // approval prompt dismisses it.
            pending_questions.set(None);
            pending_approval.set(None);
            attachments.set(Vec::new());
            sending.set(true);

            // User line + a placeholder assistant line to stream into. Keep the user
            // line's id so `drive_turn` can backfill its server message id (for the
            // regenerate control) when the turn completes. The uploaded files render
            // as chips on top of the bubble; an attachments-only turn shows just those
            // (empty text), so no placeholder summary is needed.
            let user_line_id = push_line(Role::User, content.clone(), false);
            let assistant_id = push_line(Role::Assistant, String::new(), true);
            if !atts.is_empty() {
                lines.update(|v| {
                    if let Some(line) = v.iter_mut().find(|l| l.id == user_line_id) {
                        line.attachments = atts.clone();
                    }
                });
            }

            // Whether this thread already exists server-side.
            let existing = active_id.get_untracked();
            let was_new = existing.is_none();
            let cid = existing.unwrap_or_else(fresh_uuid);
            if was_new {
                active_id.set(Some(cid.clone()));
            }
            let message_id = fresh_uuid();
            let outbound = ClientChatMessage::new(cid.clone(), content.clone())
                .with_user_message_id(message_id)
                .with_attachments(atts.clone())
                .with_skill(skill)
                .with_answers(answers)
                .with_conversation_mode(spoken);
            put_chat_outbox(&outbound);

            spawn_local(async move {
                let token = auth::resolve_token();

                // Resolve (or lazily create) the server-side conversation id. The
                // WS handler 404s an unknown id, so a fresh chat is persisted here
                // first, titled from its opening message.
                if was_new {
                    let body = CreateConversation {
                        id: Some(cid.clone()),
                        title: Some(derive_title(&content)),
                    };
                    match create_conversation_with_retry(token.as_deref(), &body).await {
                        Ok(conv) => {
                            // Show the new thread in the sidebar immediately —
                            // it's newest, so it goes to the front of the
                            // (created_at DESC) list. Don't wait for the turn to
                            // finish streaming, and don't lose it if the socket
                            // open/send below fails (those early-return before the
                            // end-of-turn refresh). The reconciling `load_sessions`
                            // at the end replaces this with the server's view.
                            conversations.update(|list| list.insert(0, conv));

                            // Apply the pre-send picks now — before the turn
                            // opens — so the first turn already runs as the
                            // chosen profile / folder / model. Each value came
                            // from a server-provided picker, so a bind failure is
                            // unlikely; on one, surface it in that picker's error
                            // line and carry on rather than dropping the user's
                            // message. Mirror each server response into the list so
                            // the now-active pickers reflect the binding at once.
                            let prof = pending_profile.get_untracked();
                            if !prof.is_empty() {
                                match rest::set_conversation_profile(
                                    token.as_deref(),
                                    &cid,
                                    Some(&prof),
                                )
                                .await
                                {
                                    Ok(updated) => conversations.update(|list| {
                                        if let Some(c) =
                                            list.iter_mut().find(|c| c.id == updated.id)
                                        {
                                            c.agent_profile_id = updated.agent_profile_id.clone();
                                        }
                                    }),
                                    Err(e) => bind_error.set(Some(e.to_string())),
                                }
                            }
                            let mdl = pending_model.get_untracked();
                            if !mdl.is_empty() {
                                match rest::set_conversation_model(
                                    token.as_deref(),
                                    &cid,
                                    Some(&mdl),
                                )
                                .await
                                {
                                    Ok(updated) => conversations.update(|list| {
                                        if let Some(c) =
                                            list.iter_mut().find(|c| c.id == updated.id)
                                        {
                                            c.model = updated.model.clone();
                                        }
                                    }),
                                    Err(e) => model_error.set(Some(e.to_string())),
                                }
                            }
                            let reff = pending_reasoning.get_untracked();
                            if !reff.is_empty() {
                                match rest::set_conversation_reasoning(
                                    token.as_deref(),
                                    &cid,
                                    Some(&reff),
                                )
                                .await
                                {
                                    Ok(updated) => conversations.update(|list| {
                                        if let Some(c) =
                                            list.iter_mut().find(|c| c.id == updated.id)
                                        {
                                            c.reasoning_effort = updated.reasoning_effort.clone();
                                        }
                                    }),
                                    Err(e) => reasoning_error.set(Some(e.to_string())),
                                }
                            }
                            // The picks are live bindings now; drop the holders
                            // (reasoning back to its new-chat default).
                            pending_profile.set(String::new());
                            pending_model.set(String::new());
                            pending_reasoning.set(DEFAULT_REASONING_EFFORT.to_string());
                        }
                        Err(e) => {
                            mark_error(lines, assistant_id);
                            append_to(
                                assistant_id,
                                &format!("[could not start conversation: {e}]"),
                            );
                            finalize(assistant_id);
                            sending.set(false);
                            return;
                        }
                    }
                }

                drive_turn.run(DriveTurnArgs {
                    outbound: Outbound::Chat(outbound),
                    user_line_id,
                    assistant_id,
                });
            });
            true
        })
    };

    // The emerged-UI `ai` handler sink: `send_turn` minus its accepted flag (the
    // [`EmergedUi`] contract is fire-and-forget).
    let ai_sink: UnsyncCallback<String> = UnsyncCallback::new(move |content: String| {
        send_turn.run(content);
    });

    // Submit a decision on a guard-deferred tool call (SOUL §19): resolve the
    // durable approval + re-run (approve) or drop (reject) the held call, streaming
    // the response into a fresh assistant line. `(approval_id, approved)`.
    let submit_approval: UnsyncCallback<(String, bool)> = {
        UnsyncCallback::new(move |(approval_id, approved): (String, bool)| {
            if sending.get_untracked() {
                return;
            }
            // Dismiss the prompt optimistically; the server resolves it durably.
            pending_approval.set(None);
            let Some(conversation_id) = active_id.get_untracked() else {
                return;
            };
            let user_message_id = fresh_uuid();
            sending.set(true);
            // An assistant line to stream the resume response into (the deferred
            // call's result, or the model's adjustment after a reject). No user line —
            // the server records a synthetic "approved/rejected" message, shown on the
            // next reload.
            let assistant_id = push_line(Role::Assistant, String::new(), true);
            drive_turn.run(DriveTurnArgs {
                outbound: Outbound::Approval {
                    approval_id,
                    approved,
                    conversation_id,
                    user_message_id,
                },
                user_line_id: assistant_id,
                assistant_id,
            });
        })
    };

    // Regenerate from a user line: re-run the conversation from that message,
    // dropping its old answer (and anything after it) and streaming a fresh
    // response. Targets the line's server message id (set on replay, backfilled on
    // the live turn's `done`), so it is only ever invoked once that id is known.
    // An `UnsyncCallback<usize>` over the clicked line's render id.
    let regenerate: UnsyncCallback<usize> = {
        UnsyncCallback::new(move |from_line_id: usize| {
            if sending.get_untracked() {
                return;
            }
            let Some(cid) = active_id.get_untracked() else {
                return;
            };
            // The anchor's server id (required to re-run server-side) + its text
            // (advisory — the server re-answers the stored message). Absent id ⇒
            // not yet persisted, so there is nothing to regenerate from.
            let anchor = lines.with_untracked(|v| {
                v.iter()
                    .find(|l| l.id == from_line_id)
                    .and_then(|l| l.message_id.clone().map(|mid| (mid, l.text.clone())))
            });
            let Some((msg_id, text)) = anchor else {
                return;
            };
            sending.set(true);

            // Drop the old answer + anything after the anchor, then open a fresh
            // assistant line to stream the regenerated response into.
            lines.update(|v| {
                if let Some(pos) = v.iter().position(|l| l.id == from_line_id) {
                    v.truncate(pos + 1);
                }
            });
            let assistant_id = push_line(Role::Assistant, String::new(), true);

            drive_turn.run(DriveTurnArgs {
                outbound: Outbound::Chat(ClientChatMessage::regenerate(cid, text, msg_id)),
                user_line_id: from_line_id,
                assistant_id,
            });
        })
    };

    // Stage a picked file (SOUL §9/§12): upload its bytes to the user's default
    // files store under a `chat/<ts>-<rand>-<name>` key (no `?store=`, so the
    // server resolves the per-user default), then append a reference. Counted in
    // `uploads` so a send waits until every pick has landed.
    let add_attachment_file = move |file: web_sys::File| {
        uploads.update(|n| *n += 1);
        attach_error.set(None);
        spawn_local(async move {
            match read_file(file).await {
                Ok((name, ctype, bytes)) => {
                    let size = bytes.len() as u64;
                    let key = chat_upload_key(&name);
                    let token = auth::resolve_token();
                    let result =
                        rest::upload_object(token.as_deref(), &key, None, bytes, ctype.as_deref())
                            .await;
                    uploads.update(|n| *n = n.saturating_sub(1));
                    match result {
                        Ok(()) => attachments.update(|list| {
                            list.push(Attachment {
                                url: format!("/storage/objects/{key}"),
                                filename: (!name.is_empty()).then_some(name),
                                content_type: ctype,
                                size: Some(size),
                            });
                        }),
                        Err(e) => attach_error.set(Some(format!("upload failed: {e}"))),
                    }
                }
                Err(e) => {
                    uploads.update(|n| *n = n.saturating_sub(1));
                    attach_error.set(Some(e));
                }
            }
        });
    };

    // `<input type=file>` change: stage every picked file, then clear the input so
    // re-picking the same file re-fires.
    let on_attach_file_change = move |ev: leptos::ev::Event| {
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        if let Some(files) = input.files() {
            for i in 0..files.length() {
                if let Some(file) = files.get(i) {
                    add_attachment_file(file);
                }
            }
        }
        input.set_value("");
    };

    // Remove a staged attachment by index (before send).
    let remove_attachment = move |idx: usize| {
        attachments.update(|list| {
            if idx < list.len() {
                list.remove(idx);
            }
        });
    };

    // ---- Microphone dictation (speech-to-text, SOUL §7) ----
    // The live recording's JS handles live in single-threaded `Rc` cells (web_sys
    // objects are `!Send`, so a signal can't hold them): the `MediaRecorder`, its
    // mic `MediaStream` (tracks stopped on finish to release the mic), an optional
    // `AudioContext` + silence-poll `Interval` for the voice-activity auto-stop, the
    // collected blob chunks, and the two kept-alive recorder event `Closure`s.
    let media_recorder: Rc<RefCell<Option<web_sys::MediaRecorder>>> = Rc::new(RefCell::new(None));
    let media_stream: Rc<RefCell<Option<web_sys::MediaStream>>> = Rc::new(RefCell::new(None));
    let audio_ctx: Rc<RefCell<Option<web_sys::AudioContext>>> = Rc::new(RefCell::new(None));
    let vad_interval: Rc<RefCell<Option<Interval>>> = Rc::new(RefCell::new(None));
    let rec_chunks: Rc<RefCell<Vec<web_sys::Blob>>> = Rc::new(RefCell::new(Vec::new()));
    let ondata_hold: Rc<RefCell<Option<BlobEventClosure>>> = Rc::new(RefCell::new(None));
    let onstop_hold: Rc<RefCell<Option<StopClosure>>> = Rc::new(RefCell::new(None));
    // Where the *current* take's transcript goes (SOUL §7/§12) — fixed by
    // `start_recording`, read by `stop_recording` so only a composer take flips
    // the composer's `transcribing` spinner.
    let rec_dest: Rc<Cell<RecordDest>> = Rc::new(Cell::new(RecordDest::Composer));
    // Whether the current voice take's VAD actually heard speech — an
    // all-silence take skips the transcription POST and just re-listens.
    let voice_heard_flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    // The voice overlay's playback engine + the per-request correlation for
    // its speech channel (stale replies are discarded by id; the socket cell
    // itself is declared up beside the segmenter, in `drive_turn`'s reach).
    let playback = SpeechPlayback::default();
    let speech_ids = SpeechReqId::default();
    // Keeps the phone's screen from locking (which would kill the mic and the
    // playback) while the overlay is open: acquired on open, dropped on close.
    let wake_lock = voice::ScreenWakeLock::default();
    // The spoken-turn pump's control cells: `voice_gen` invalidates a
    // superseded pump (a new spoken turn, or the overlay closing), `voice_muted`
    // silences the rest of the current reply (orb-tap skip / pause), and
    // `pump_live` tells the turn-end effect whether a pump will hand the mic
    // back or it must do so itself.
    let voice_gen: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    let voice_muted: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let pump_live: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // Release the mic + tear down the analyser (idempotent): cancel the silence
    // poll, stop every track (drops the browser's recording indicator), and close
    // the `AudioContext`. Called from `onstop` once the blob has been handed off.
    let release_media: Rc<dyn Fn()> = {
        let media_stream = media_stream.clone();
        let audio_ctx = audio_ctx.clone();
        let vad_interval = vad_interval.clone();
        Rc::new(move || {
            vad_interval.borrow_mut().take(); // dropping the Interval cancels it
            if let Some(stream) = media_stream.borrow_mut().take() {
                stop_stream_tracks(&stream);
            }
            if let Some(ctx) = audio_ctx.borrow_mut().take() {
                let _ = ctx.close(); // returns a Promise; fire-and-forget
            }
        })
    };
    // Release the mic if the panel unmounts mid-recording (no-op otherwise). The
    // `!Send` `Rc` lives behind a LocalStorage `StoredValue` whose handle is the
    // `Send + Sync` value `on_cleanup` requires (same trick as the emerged-UI
    // observer).
    let cleanup_release = StoredValue::new_local(release_media.clone());
    on_cleanup(move || cleanup_release.with_value(|f| (**f)()));
    // Likewise tear down the voice overlay's playback + speech socket if the
    // panel unmounts mid-conversation; the generation bump invalidates a pump
    // still in flight before this panel's signals are disposed.
    let cleanup_voice = StoredValue::new_local((
        playback.clone(),
        speech_sock.clone(),
        voice_gen.clone(),
        wake_lock.clone(),
    ));
    on_cleanup(move || {
        cleanup_voice.with_value(|(pb, sock, gen, wl)| {
            gen.set(gen.get().wrapping_add(1));
            pb.shutdown();
            sock.borrow_mut().take();
            wl.release();
        });
    });

    // Drop the voice loop back to listening and ask for a mic re-arm (the bump
    // counter re-triggers the arming effect even if the state already reads
    // `Listening`). `Copy` — captures only signals.
    let arm_listen = move || {
        voice_state.set(VoiceState::Listening);
        voice_arm.update(|n| *n += 1);
    };

    // Start the speech pump for a freshly sent voice turn (SOUL §7/§12): a
    // task that eats paragraphs off the feed as the model streams them,
    // fetches each one's audio over `/ws/speech` **while the previous clip is
    // still playing** (the socket answers strictly in order — a one-clip
    // prefetch), and plays them back to back. It ends when the feed closes
    // (turn over, tail flushed by the effect below) or on error/supersession,
    // handing the mic back unless something else moved the loop on.
    let start_voice_pump: Rc<dyn Fn()> = {
        let playback = playback.clone();
        let speech_sock = speech_sock.clone();
        let speech_ids = speech_ids.clone();
        let voice_seg = voice_seg.clone();
        let voice_feed = voice_feed.clone();
        let voice_gen = voice_gen.clone();
        let voice_muted = voice_muted.clone();
        let pump_live = pump_live.clone();
        Rc::new(move || {
            // A new spoken turn supersedes whatever the previous one was doing.
            voice_gen.set(voice_gen.get().wrapping_add(1));
            let gen = voice_gen.get();
            voice_muted.set(false);
            voice_seg.borrow_mut().reset();
            voice::stop_speech(&playback);
            let (tx, rx) = unbounded::<String>();
            *voice_feed.borrow_mut() = Some(tx);
            pump_live.set(true);
            let playback = playback.clone();
            let speech_sock = speech_sock.clone();
            let speech_ids = speech_ids.clone();
            let voice_gen = voice_gen.clone();
            let voice_muted = voice_muted.clone();
            let pump_live = pump_live.clone();
            spawn_local(async move {
                let prepare_sock = speech_sock.clone();
                let prepare_ids = speech_ids.clone();
                let prepare_gen = voice_gen.clone();
                let prepare_muted = voice_muted.clone();
                let start_playback = playback.clone();
                let start_gen = voice_gen.clone();
                let start_muted = voice_muted.clone();
                voice::pump_one_ahead(
                    rx,
                    move |para| {
                        let speech_sock = prepare_sock.clone();
                        let speech_ids = prepare_ids.clone();
                        let voice_gen = prepare_gen.clone();
                        let voice_muted = prepare_muted.clone();
                        async move {
                            if voice_gen.get() != gen || voice_muted.get() {
                                return voice::PipelineStep::Stop;
                            }
                            let text = voice::speech_text(&para);
                            if text.is_empty() {
                                return voice::PipelineStep::Skip;
                            }
                            let fetched =
                                fetch_speech_bytes(&speech_sock, &speech_ids, voice_state, &text)
                                    .await;
                            if voice_gen.get() != gen || voice_muted.get() {
                                return voice::PipelineStep::Stop;
                            }
                            match fetched {
                                Ok(bytes) => voice::PipelineStep::Ready(bytes),
                                Err(e) => {
                                    voice_error.set(Some(e));
                                    voice::PipelineStep::Stop
                                }
                            }
                        }
                    },
                    move |bytes| {
                        let playback = start_playback.clone();
                        let voice_gen = start_gen.clone();
                        let voice_muted = start_muted.clone();
                        async move {
                            if voice_gen.get() != gen || voice_muted.get() {
                                return voice::PipelineStep::Stop;
                            }
                            if !matches!(
                                voice_state.get_untracked(),
                                VoiceState::Waiting | VoiceState::Speaking
                            ) {
                                return voice::PipelineStep::Stop;
                            }
                            if voice_state.get_untracked() != VoiceState::Speaking {
                                voice_state.set(VoiceState::Speaking);
                            }
                            let (etx, erx) = oneshot::channel::<()>();
                            let etx = RefCell::new(Some(etx));
                            let on_ended: Rc<dyn Fn()> = Rc::new(move || {
                                if let Some(s) = etx.borrow_mut().take() {
                                    let _ = s.send(());
                                }
                            });
                            match voice::play_speech(&playback, bytes, voice_level, on_ended).await
                            {
                                Ok(()) => voice::PipelineStep::Ready(erx),
                                // An undecodable clip skips itself; keep reading.
                                Err(e) => {
                                    voice_error.set(Some(e));
                                    voice::PipelineStep::Skip
                                }
                            }
                        }
                    },
                )
                .await;
                pump_live.set(false);
                // Hand the mic back — unless a newer pump owns the loop, the
                // user paused/closed, or a mid-stream mute leaves the turn-end
                // effect to do it once the model actually finishes.
                if voice_gen.get() == gen
                    && matches!(
                        voice_state.get_untracked(),
                        VoiceState::Waiting | VoiceState::Speaking
                    )
                    && (!voice_muted.get() || !sending.get_untracked())
                {
                    arm_listen();
                }
            });
        })
    };

    // Stop an in-progress recording: flip the UI to "transcribing" and ask the
    // recorder to stop, which fires `ondataavailable` then `onstop` (where the blob
    // is combined and posted). Idempotent — a second call (the button and the
    // silence auto-stop racing) is a no-op once `recording` is false. An
    // `UnsyncCallback` (not `Callback`) so the view can hold it despite the `!Send`
    // recorder `Rc` it captures.
    let stop_recording: UnsyncCallback<()> = {
        let media_recorder = media_recorder.clone();
        let rec_dest = rec_dest.clone();
        UnsyncCallback::new(move |(): ()| {
            if !recording.get_untracked() {
                return;
            }
            recording.set(false);
            if let Some(rec) = media_recorder.borrow().as_ref() {
                if matches!(rec.state(), web_sys::RecordingState::Recording) {
                    // The composer spinner is the composer's alone; a voice take
                    // shows its progress on the overlay (`VoiceState::Transcribing`).
                    if rec_dest.get() == RecordDest::Composer {
                        transcribing.set(true);
                    }
                    let _ = rec.stop();
                }
            }
        })
    };

    // Start recording: request the mic, wire the recorder's data/stop handlers (the
    // stop handler combines the chunks, POSTs them, and routes the transcript to
    // its destination — the composer draft, or straight out as a voice-overlay
    // chat turn), begin recording, and arm the voice-activity auto-stop.
    let start_recording: UnsyncCallback<RecordDest> = {
        let media_recorder = media_recorder.clone();
        let media_stream = media_stream.clone();
        let audio_ctx = audio_ctx.clone();
        let vad_interval = vad_interval.clone();
        let rec_chunks = rec_chunks.clone();
        let ondata_hold = ondata_hold.clone();
        let onstop_hold = onstop_hold.clone();
        let release_media = release_media.clone();
        let rec_dest = rec_dest.clone();
        let voice_heard_flag = voice_heard_flag.clone();
        let start_voice_pump = start_voice_pump.clone();
        UnsyncCallback::new(move |dest: RecordDest| {
            let media_recorder = media_recorder.clone();
            let media_stream = media_stream.clone();
            let audio_ctx = audio_ctx.clone();
            let vad_interval = vad_interval.clone();
            let rec_chunks = rec_chunks.clone();
            let ondata_hold = ondata_hold.clone();
            let onstop_hold = onstop_hold.clone();
            let release_media = release_media.clone();
            let rec_dest = rec_dest.clone();
            let voice_heard_flag = voice_heard_flag.clone();
            let start_voice_pump = start_voice_pump.clone();
            spawn_local(async move {
                rec_dest.set(dest);
                voice_heard_flag.set(false);
                // Failures land on the take's own surface: the composer's error
                // line, or the overlay — *parked*, because a hot re-listen loop
                // on a denied mic permission would spin forever.
                let fail = move |msg: &str| match dest {
                    RecordDest::Composer => stt_error.set(Some(msg.to_string())),
                    RecordDest::Voice => {
                        voice_error.set(Some(msg.to_string()));
                        voice_state.set(VoiceState::Paused);
                    }
                };
                stt_error.set(None);
                // 1. Capture the microphone.
                let Some(window) = web_sys::window() else {
                    return;
                };
                let devices = match window.navigator().media_devices() {
                    Ok(d) => d,
                    Err(_) => {
                        fail("this browser has no microphone access");
                        return;
                    }
                };
                let constraints = web_sys::MediaStreamConstraints::new();
                constraints.set_audio(&JsValue::TRUE);
                let stream = match devices.get_user_media_with_constraints(&constraints) {
                    Ok(promise) => match JsFuture::from(promise).await {
                        Ok(s) => s.unchecked_into::<web_sys::MediaStream>(),
                        Err(_) => {
                            fail("microphone permission denied");
                            return;
                        }
                    },
                    Err(_) => {
                        fail("could not access the microphone");
                        return;
                    }
                };
                // A voice take could have been cancelled (overlay closed) while
                // the permission prompt was up — release the mic and bow out.
                if dest == RecordDest::Voice && voice_state.get_untracked() != VoiceState::Listening
                {
                    stop_stream_tracks(&stream);
                    return;
                }
                // 2. Build the recorder.
                let recorder = match web_sys::MediaRecorder::new_with_media_stream(&stream) {
                    Ok(r) => r,
                    Err(_) => {
                        stop_stream_tracks(&stream);
                        fail("recording is unsupported in this browser");
                        return;
                    }
                };
                // 3. Collect the audio chunks as they arrive.
                rec_chunks.borrow_mut().clear();
                let ondata = {
                    let rec_chunks = rec_chunks.clone();
                    Closure::wrap(Box::new(move |ev: web_sys::BlobEvent| {
                        if let Some(blob) = ev.data() {
                            rec_chunks.borrow_mut().push(blob);
                        }
                    }) as Box<dyn FnMut(web_sys::BlobEvent)>)
                };
                recorder.set_ondataavailable(Some(ondata.as_ref().unchecked_ref()));
                *ondata_hold.borrow_mut() = Some(ondata);
                // 4. On stop: release the mic, combine the chunks, transcribe, and
                //    route the transcript — into the composer draft, or (voice
                //    overlay) straight out as a chat turn.
                let onstop = {
                    let rec_chunks = rec_chunks.clone();
                    let release_media = release_media.clone();
                    let recorder_for_mime = recorder.clone();
                    let voice_heard_flag = voice_heard_flag.clone();
                    let start_voice_pump = start_voice_pump.clone();
                    Closure::wrap(Box::new(move || {
                        (*release_media)();
                        let parts = js_sys::Array::new();
                        let mime = {
                            let chunks = rec_chunks.borrow();
                            for b in chunks.iter() {
                                parts.push(&JsValue::from(b.clone()));
                            }
                            chunks
                                .first()
                                .map(web_sys::Blob::type_)
                                .filter(|t| !t.is_empty())
                                .unwrap_or_else(|| recorder_for_mime.mime_type())
                        };
                        rec_chunks.borrow_mut().clear();
                        // A take error before the POST ends the composer spinner
                        // or drops the voice loop back to listening.
                        let bail = move || match dest {
                            RecordDest::Composer => transcribing.set(false),
                            RecordDest::Voice => arm_listen(),
                        };
                        if dest == RecordDest::Voice {
                            // The overlay stopped wanting this take (closed or
                            // paused mid-recording): discard it unheard — this
                            // doubles as the overlay's cancel path.
                            if voice_state.get_untracked() != VoiceState::Listening {
                                return;
                            }
                            // An all-silence take (the hard cap fired without any
                            // speech): skip the POST entirely, just listen again.
                            if !voice_heard_flag.get() {
                                arm_listen();
                                return;
                            }
                            voice_state.set(VoiceState::Transcribing);
                        }
                        let blob = match web_sys::Blob::new_with_blob_sequence(parts.as_ref()) {
                            Ok(b) => b,
                            Err(_) => {
                                bail();
                                return;
                            }
                        };
                        let start_voice_pump = start_voice_pump.clone();
                        spawn_local(async move {
                            let bytes = match JsFuture::from(blob.array_buffer()).await {
                                Ok(buf) => js_sys::Uint8Array::new(&buf).to_vec(),
                                Err(_) => {
                                    match dest {
                                        RecordDest::Composer => stt_error
                                            .set(Some("could not read the recording".into())),
                                        RecordDest::Voice => voice_error
                                            .set(Some("could not read the recording".into())),
                                    }
                                    bail();
                                    return;
                                }
                            };
                            if bytes.is_empty() {
                                bail();
                                return;
                            }
                            // Shorten the actual waveform before upload. Decoding
                            // is best-effort because some browsers can record a
                            // container their Web Audio decoder cannot read; in
                            // that case preserve the take and upload it unchanged.
                            let mut upload_bytes = bytes;
                            let mut upload_mime = mime;
                            let speed = voice_input_speed.get_untracked().clamp(1.0, 2.0);
                            if speed > 1.001 {
                                if let Ok(wav) = compress_recording(&upload_bytes, speed).await {
                                    upload_bytes = wav;
                                    upload_mime = "audio/wav".to_string();
                                }
                            }
                            let token = auth::resolve_token();
                            let ct = (!upload_mime.is_empty()).then_some(upload_mime.as_str());
                            let request_id = fresh_uuid();
                            match transcribe_with_retry(
                                token.as_deref(),
                                &request_id,
                                &upload_bytes,
                                ct,
                            )
                            .await
                            {
                                Ok(t) => {
                                    let text = t.text.trim().to_string();
                                    match dest {
                                        RecordDest::Composer => {
                                            if !text.is_empty() {
                                                draft.update(|d| {
                                                    if !d.is_empty()
                                                        && !d.ends_with(char::is_whitespace)
                                                    {
                                                        d.push(' ');
                                                    }
                                                    d.push_str(&text);
                                                });
                                            }
                                            transcribing.set(false);
                                        }
                                        RecordDest::Voice => {
                                            // Nothing intelligible → listen again.
                                            if text.is_empty() {
                                                arm_listen();
                                                return;
                                            }
                                            voice_heard.set(text.clone());
                                            voice_error.set(None);
                                            conversation_mode.set(true);
                                            if send_turn.run(text) {
                                                // Fresh feed + pump for this
                                                // turn's streamed paragraphs —
                                                // BEFORE any delta can arrive.
                                                // (`send_turn` set `sending`
                                                // synchronously, so Waiting
                                                // can't see a premature
                                                // turn-over.)
                                                (*start_voice_pump)();
                                                voice_state.set(VoiceState::Waiting);
                                            } else {
                                                arm_listen();
                                            }
                                        }
                                    }
                                }
                                Err(e) => match dest {
                                    RecordDest::Composer => {
                                        transcribing.set(false);
                                        stt_error.set(Some(format!("transcription failed: {e}")));
                                    }
                                    RecordDest::Voice => {
                                        voice_error.set(Some(format!("transcription failed: {e}")));
                                        arm_listen();
                                    }
                                },
                            }
                        });
                    }) as Box<dyn FnMut()>)
                };
                recorder.set_onstop(Some(onstop.as_ref().unchecked_ref()));
                *onstop_hold.borrow_mut() = Some(onstop);
                // 5. Begin recording (single blob flushed on stop).
                if recorder.start().is_err() {
                    stop_stream_tracks(&stream);
                    fail("could not start recording");
                    return;
                }
                *media_recorder.borrow_mut() = Some(recorder);
                *media_stream.borrow_mut() = Some(stream.clone());
                recording.set(true);
                // 6. Arm the voice-activity auto-stop (best-effort; the manual stop
                //    and a hard time cap back it up). It fires the same stop path as
                //    the button via the `Copy` `UnsyncCallback`. A voice take also
                //    taps the analyser for the overlay's orb level + heard flag.
                let stop_for_vad: Rc<dyn Fn()> = Rc::new(move || stop_recording.run(()));
                let (level_out, heard_out) = match dest {
                    RecordDest::Composer => (None, None),
                    RecordDest::Voice => (Some(voice_level), Some(voice_heard_flag.clone())),
                };
                spawn_vad(
                    &stream,
                    &audio_ctx,
                    &vad_interval,
                    stop_for_vad,
                    level_out,
                    heard_out,
                );
            });
        })
    };

    // ——— The voice-conversation loop (SOUL §7/§12) ———
    // listen → transcribe → send turn → speak the reply paragraph by paragraph
    // *while it streams* → listen again. The `!Send` handles ride a LocalStorage
    // `StoredValue` so the effects below stay spawnable (same trick as the
    // unmount cleanup above).
    let voice_rt = StoredValue::new_local((
        voice_seg.clone(),
        voice_feed.clone(),
        voice_muted.clone(),
        pump_live.clone(),
    ));

    // Re-arm the mic whenever the loop asks to listen. Keyed on the bump
    // counter — not the state signal — so consecutive "listen again"s (state
    // already `Listening`) all re-fire.
    Effect::new(move |_| {
        voice_arm.track();
        if voice_state.get_untracked() != VoiceState::Listening
            || recording.get_untracked()
            || transcribing.get_untracked()
        {
            return;
        }
        start_recording.run(RecordDest::Voice);
    });

    // Turn over while the voice loop is engaged (`sending` falls in Waiting or
    // Speaking): flush the segmenter's unfinished tail into the pump and close
    // the feed — the pump speaks the tail, then hands the mic back. If no pump
    // is running any more (a mid-stream skip muted the reply, or it errored
    // out and already re-armed), hand the mic back here so the loop can't
    // strand in Waiting.
    Effect::new(move |_| {
        if sending.get()
            || !matches!(
                voice_state.get(),
                VoiceState::Waiting | VoiceState::Speaking
            )
        {
            return;
        }
        let (seg, feed, muted, live) = voice_rt.with_value(Clone::clone);
        if let Some(tx) = feed.borrow_mut().take() {
            if !muted.get() {
                for rest in seg.borrow_mut().flush() {
                    let _ = tx.unbounded_send(rest);
                }
            }
            // Dropping `tx` ends the pump's feed; it finishes its clips first.
        }
        if !live.get() {
            arm_listen();
        }
    });

    // Open the overlay. The output `AudioContext` must be created + resumed
    // HERE, inside the click gesture — autoplay policy keeps a lazily created
    // context suspended and the first reply would play silence.
    let open_voice: UnsyncCallback<()> = {
        let playback = playback.clone();
        let wake_lock = wake_lock.clone();
        UnsyncCallback::new(move |(): ()| {
            if voice_state.get_untracked() != VoiceState::Off
                || recording.get_untracked()
                || transcribing.get_untracked()
            {
                return;
            }
            match web_sys::AudioContext::new() {
                Ok(ctx) => {
                    let _ = ctx.resume();
                    *playback.ctx.borrow_mut() = Some(ctx);
                }
                Err(_) => {
                    voice_error.set(Some("audio output is unavailable in this browser".into()));
                    return;
                }
            }
            // A hands-free conversation must survive the phone's idle timeout;
            // held for the whole overlay (pauses included), dropped on close.
            wake_lock.acquire();
            voice_error.set(None);
            voice_heard.set(String::new());
            voice_level.set(0.0);
            arm_listen();
        })
    };
    // Silence the rest of the current reply: mute the pump, drop the feed, and
    // stop whatever clip is sounding. Shared by skip / pause / close.
    let hush_reply = {
        let playback = playback.clone();
        let voice_feed = voice_feed.clone();
        let voice_seg = voice_seg.clone();
        let voice_muted = voice_muted.clone();
        move || {
            voice_muted.set(true);
            voice_feed.borrow_mut().take();
            voice_seg.borrow_mut().reset();
            voice::stop_speech(&playback);
        }
    };
    // Close it. `Off` is set FIRST: the recorder's `onstop` and the playback's
    // `onended` both check the state and discard/no-op instead of re-arming.
    let close_voice: UnsyncCallback<()> = {
        let playback = playback.clone();
        let speech_sock = speech_sock.clone();
        let voice_gen = voice_gen.clone();
        let hush_reply = hush_reply.clone();
        let wake_lock = wake_lock.clone();
        UnsyncCallback::new(move |(): ()| {
            voice_state.set(VoiceState::Off);
            // Invalidate the pump generation so an in-flight fetch can't act.
            voice_gen.set(voice_gen.get().wrapping_add(1));
            hush_reply();
            playback.shutdown();
            wake_lock.release();
            if recording.get_untracked() {
                stop_recording.run(());
            }
            speech_sock.borrow_mut().take();
            voice_level.set(0.0);
        })
    };
    // Pause/resume. Pausing mid-take discards the recording (the onstop cancel
    // path); pausing while the reply streams/plays silences the REST of it (a
    // resumed loop goes straight back to listening).
    let toggle_voice_pause: UnsyncCallback<()> = {
        let hush_reply = hush_reply.clone();
        UnsyncCallback::new(move |(): ()| match voice_state.get_untracked() {
            VoiceState::Off => {}
            VoiceState::Paused => arm_listen(),
            VoiceState::Speaking | VoiceState::Waiting => {
                voice_state.set(VoiceState::Paused);
                hush_reply();
            }
            VoiceState::Listening | VoiceState::Transcribing => {
                voice_state.set(VoiceState::Paused);
                if recording.get_untracked() {
                    stop_recording.run(());
                }
            }
        })
    };
    // Tap on the orb: skip the WHOLE rest of the reply — remaining paragraphs
    // included, not just the sounding clip. If the model is still streaming,
    // wait out the turn silently (the turn-end effect re-arms); else listen now.
    let voice_orb_tap: UnsyncCallback<()> = {
        let hush_reply = hush_reply.clone();
        UnsyncCallback::new(move |(): ()| {
            if voice_state.get_untracked() != VoiceState::Speaking {
                return;
            }
            hush_reply();
            if sending.get_untracked() {
                voice_state.set(VoiceState::Waiting);
            } else {
                arm_listen();
            }
        })
    };

    // The slash-command menu's rows: commands whose name starts with what's typed
    // after the leading `/` (case-insensitive). Non-empty only while the draft is
    // a single line still spelling a command name — args or a diverging spelling
    // empty it, which is what closes the menu. `/new` is built-in and listed
    // first; it shadows a same-named skill (`submit` resolves it first too).
    let slash_matches = Memo::new(move |_| {
        if slash_dismissed.get() {
            return Vec::new();
        }
        let d = draft.get();
        let Some(typed) = d.strip_prefix('/') else {
            return Vec::new();
        };
        if typed.contains('\n') {
            return Vec::new();
        }
        // During a Tab-completion session the draft shows a candidate, not what
        // the user typed — keep filtering by the remembered stem so the full
        // candidate list survives the cycling.
        let q = slash_stem
            .get()
            .unwrap_or_else(|| typed.to_string())
            .to_lowercase();
        let mut items = vec![("new".to_string(), "Start a new chat".to_string())];
        items.extend(skills.with(|list| {
            list.iter()
                .filter(|s| s.name != "new")
                .map(|s| (s.name.clone(), s.description.clone()))
                .collect::<Vec<_>>()
        }));
        items.retain(|(name, _)| name.to_lowercase().starts_with(&q));
        items
    });

    // Run a picked slash command: `/new` acts immediately (same as the sidebar
    // button); a skill inserts `/<name> ` so the user can append their request
    // before sending (the trailing space is what closes the menu).
    let apply_slash = move |name: String| {
        if name == "new" {
            new_chat();
        } else {
            draft.set(format!("/{name} "));
        }
        slash_idx.set(0);
        slash_stem.set(None);
    };

    // Send the input box's contents as a turn — or, while one is streaming,
    // queue it into the running turn (SOUL §12). The draft clears only when the
    // send was actually taken (`send_turn` declines during the stop window and
    // the brief pre-turn gaps), so nothing typed is ever dropped. A turn may be
    // attachments-only; never send while an upload is still in flight.
    //
    // A draft starting with `/` names a slash command: `/new [opener]` starts a
    // fresh chat (optionally sending `opener` as its first turn), and
    // `/<skill> [request]` sends the typed text as-is with the skill name riding
    // the frame — the server attaches the skill's runbook for the model, while
    // the UI (and transcript) show only the typed command (SOUL §12/§23).
    // Anything unrecognized sends as typed (a message may legitimately start
    // with a path like `/etc/hosts`).
    let submit = move || {
        let mut content = draft.get_untracked();
        match skills.with_untracked(|list| parse_slash_command(&content, list)) {
            Some((SlashCmd::New, opener)) => {
                new_chat();
                if opener.is_empty() {
                    return;
                }
                content = opener;
            }
            Some((SlashCmd::Skill(name), _args)) => {
                invoke_skill.set(Some(name));
            }
            None => {}
        }
        let has_atts = !attachments.with_untracked(Vec::is_empty);
        if (content.trim().is_empty() && !has_atts) || uploads.get_untracked() > 0 {
            return;
        }
        if send_turn.run(content) {
            draft.set(String::new());
        }
    };

    // The Stop button (SOUL §12): ask the server to cancel the streaming turn.
    // The control disables at once (`stopping`); everything resets when the
    // stopped turn's terminal frame lands. An `UnsyncCallback` so the view can
    // copy it freely.
    let stop_turn: UnsyncCallback<()> = {
        let turn_cmds = turn_cmds.clone();
        UnsyncCallback::new(move |(): ()| {
            if !sending.get_untracked() || stopping.get_untracked() {
                return;
            }
            if let Some(tx) = turn_cmds.borrow().clone() {
                stopping.set(true);
                let _ = tx.unbounded_send(MidTurnCmd::Stop);
            }
        })
    };

    // Enter submits; Shift+Enter inserts a newline (handled by the textarea).
    // `submit` is `Copy` (it captures only `Copy` handles), so each closure takes
    // its own copy. While the slash-command menu is showing, the keys drive it
    // instead: ↑/↓ move the highlight, Enter picks, Esc dismisses, and Tab runs
    // shell-style completion — the first Tab previews the highlighted command in
    // the draft, repeated Tab / Shift+Tab cycle the candidates, and stepping past
    // either end closes the menu and restores the typed stem. With the menu
    // closed (or on a Shift+Tab that has nothing to cycle back through) Tab keeps
    // its browser default, so the composer is never a keyboard trap.
    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        let menu = slash_matches.get_untracked();
        if !menu.is_empty() {
            let cur = slash_idx.get_untracked().min(menu.len() - 1);
            match ev.key().as_str() {
                "ArrowDown" => {
                    ev.prevent_default();
                    slash_idx.set((cur + 1).min(menu.len() - 1));
                    return;
                }
                "ArrowUp" => {
                    ev.prevent_default();
                    slash_idx.set(cur.saturating_sub(1));
                    return;
                }
                "Enter" if !ev.shift_key() => {
                    ev.prevent_default();
                    apply_slash(menu[cur].0.clone());
                    return;
                }
                "Tab" => {
                    let Some(stem) = slash_stem.get_untracked() else {
                        if ev.shift_key() {
                            // No session to cycle back through — default Tab.
                            return;
                        }
                        // Start a session: remember the typed stem, preview the
                        // highlighted command.
                        ev.prevent_default();
                        let typed = draft.with_untracked(|d| {
                            d.strip_prefix('/').unwrap_or_default().to_string()
                        });
                        slash_stem.set(Some(typed));
                        draft.set(format!("/{}", menu[cur].0));
                        return;
                    };
                    ev.prevent_default();
                    let next = if ev.shift_key() {
                        cur.checked_sub(1)
                    } else {
                        (cur + 1 < menu.len()).then_some(cur + 1)
                    };
                    match next {
                        Some(i) => {
                            slash_idx.set(i);
                            draft.set(format!("/{}", menu[i].0));
                        }
                        None => {
                            // Stepped past the end: give the typed stem back and
                            // close (a further Tab then does its default thing).
                            draft.set(format!("/{stem}"));
                            slash_stem.set(None);
                            slash_idx.set(0);
                            slash_dismissed.set(true);
                        }
                    }
                    return;
                }
                "Escape" => {
                    ev.prevent_default();
                    if let Some(stem) = slash_stem.get_untracked() {
                        draft.set(format!("/{stem}"));
                        slash_stem.set(None);
                    }
                    slash_dismissed.set(true);
                    return;
                }
                _ => {}
            }
        }
        if ev.key() == "Enter" && !ev.shift_key() {
            ev.prevent_default();
            submit();
        }
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        submit();
    };

    // Populate the sidebar + the profile picker on mount.
    load_sessions();
    load_profiles();
    load_models();
    load_builtin_tools();
    load_skills();

    // A pending outbox may belong to a client-minted conversation whose original
    // create response was lost. Ensure it exists idempotently before replaying.
    let restore_session = UnsyncCallback::new(move |id: String| {
        let pending = read_chat_outbox()
            .into_iter()
            .find(|m| m.conversation_id == id);
        if let Some(message) = pending {
            active_id.set(Some(id.clone()));
            spawn_local(async move {
                let token = auth::resolve_token();
                let body = CreateConversation {
                    id: Some(id.clone()),
                    title: Some(derive_title(&message.content)),
                };
                match create_conversation_with_retry(token.as_deref(), &body).await {
                    Ok(_) => open_session(id),
                    Err(e) => {
                        sessions_error.set(Some(format!("Could not restore pending chat: {e}")))
                    }
                }
            });
        } else {
            open_session(id);
        }
    });

    // A mobile connection returning also retries an idle/load failure. A live
    // turn already has its own retry loop and is left alone here.
    let online_callback = StoredValue::new_local(Option::<Closure<dyn FnMut()>>::None);
    if let Some(window) = web_sys::window() {
        let cb = Closure::wrap(Box::new(move || {
            if !sending.get_untracked() {
                if let Some(id) = active_id.get_untracked() {
                    restore_session.run(id);
                }
            }
        }) as Box<dyn FnMut()>);
        let _ = window.add_event_listener_with_callback("online", cb.as_ref().unchecked_ref());
        online_callback.set_value(Some(cb));
    }
    on_cleanup(move || {
        online_callback.with_value(|callback| {
            if let (Some(window), Some(callback)) = (web_sys::window(), callback.as_ref()) {
                let _ = window.remove_event_listener_with_callback(
                    "online",
                    callback.as_ref().unchecked_ref(),
                );
            }
        });
    });

    // On mount, open the requested/deep-linked thread. A bare `/app/chat` leaves
    // a fresh, unsaved chat.
    if let Some(id) = resume.get_untracked() {
        resume.set(None);
        restore_session.run(id);
    } else if let Some(id) = session_from_location() {
        restore_session.run(id);
    }

    // Mirror the open conversation into the URL as `/app/chat/<id>` so a thread
    // is deep-linkable and survives reload. Declared after the mount logic above
    // so `active_id` is already settled when this effect first runs (a matching
    // URL is then a no-op); thereafter it tracks every open/new/switch through
    // the single `active_id` signal. See `sync_location_to_session`.
    Effect::new(move |_| {
        active_id.with(|id| sync_location_to_session(id.as_deref()));
    });

    // Keep streamed generation pinned to the newest content when the reader was
    // already at the bottom. A manual upward scroll flips `follow_chat_bottom`
    // off, so long replies never pull someone away from text they are reading.
    Effect::new(move |_| {
        lines.track();
        if !follow_chat_bottom.get_untracked() || chat_scroll_frame_pending.get_value() {
            return;
        }
        chat_scroll_frame_pending.set_value(true);
        request_animation_frame(move || {
            chat_scroll_frame_pending.set_value(false);
            if !follow_chat_bottom.get_untracked() {
                return;
            }
            if let Some(el) = chat_log.get_untracked() {
                let el: web_sys::Element = el.unchecked_into();
                el.set_scroll_top(el.scroll_height());
            }
        });
    });

    view! {
        <section class="chat-layout">
            <button
                class="chat-sidebar-scrim"
                class:chat-sidebar-scrim-open=move || sessions_open.get()
                aria-label="Close chats"
                tabindex="-1"
                on:click=move |_| sessions_open.set(false)
            ></button>
            <aside class="pane-list chat-sidebar" class:chat-sidebar-open=move || sessions_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Chats"</h2>
                    <button
                        class="pane-btn pane-btn-primary"
                        on:click=move |_| {
                            new_chat();
                            sessions_open.set(false);
                        }
                    >
                        "+ New"
                    </button>
                </header>

                <div class="pane-list-body chat-sidebar-body">
                    <Show
                        when=move || !conversations.with(Vec::is_empty)
                        fallback=|| ().into_view()
                    >
                        <input
                            class="chat-search"
                            placeholder="Search chats…"
                            prop:value=move || session_query.get()
                            on:input=move |ev| session_query.set(event_target_value(&ev))
                        />
                    </Show>

                    <Show when=move || renaming.get().is_some() fallback=|| ().into_view()>
                        <div class="chat-rename-form">
                            <input
                                class="chat-rename-input"
                                placeholder="Conversation title…"
                                prop:value=move || rename_title.get()
                                on:input=move |ev| rename_title.set(event_target_value(&ev))
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                    if ev.key() == "Enter" {
                                        ev.prevent_default();
                                        submit_rename();
                                    } else if ev.key() == "Escape" {
                                        renaming.set(None);
                                    }
                                }
                            />
                            <button class="chat-rename-btn" on:click=move |_| submit_rename()>
                                "Save"
                            </button>
                            <button class="chat-rename-btn" on:click=move |_| renaming.set(None)>
                                "Cancel"
                            </button>
                        </div>
                    </Show>

                    <Show when=move || sessions_loading.get() fallback=|| ().into_view()>
                        <div class="pane-list-status">"Loading…"</div>
                    </Show>

                    <Show
                        when=move || sessions_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status pane-list-error">
                            {move || {
                                format!(
                                    "Could not load chats: {}",
                                    sessions_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !sessions_loading.get()
                                && sessions_error.with(Option::is_none)
                                && conversations.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No conversations yet."</div>
                    </Show>

                    <Show
                        when=move || {
                            !conversations.with(Vec::is_empty)
                                && session_query.with(|q| !q.trim().is_empty())
                                && conversations
                                    .with(|cs| session_query.with(|q| filter_conversations(cs, q).is_empty()))
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No chats match."</div>
                    </Show>

                    <For
                        each=move || {
                            let filtered = conversations
                                .with(|cs| session_query.with(|q| filter_conversations(cs, q)));
                            group_sessions(&filtered, now_ms(), local_tz_offset_ms())
                        }
                        key=group_key
                        children=move |g: SessionGroup| {
                            let label = g.label.clone();
                            let items = g.items;
                            view! {
                                <div class="chat-session-group">
                                    <div class="chat-group-label">{label}</div>
                                    <ul class="chat-session-list">
                                        <For
                                            each=move || items.clone()
                                            key=|c| c.id.clone()
                                            children=move |c: Conversation| {
                                                let id = c.id.clone();
                                                let is_active = {
                                                    let id = id.clone();
                                                    move || {
                                                        active_id.get().as_deref() == Some(id.as_str())
                                                    }
                                                };
                                                let class = move || {
                                                    if is_active() {
                                                        "chat-session chat-session-active"
                                                    } else {
                                                        "chat-session"
                                                    }
                                                };
                                                let title = match &c.title {
                                                    Some(t) if !t.trim().is_empty() => t.clone(),
                                                    _ => "(untitled)".to_string(),
                                                };
                                                let tooltip = title.clone();
                                                let tags = c.tags.clone();
                                                // A real link to the thread's deep-link URL so
                                                // middle-click / ctrl-click open it in a new tab;
                                                // a plain left-click stays in-app (SPA replay).
                                                let href = format!("{CHAT_ROUTE}/{id}");
                                                let id_click = id.clone();
                                                let id_del = id.clone();
                                                let id_ren = id.clone();
                                                let title_ren = title.clone();
                                                view! {
                                                    <li class="chat-session-row">
                                                        <a
                                                            class=class
                                                            title=tooltip
                                                            href=href
                                                            on:click=move |ev: leptos::ev::MouseEvent| {
                                                                if ev.ctrl_key() || ev.meta_key()
                                                                    || ev.shift_key() || ev.alt_key()
                                                                    || ev.button() != 0
                                                                {
                                                                    return;
                                                                }
                                                                ev.prevent_default();
                                                                open_session(id_click.clone());
                                                                sessions_open.set(false);
                                                            }
                                                        >
                                                            {title}
                                                        </a>
                                                        {(!tags.is_empty()).then(|| view! {
                                                            <span class="chat-session-tags">
                                                                {tags
                                                                    .iter()
                                                                    .map(|t| {
                                                                        let t = t.clone();
                                                                        view! { <span class="chat-session-tag">{t}</span> }
                                                                    })
                                                                    .collect::<Vec<_>>()}
                                                            </span>
                                                        })}
                                                        <div class="row-acts row-acts-reveal chat-session-acts">
                                                            {row_action(
                                                                MdIcon::Edit,
                                                                "Rename conversation",
                                                                false,
                                                                move || {
                                                                    rename_title.set(title_ren.clone());
                                                                    renaming.set(Some(id_ren.clone()));
                                                                },
                                                            )}
                                                            {row_action(
                                                                MdIcon::Delete,
                                                                "Delete conversation",
                                                                true,
                                                                move || delete_session(id_del.clone()),
                                                            )}
                                                        </div>
                                                    </li>
                                                }
                                            }
                                        />
                                    </ul>
                                </div>
                            }
                        }
                    />
                </div>
            </aside>

            <section class="chat-panel">
                <div class="chat-toolbar">
                    <button
                        class="chat-panel-toggle chat-sessions-toggle"
                        class:chat-panel-toggle-on=move || sessions_open.get()
                        title="Conversations"
                        on:click=move |_| sessions_open.update(|s| *s = !*s)
                    >
                        <Icon icon=MdIcon::Menu />
                        <span>"Chats"</span>
                    </button>
                    <Show
                        when=move || !profiles.get().is_empty()
                        fallback=|| ().into_view()
                    >
                        <label class="chat-toolbar-profile">
                            <span class="chat-toolbar-profile-label">"Profile"</span>
                            <select
                                class="chat-toolbar-profile-select"
                                aria-label="Chat profile"
                                title="Run this chat with a profile"
                                prop:value=current_profile
                                disabled=move || binding_profile.get()
                                on:change=move |ev| set_profile(event_target_value(&ev))
                            >
                                <option value="">"No profile"</option>
                                <For
                                    each=move || profiles.get()
                                    key=|p| p.id.clone()
                                    children=move |p: AgentProfile| {
                                        view! {
                                            <option value=p.id.clone()>
                                                {p.name.clone()}
                                            </option>
                                        }
                                    }
                                />
                            </select>
                        </label>
                    </Show>
                    <button
                        class="chat-panel-toggle"
                        class:chat-panel-toggle-on=move || sidebar_open.get()
                        title="Terminal output & conversation settings"
                        on:click=move |_| sidebar_open.update(|s| *s = !*s)
                    >
                        {move || if sidebar_open.get() { "Panel ›" } else { "‹ Panel" }}
                    </button>
                </div>
                <div
                    node_ref=chat_log
                    class="chat-log"
                    on:scroll=move |_| {
                        if let Some(el) = chat_log.get_untracked() {
                            let el: web_sys::Element = el.unchecked_into();
                            follow_chat_bottom.set(chat_is_at_bottom(
                                el.scroll_top(),
                                el.scroll_height(),
                                el.client_height(),
                            ));
                        }
                    }
                >
                    <Show
                        when=move || lines.with(|v| v.is_empty())
                        fallback=|| ().into_view()
                    >
                        <div class="chat-empty">
                            <p>"Start a conversation — type a message below."</p>
                            <p class="chat-empty-disclaimer">
                                <Icon icon=MdIcon::Warning />
                                <span>"AI can make mistakes. Check important output before relying on it."</span>
                            </p>
                        </div>
                    </Show>
                    <For
                        each=move || lines.get()
                        key=|line| line.id
                        children=move |line: ChatLine| {
                            // A keyed <For> creates this child once per line id and
                            // never re-runs it when the underlying `ChatLine` mutates
                            // in place. An assistant line is inserted empty + streaming
                            // and then grows (text/reasoning deltas), flips `streaming`
                            // off and gains a cost on `done`, and flips `role` to Error
                            // on failure — so every mutable field must be read
                            // *reactively* from `lines` (keyed by id), not captured
                            // once, or the bubble freezes at its initial empty/streaming
                            // state (i.e. live streaming silently "not working"). The
                            // inline-UI mount below already used this Memo pattern; now
                            // the text/reasoning/cost/role do too.
                            let line_id = line.id;
                            let row = Memo::new(move |_| {
                                lines.with(|v| v.iter().find(|l| l.id == line_id).cloned())
                            });
                            let row_role =
                                move || row.with(|l| l.as_ref().map_or(Role::Assistant, |l| l.role));
                            // One incremental Markdown renderer per row (created once by the
                            // keyed <For>). Each streaming delta commits only the newly-stable
                            // blocks — already-emitted HTML is never re-rendered or mutated, so
                            // the bubble grows append-only with no reflow/flicker, and the work
                            // per delta is O(new text) not O(whole reply).
                            let stream = StoredValue::new(catalerum_markdown::StreamRenderer::new());
                            let row_queued =
                                move || row.with(|l| l.as_ref().is_some_and(|l| l.queued));
                            view! {
                                <div
                                    class=move || row_role().css()
                                    class:msg-queued=row_queued
                                >
                                    <span class="msg-role">
                                        {move || row_role().label()}
                                        {move || {
                                            row_queued()
                                                .then(|| {
                                                    view! {
                                                        <span
                                                            class="msg-queued-tag"
                                                            title="Sent while the assistant was working — it joins the conversation at the next step"
                                                        >
                                                            "queued"
                                                        </span>
                                                    }
                                                })
                                        }}
                                    </span>
                                    {move || {
                                        // Uploaded files ride on **top** of the bubble —
                                        // above the text — as thumbnails (images) or
                                        // labelled download chips (everything else), each
                                        // resolved through the shared XSS-safe href helper
                                        // so an upload carries its auth token and a pasted
                                        // link can't smuggle a script scheme. Read
                                        // reactively so a live user line gains its chips
                                        // the moment it's pushed.
                                        let atts = row.with(|l| {
                                            l.as_ref().map(|l| l.attachments.clone()).unwrap_or_default()
                                        });
                                        (!atts.is_empty()).then(|| {
                                            let chips: Vec<AnyView> =
                                                atts.iter().map(message_attachment_view).collect();
                                            view! { <div class="msg-attachments">{chips}</div> }
                                        })
                                    }}
                                    <Show
                                        when=move || {
                                            row.with(|l| {
                                                l.as_ref().is_some_and(|l| !l.reasoning.is_empty())
                                            })
                                        }
                                        fallback=|| ().into_view()
                                    >
                                        <details class="msg-thinking">
                                            <summary>"Thinking"</summary>
                                            <span class="msg-thinking-text">
                                                {move || {
                                                    row.with(|l| {
                                                        l.as_ref()
                                                            .map(|l| l.reasoning.clone())
                                                            .unwrap_or_default()
                                                    })
                                                }}
                                            </span>
                                        </details>
                                    </Show>
                                    {move || {
                                        // LLM output is Markdown. A finalized assistant reply
                                        // renders fully. *While streaming*, the shared parser's
                                        // `stable_boundary` splits the text: the prefix that can
                                        // no longer change is rendered as Markdown live, and the
                                        // still-open tail shows as plain text (+cursor) so a
                                        // half-written `**bold` never flashes as broken HTML.
                                        // User/error lines are always plain text. Re-runs on
                                        // each delta via the row Memo.
                                        //
                                        // This prose renders *above* the tool-call cards below:
                                        // within a streamed round the model emits its text before
                                        // dispatching tools (see `split_after_tools`), so the
                                        // round's narration ("I'll open a terminal…") must precede
                                        // the cards for the calls it then made.
                                        let (role, text, streaming, show_text) = row.with(|l| {
                                            l.as_ref().map_or(
                                                (Role::Assistant, String::new(), false, false),
                                                |l| {
                                                    (
                                                        l.role,
                                                        l.text.clone(),
                                                        l.streaming,
                                                        should_render_message_text(
                                                            &l.text,
                                                            l.role == Role::Assistant && l.streaming,
                                                            !l.tool_calls.is_empty() || l.ui_id.is_some(),
                                                        ),
                                                    )
                                                },
                                            )
                                        });
                                        if !show_text {
                                            ().into_any()
                                        } else if role == Role::Assistant && !streaming {
                                            let rendered = markdown_html(&text);
                                            view! {
                                                <div
                                                    class="msg-text msg-markdown"
                                                    inner_html=rendered
                                                ></div>
                                            }
                                            .into_any()
                                        } else if role == Role::Assistant {
                                            // Commit any newly-stable blocks into the per-row
                                            // renderer (append-only) and show the still-open tail
                                            // as plain text so a half-written `**bold` never
                                            // flashes as broken HTML.
                                            let (committed, tail) = stream
                                                .try_update_value(|r| {
                                                    let tail = r.update(&text).to_string();
                                                    (r.html().to_string(), tail)
                                                })
                                                .unwrap_or_default();
                                            view! {
                                                <div class="msg-text msg-markdown msg-streaming">
                                                    <div inner_html=committed></div>
                                                    <span class="msg-stream-tail">
                                                        {tail}
                                                        <span class="msg-cursor">"▍"</span>
                                                    </span>
                                                </div>
                                            }
                                            .into_any()
                                        } else {
                                            view! {
                                                <span class="msg-text">
                                                    {text}
                                                    {streaming
                                                        .then(|| {
                                                            view! {
                                                                <span class="msg-cursor">"▍"</span>
                                                            }
                                                        })}
                                                </span>
                                            }
                                            .into_any()
                                        }
                                    }}
                                    {move || {
                                        // Tool-call cards: one collapsible <details> per
                                        // call this turn made, read through the per-line
                                        // `row` Memo so a live (spinner) card flips to
                                        // ✓/✗ in place. Rendered *below* the prose above —
                                        // the model narrates a round, then dispatches its
                                        // tools. App-authoring tools are not here — they
                                        // mount inline as the UI below.
                                        let tools = row.with(|l| {
                                            l.as_ref().map(|l| l.tool_calls.clone()).unwrap_or_default()
                                        });
                                        (!tools.is_empty()).then(|| {
                                            let cards: Vec<AnyView> = tools
                                                .into_iter()
                                                .map(|tc| {
                                                    let detail = tool_summary(
                                                        &tc.name,
                                                        &tc.arguments,
                                                        tc.result.as_deref(),
                                                    );
                                                    let body = render_tool_body(
                                                        &tc.name,
                                                        &tc.arguments,
                                                        tc.result.as_deref(),
                                                        tc.status == ToolStatus::Err,
                                                    );
                                                    let dur = tc.duration_ms.map(fmt_duration);
                                                    let running = tc.status == ToolStatus::Running;
                                                    let failed = tc.status == ToolStatus::Err;
                                                    let truncated = tc.truncated;
                                                    // External (MCP) iff the catalog is
                                                    // loaded and the name isn't a built-in.
                                                    let external = builtin_tools
                                                        .with(|s| !s.is_empty() && !s.contains(&tc.name));
                                                    let glyph = if running {
                                                        view! { <span class="msg-tool-spinner"></span> }
                                                            .into_any()
                                                    } else {
                                                        view! {
                                                            <span class="msg-tool-glyph">
                                                                {tool_status_glyph(tc.status)}
                                                            </span>
                                                        }
                                                        .into_any()
                                                    };
                                                    view! {
                                                        <details
                                                            class="msg-tool"
                                                            class:msg-tool-running=running
                                                            class:msg-tool-failed=failed
                                                        >
                                                            <summary class="msg-tool-summary">
                                                                {glyph}
                                                                <span class="msg-tool-name">
                                                                    {tc.name.clone()}
                                                                </span>
                                                                {external.then(|| view! {
                                                                    <span
                                                                        class="msg-tool-src"
                                                                        title="External tool provided by an MCP server"
                                                                    >"MCP"</span>
                                                                })}
                                                                {detail.map(|d| view! {
                                                                    <span class="msg-tool-detail">{d}</span>
                                                                })}
                                                                {dur.map(|d| view! {
                                                                    <span class="msg-tool-dur">{d}</span>
                                                                })}
                                                                {truncated.then(|| view! {
                                                                    <span
                                                                        class="msg-tool-trunc"
                                                                        title="Result truncated for streaming — reopen the conversation for the full output"
                                                                    >"truncated"</span>
                                                                })}
                                                            </summary>
                                                            {body}
                                                        </details>
                                                    }
                                                    .into_any()
                                                })
                                                .collect();
                                            view! { <div class="msg-tools">{cards}</div> }
                                        })
                                    }}
                                    {move || {
                                        // Per-message action row (bottom-right, hover-revealed):
                                        // copy the message text, and — only on a user line whose
                                        // server message id is known — regenerate the conversation
                                        // from it (dropping the old answer and anything after;
                                        // disabled while a turn is in flight).
                                        let has_text =
                                            row.with(|l| l.as_ref().is_some_and(|l| {
                                                !l.text.trim().is_empty()
                                            }));
                                        let regenerable = row.with(|l| {
                                            l.as_ref()
                                                .filter(|l| l.role == Role::User)
                                                .and_then(|l| l.message_id.clone())
                                                .is_some()
                                        });
                                        (has_text || regenerable).then(|| {
                                            view! {
                                                <div class="msg-acts">
                                                    {has_text.then(|| copy_button(
                                                        move || row.with(|l| {
                                                            l.as_ref()
                                                                .map(|l| l.text.clone())
                                                                .unwrap_or_default()
                                                        }),
                                                        "⧉ Copy",
                                                        "✓ Copied",
                                                        "msg-act",
                                                    ))}
                                                    {regenerable.then(|| view! {
                                                        <button
                                                            class="msg-act msg-regen"
                                                            title="Regenerate the response from this message"
                                                            disabled=move || sending.get()
                                                            on:click=move |_| regenerate.run(line_id)
                                                        >
                                                            <Icon icon=MdIcon::Refresh />
                                                            <span>"Regenerate"</span>
                                                        </button>
                                                    })}
                                                </div>
                                            }
                                        })
                                    }}
                                    {move || {
                                        row.with(|l| l.as_ref().and_then(|l| l.cost_usd)).map(|c| {
                                            view! {
                                                <span
                                                    class="msg-cost"
                                                    title="LLM cost for this turn"
                                                >
                                                    {fmt_cost(c)}
                                                </span>
                                            }
                                        })
                                    }}
                                    {move || {
                                        // A muted info-icon under the bubble whose hover
                                        // title spells out this turn's token + cache
                                        // usage plus the conversation's running total up
                                        // to and including this line. Present whenever
                                        // usage was reported — live or rehydrated from
                                        // the persisted assistant message on replay.
                                        row.with(|l| l.as_ref().and_then(|l| l.tokens)).map(|t| {
                                            // Running total of `total_tokens` over every
                                            // line up to and including this one. Reads
                                            // `lines` so it recomputes as earlier turns
                                            // land their usage.
                                            let cumulative = lines.with(|v| {
                                                let mut acc = 0u32;
                                                for l in v {
                                                    if let Some(u) = l.tokens {
                                                        acc = acc.saturating_add(u.total_tokens);
                                                    }
                                                    if l.id == line_id {
                                                        break;
                                                    }
                                                }
                                                acc
                                            });
                                            let tip = fmt_token_tooltip(&t, cumulative);
                                            view! {
                                                <span class="msg-tokens" title=tip><Icon icon=MdIcon::Info /></span>
                                            }
                                        })
                                    }}
                                    {
                                        // Mount an emerged UI inline when this line
                                        // carries one. A keyed <For> never re-runs a
                                        // same-key child, so read (ui_id, version)
                                        // through a Memo that fires when *this* line's
                                        // UI is first set OR its version bumps — so a
                                        // re-present/edit of the same id remounts (and
                                        // EmergedUi re-fetches the fresh definition).
                                        let ui = Memo::new(move |_| {
                                            lines.with(|v| {
                                                v.iter().find(|l| l.id == line_id).and_then(|l| {
                                                    l.ui_id
                                                        .clone()
                                                        .map(|id| (id, l.ui_version.unwrap_or(0)))
                                                })
                                            })
                                        });
                                        move || {
                                            ui.get().map(|(uid, _ver)| {
                                                view! { <EmergedUi ui_id=uid ai_sink=ai_sink /> }
                                            })
                                        }
                                    }
                                </div>
                            }
                        }
                    />
                </div>

                // Tool-guard approval prompt (SOUL §19): shown while a guarded tool
                // call is paused awaiting the user's decision. Approve resumes the
                // call; reject denies it (the model sees a policy error).
                <Show
                    when=move || pending_approval.get().is_some()
                    fallback=|| ().into_view()
                >
    {
                        move || {
                            pending_approval.get().map(|pa| {
                                let id = pa.id.clone();
                                // Approve/Reject resolves the durable approval and re-runs
                                // (or drops) the held call as a fresh turn (SOUL §19) — works
                                // even after a reload / reconnect / restart re-fetched it.
                                let reply = move |approved: bool| {
                                    submit_approval.run((id.clone(), approved));
                                };
                                let approve = { let reply = reply.clone(); move |_| reply(true) };
                                let reject = move |_| reply(false);
                                view! {
                                    <div class="chat-approval">
                                        <div class="chat-approval-head">
                                            <span class="chat-approval-title">"Approve tool call?"</span>
                                            <code class="chat-approval-tool">{pa.tool.clone()}</code>
                                        </div>
                                        <div class="chat-approval-reason">{pa.reason.clone()}</div>
                                        <Show
                                            when={let a = pa.arguments.clone(); move || !a.is_empty()}
                                            fallback=|| ().into_view()
                                        >
                                            <pre class="chat-approval-args">{pa.arguments.clone()}</pre>
                                        </Show>
                                        <div class="chat-approval-actions">
                                            <button
                                                type="button"
                                                class="chat-approval-approve"
                                                on:click=approve
                                            >
                                                "Approve"
                                            </button>
                                            <button
                                                type="button"
                                                class="chat-approval-reject"
                                                on:click=reject
                                            >
                                                "Reject"
                                            </button>
                                        </div>
                                    </div>
                                }
                            })
                        }
                    }
                </Show>

                // `ask_user` question form (SOUL §7/§12): shown while the tool is
                // paused awaiting the user's answers. Submitting resumes the turn.
                <Show
                    when=move || pending_questions.get().is_some()
                    fallback=|| ().into_view()
                >
                    {
                        move || {
                            pending_questions.get().map(|questions| {
                                let form_questions = questions.clone();
                                // Submitting formats the answers into a readable message
                                // and sends it as an ordinary turn via `send_turn` (which
                                // clears the form). The structured answers ride the same
                                // frame (the `form_answers` handoff) so the server stamps
                                // them onto the question row it resolves. The assistant
                                // continues — durable, so this works even if the form
                                // outlived a reload/reconnect.
                                let on_submit = UnsyncCallback::new(move |answers: Vec<Answer>| {
                                    let message =
                                        format_answers_message(&form_questions, &answers);
                                    form_answers.set(Some(answers));
                                    send_turn.run(message);
                                });
                                view! {
                                    <div class="chat-questions-wrap">
                                        <div class="chat-questions-head">
                                            "The assistant is asking:"
                                        </div>
                                        <QuestionForm questions=questions.clone() on_submit=on_submit />
                                    </div>
                                }
                            })
                        }
                    }
                </Show>

                <form class="chat-input" on:submit=on_submit>
                    // Staged attachment chips (uploaded; sent as references, SOUL §9).
                    <Show
                        when=move || !attachments.with(Vec::is_empty)
                        fallback=|| ().into_view()
                    >
                        <div class="chat-attachments">
                            <For
                                each=move || {
                                    attachments.get().into_iter().enumerate().collect::<Vec<_>>()
                                }
                                key=|(i, a)| (*i, a.url.clone())
                                children=move |(idx, a)| {
                                    let label = a
                                        .filename
                                        .clone()
                                        .unwrap_or_else(|| "file".to_string());
                                    view! {
                                        <span class="chat-attachment-chip">
                                            <span class="chat-attachment-name">{label}</span>
                                            <button
                                                type="button"
                                                class="chat-attachment-remove"
                                                title="Remove"
                                                on:click=move |_| remove_attachment(idx)
                                            >
                                                <Icon icon=MdIcon::Close />
                                            </button>
                                        </span>
                                    }
                                }
                            />
                        </div>
                    </Show>
                    <Show when=move || { uploads.get() > 0 } fallback=|| ().into_view()>
                        <div class="chat-attach-status">"Uploading…"</div>
                    </Show>
                    <Show
                        when=move || attach_error.get().is_some()
                        fallback=|| ().into_view()
                    >
                        <div class="chat-attach-error">
                            {move || attach_error.get().unwrap_or_default()}
                        </div>
                    </Show>
                    // Mic dictation status (SOUL §7): recording / transcribing hints
                    // and a permission/transcription error.
                    <Show when=move || recording.get() fallback=|| ().into_view()>
                        <div class="chat-attach-status">
                            "● Recording… speak now (stops on silence, or tap ⏹)"
                        </div>
                    </Show>
                    <Show when=move || transcribing.get() fallback=|| ().into_view()>
                        <div class="chat-attach-status">"Transcribing…"</div>
                    </Show>
                    <Show
                        when=move || stt_error.get().is_some()
                        fallback=|| ().into_view()
                    >
                        <div class="chat-attach-error">
                            {move || stt_error.get().unwrap_or_default()}
                        </div>
                    </Show>
                    // Slash-command menu (SOUL §12/§23): floats above the composer
                    // while the draft spells a command (`/new` or a skill name).
                    // `mousedown` is swallowed on each row so the textarea keeps
                    // focus (its blur would dismiss the menu before the click).
                    <Show
                        when=move || !slash_matches.with(Vec::is_empty)
                        fallback=|| ().into_view()
                    >
                        <div class="chat-slash-menu" role="listbox">
                            {move || {
                                let items = slash_matches.get();
                                let active = slash_idx.get().min(items.len().saturating_sub(1));
                                items
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, (name, desc))| {
                                        let pick = name.clone();
                                        view! {
                                            <button
                                                type="button"
                                                class="chat-slash-item"
                                                class:chat-slash-item-active=i == active
                                                role="option"
                                                aria-selected=if i == active { "true" } else { "false" }
                                                on:mousedown=move |ev: leptos::ev::MouseEvent| {
                                                    ev.prevent_default()
                                                }
                                                on:mouseenter=move |_| slash_idx.set(i)
                                                on:click=move |_| apply_slash(pick.clone())
                                            >
                                                <span class="chat-slash-name">{format!("/{name}")}</span>
                                                <span class="chat-slash-desc">{desc}</span>
                                            </button>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }}
                        </div>
                    </Show>
                    <div class="chat-input-row">
                        <textarea
                            class="chat-textarea"
                            placeholder="Message catalerum…  (Enter to send, Shift+Enter for newline, / for commands)"
                            rows="2"
                            prop:value=move || draft.get()
                            on:input=move |ev| {
                                draft.set(event_target_value(&ev));
                                slash_idx.set(0);
                                slash_dismissed.set(false);
                                // Typing ends a Tab-completion session: the new
                                // text is the stem now.
                                slash_stem.set(None);
                            }
                            on:keydown=on_keydown
                            on:blur=move |_| {
                                slash_dismissed.set(true);
                                // Leaving the box keeps whatever the draft shows;
                                // the session just ends.
                                slash_stem.set(None);
                            }
                            on:focus=move |_| slash_dismissed.set(false)
                        ></textarea>
                        <button
                            type="button"
                            class="chat-mic"
                            class:chat-mic-recording=move || recording.get()
                            class:chat-capability-checking=move || {
                                stt_capability.get() == SpeechCapability::Checking
                            }
                            disabled=move || {
                                transcribing.get() || !stt_capability.get().ready()
                            }
                            aria-label="Dictate a message"
                            aria-busy=move || {
                                (stt_capability.get() == SpeechCapability::Checking).to_string()
                            }
                            title=move || match stt_capability.get() {
                                SpeechCapability::Checking => {
                                    "Checking speech-to-text availability…"
                                }
                                SpeechCapability::Unavailable => {
                                    "Speech-to-text is unavailable right now"
                                }
                                SpeechCapability::Available if recording.get() => "Stop recording",
                                SpeechCapability::Available if transcribing.get() => "Transcribing…",
                                SpeechCapability::Available => {
                                    "Dictate a message (records into the composer)"
                                }
                            }
                            on:click=move |_| {
                                if transcribing.get_untracked()
                                    || !stt_capability.get_untracked().ready()
                                {
                                    return;
                                }
                                if recording.get_untracked() {
                                    stop_recording.run(());
                                } else {
                                    start_recording.run(RecordDest::Composer);
                                }
                            }
                        >
                            {move || {
                                if recording.get() {
                                    "⏹"
                                } else if transcribing.get() {
                                    "…"
                                } else {
                                    "🎙"
                                }
                            }}
                        </button>
                        <button
                            type="button"
                            class="chat-voice"
                            class:chat-capability-checking=move || {
                                stt_capability.get() == SpeechCapability::Checking
                                    || tts_capability.get() == SpeechCapability::Checking
                            }
                            aria-label="Start a voice conversation"
                            aria-busy=move || {
                                (stt_capability.get() == SpeechCapability::Checking
                                    || tts_capability.get() == SpeechCapability::Checking)
                                    .to_string()
                            }
                            title=move || match (stt_capability.get(), tts_capability.get()) {
                                (SpeechCapability::Checking, _)
                                | (_, SpeechCapability::Checking) => {
                                    "Checking voice-conversation availability…"
                                }
                                (SpeechCapability::Unavailable, _) => {
                                    "Voice conversation unavailable: speech-to-text is missing"
                                }
                                (_, SpeechCapability::Unavailable) => {
                                    "Voice conversation unavailable: text-to-speech is missing"
                                }
                                (SpeechCapability::Available, SpeechCapability::Available) => {
                                    "Start a voice conversation (hands-free)"
                                }
                            }
                            disabled=move || {
                                recording.get()
                                    || transcribing.get()
                                    || !stt_capability.get().ready()
                                    || !tts_capability.get().ready()
                            }
                            on:click=move |_| {
                                if stt_capability.get_untracked().ready()
                                    && tts_capability.get_untracked().ready()
                                {
                                    open_voice.run(());
                                }
                            }
                        >
                            <Icon icon=MdIcon::Headphones />
                        </button>
                        <label class="chat-attach-btn" title="Attach file">
                            <Icon icon=MdIcon::Attachment />
                            <input
                                type="file"
                                multiple
                                style="display:none;"
                                on:change=on_attach_file_change
                            />
                        </label>
                        <button
                            class="chat-send"
                            type="submit"
                            disabled=move || {
                                stopping.get()
                                    || uploads.get() > 0
                                    || (sending.get() && active_id.get().is_none())
                                    || (draft.with(|d| d.trim().is_empty())
                                        && attachments.with(Vec::is_empty))
                            }
                            title=move || {
                                if sending.get() {
                                    "Queue this message — it joins the conversation at the assistant's next step"
                                } else {
                                    "Send"
                                }
                            }
                        >
                            {move || if sending.get() { "Queue" } else { "Send" }}
                        </button>
                        <Show when=move || sending.get() fallback=|| ().into_view()>
                            <button
                                type="button"
                                class="chat-stop"
                                title="Stop generating"
                                disabled=move || stopping.get()
                                on:click=move |_| stop_turn.run(())
                            >
                                {move || if stopping.get() { "Stopping…" } else { "◼ Stop" }}
                            </button>
                        </Show>
                    </div>
                </form>
            </section>

            <Show when=move || voice_state.get() != VoiceState::Off fallback=|| ().into_view()>
                <VoiceOverlay
                    state=voice_state
                    level=voice_level
                    heard=voice_heard
                    error=voice_error
                    on_close=close_voice
                    on_orb=voice_orb_tap
                    on_toggle_pause=toggle_voice_pause
                />
            </Show>

            <Show when=move || sidebar_open.get() fallback=|| ().into_view()>
                <button
                    class="chat-side-scrim"
                    aria-label="Close panel"
                    tabindex="-1"
                    on:click=move |_| sidebar_open.set(false)
                ></button>
                <aside class="chat-side">
                    <div class="chat-side-tabs">
                        <button
                            class="chat-side-tab"
                            class:chat-side-tab-active=move || sidebar_tab.get() == SideTab::Output
                            on:click=move |_| sidebar_tab.set(SideTab::Output)
                        >
                            "Output"
                        </button>
                        <button
                            class="chat-side-tab"
                            class:chat-side-tab-active=move || sidebar_tab.get() == SideTab::Settings
                            on:click=move |_| sidebar_tab.set(SideTab::Settings)
                        >
                            "Settings"
                        </button>
                        <button
                            class="chat-side-close"
                            title="Close panel"
                            on:click=move |_| sidebar_open.set(false)
                        >
                            <Icon icon=MdIcon::Close />
                        </button>
                    </div>
                    <div class="chat-side-body">
                        // Output tab: mount the read-only terminal pane only while it
                        // is the visible tab, so its stream opens lazily (as the old
                        // "Show output" toggle did) and closes on tab switch.
                        <Show
                            when=move || sidebar_tab.get() == SideTab::Output
                            fallback=|| ().into_view()
                        >
                            <TerminalPane />
                        </Show>
                        // Settings tab: the per-conversation pickers (profile / folder /
                        // model). For a persisted chat each binds server-side on change;
                        // for a brand-new chat they hold the picks until the first send,
                        // when they bind to the freshly created conversation — so they're
                        // editable up front, before the thread exists.
                        <Show
                            when=move || sidebar_tab.get() == SideTab::Settings
                            fallback=|| ().into_view()
                        >
                            <div class="chat-settings">
                                <Show
                                    when=move || active_id.get().is_none()
                                    fallback=|| ().into_view()
                                >
                                    <p class="chat-side-hint">
                                        "Set the profile, folder, and model for this new chat — they take effect when you send the first message, and stay editable after."
                                    </p>
                                </Show>
                                <Show
                                    when=move || !profiles.get().is_empty()
                                    fallback=|| ().into_view()
                                >
                                    <div class="chat-set-field">
                                        <label class="chat-set-label">"Run as"</label>
                                        <select
                                            class="chat-set-select"
                                            prop:value=current_profile
                                            disabled=move || binding_profile.get()
                                            on:change=move |ev| set_profile(event_target_value(&ev))
                                        >
                                            <option value="">"Default (your role)"</option>
                                            <For
                                                each=move || profiles.get()
                                                key=|p| p.id.clone()
                                                children=move |p: AgentProfile| {
                                                    view! {
                                                        <option value=p.id.clone()>
                                                            {p.name.clone()}
                                                        </option>
                                                    }
                                                }
                                            />
                                        </select>
                                        <Show
                                            when=move || bind_error.get().is_some()
                                            fallback=|| ().into_view()
                                        >
                                            <span class="chat-set-error">
                                                {move || bind_error.get().unwrap_or_default()}
                                            </span>
                                        </Show>
                                    </div>
                                </Show>
                                <div class="chat-set-field">
                                    <label class="chat-set-label">"Model"</label>
                                    {model_autocomplete(
                                        Signal::derive(current_model),
                                        set_model,
                                        model_options(models, false),
                                        Signal::derive(|| "Default".to_string()),
                                        Signal::derive(move || setting_model.get()),
                                        "chat-set-select",
                                    )}
                                    <Show
                                        when=move || model_error.get().is_some()
                                        fallback=|| ().into_view()
                                    >
                                        <span class="chat-set-error">
                                            {move || model_error.get().unwrap_or_default()}
                                        </span>
                                    </Show>
                                </div>
                                {move || {
                                    let id = current_model();
                                    if id.trim().is_empty() {
                                        return None;
                                    }
                                    // Capabilities from the catalog entry (when the id is enumerated),
                                    // plus the per-user force-image override — so a forced model shows
                                    // even if the catalog doesn't list it.
                                    let info = models.get().into_iter().find(|m| m.id == id);
                                    let forced = forced_image_models
                                        .get()
                                        .iter()
                                        .any(|m| m == &id);
                                    let catalog_vision = info.as_ref().is_some_and(|m| {
                                        m.input_modalities.iter().any(|x| x.eq_ignore_ascii_case("image"))
                                    });
                                    let vision = catalog_vision || forced;
                                    let img_out = info.as_ref().is_some_and(|m| {
                                        m.output_modalities.iter().any(|x| x.eq_ignore_ascii_case("image"))
                                    });
                                    let tools = info.as_ref().is_some_and(|m| {
                                        m.supported_parameters.iter().any(|x| x.eq_ignore_ascii_case("tools"))
                                    });
                                    let reasoning = info.as_ref().is_some_and(|m| {
                                        m.supported_parameters
                                            .iter()
                                            .any(|x| x.to_ascii_lowercase().contains("reasoning"))
                                    });
                                    let ctx = info
                                        .as_ref()
                                        .and_then(|m| m.context_length)
                                        .filter(|c| *c > 0)
                                        .map(|c| {
                                            if c >= 1000 {
                                                format!("{}K context", c / 1000)
                                            } else {
                                                format!("{c} context")
                                            }
                                        });
                                    let vision_label = if !vision {
                                        "🚫 No image input"
                                    } else if forced && !catalog_vision {
                                        "🖼 Vision (forced)"
                                    } else {
                                        "🖼 Vision"
                                    };
                                    Some(
                                        view! {
                                            <div class="chat-set-field">
                                                <label class="chat-set-label">
                                                    "Model capabilities"
                                                </label>
                                                <div class="chat-set-caps">
                                                    <span class="chat-cap" class:chat-cap-on=vision>
                                                        {vision_label}
                                                    </span>
                                                    {tools
                                                        .then(|| {
                                                            view! {
                                                                <span class="chat-cap chat-cap-on">"🔧 Tools"</span>
                                                            }
                                                        })}
                                                    {reasoning
                                                        .then(|| {
                                                            view! {
                                                                <span class="chat-cap chat-cap-on">"🧠 Reasoning"</span>
                                                            }
                                                        })}
                                                    {img_out
                                                        .then(|| {
                                                            view! {
                                                                <span class="chat-cap chat-cap-on">"🎨 Image output"</span>
                                                            }
                                                        })}
                                                    {ctx
                                                        .map(|c| {
                                                            view! { <span class="chat-cap">{c}</span> }
                                                        })}
                                                </div>
                                                <button
                                                    class="chat-cap-force"
                                                    title="Send image attachments to this model even when the gateway catalog doesn't advertise image input"
                                                    on:click=move |_| toggle_force_image()
                                                >
                                                    {if forced {
                                                        "Don't force image input"
                                                    } else {
                                                        "Force image input"
                                                    }}
                                                </button>
                                            </div>
                                        },
                                    )
                                }}
                                <div class="chat-set-field">
                                    <label class="chat-set-label">"Thinking"</label>
                                    <select
                                        class="chat-set-select"
                                        prop:value=current_reasoning
                                        disabled=move || setting_reasoning.get()
                                        on:change=move |ev| set_reasoning(event_target_value(&ev))
                                    >
                                        <option value="">"Off (model default)"</option>
                                        <option value="low">"Low"</option>
                                        <option value="medium">"Medium"</option>
                                        <option value="high">"High"</option>
                                        <option value="xhigh">"Extra high"</option>
                                        <option value="max">"Max"</option>
                                    </select>
                                    <span class="chat-side-hint">
                                        "Requests extended reasoning on models that support it; ignored otherwise."
                                    </span>
                                    <Show
                                        when=move || reasoning_error.get().is_some()
                                        fallback=|| ().into_view()
                                    >
                                        <span class="chat-set-error">
                                            {move || reasoning_error.get().unwrap_or_default()}
                                        </span>
                                    </Show>
                                </div>
                                // Debug: export the persisted transcript verbatim.
                                // Only for a saved thread — a brand-new chat has no
                                // server-side messages to dump yet.
                                <Show
                                    when=move || active_id.get().is_some()
                                    fallback=|| ().into_view()
                                >
                                    <div class="chat-set-field">
                                        <label class="chat-set-label">"Debug"</label>
                                        <button
                                            class="chat-debug-btn"
                                            class:chat-debug-btn-done=move || debug_copied.get()
                                            disabled=move || debug_copying.get()
                                            title="Copy this conversation's stored transcript (messages, tool calls, errors, token usage) as JSON"
                                            on:click=move |_| copy_debug_json()
                                        >
                                            {move || {
                                                if debug_copied.get() {
                                                    "Copied ✓"
                                                } else if debug_copying.get() {
                                                    "Copying…"
                                                } else {
                                                    "Copy chat as JSON"
                                                }
                                            }}
                                        </button>
                                        <span class="chat-side-hint chat-debug-hint">
                                            "Copies the raw stored transcript — roles, tool calls and results, errors, token usage — so you can paste it into a bug report or hand it to an AI model to diagnose this chat."
                                        </span>
                                        <Show
                                            when=move || debug_copy_error.get().is_some()
                                            fallback=|| ().into_view()
                                        >
                                            <span class="chat-set-error">
                                                {move || debug_copy_error.get().unwrap_or_default()}
                                            </span>
                                        </Show>
                                    </div>
                                </Show>
                            </div>
                        </Show>
                    </div>
                </aside>
            </Show>
        </section>
    }
}

/// One user-turn attachment (SOUL §9) as it rides on **top** of a chat bubble: an
/// inline thumbnail for an image, a labelled download chip for anything else, or
/// inert text when the URL uses an unsafe scheme (the XSS gate). The href is
/// resolved via [`attachment_href`] so an uploaded object carries its auth token.
/// Mirrors the calendar event attachment view — both lean on the same shared
/// [`crate::components::widgets`] helpers.
fn message_attachment_view(att: &Attachment) -> AnyView {
    let href = attachment_href(att);
    let name = attachment_label(att);
    let safe = is_safe_href(&href);
    if safe && attachment_is_image(att) {
        let alt = name.clone();
        let img_src = href.clone();
        view! {
            <a
                class="msg-attachment msg-attachment-img-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=name
            >
                <img class="msg-attachment-img" src=img_src alt=alt loading="lazy" />
            </a>
        }
        .into_any()
    } else if safe {
        let title = name.clone();
        view! {
            <a
                class="msg-attachment msg-attachment-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=title
            >
                <span class="msg-attachment-icon"><Icon icon=MdIcon::File /></span>
                <span class="msg-attachment-name">{name}</span>
            </a>
        }
        .into_any()
    } else {
        // Unsafe scheme (e.g. `javascript:`): render inert — never a clickable link.
        view! {
            <span class="msg-attachment msg-attachment-unsafe" title=att.url.clone()>
                <span class="msg-attachment-icon"><Icon icon=MdIcon::Warning /></span>
                <span class="msg-attachment-name">{name}</span>
            </span>
        }
        .into_any()
    }
}

/// Flag a streaming line as an error line (used when a turn fails before any
/// text arrives).
fn mark_error(lines: RwSignal<Vec<ChatLine>>, id: usize) {
    lines.update(|v| {
        if let Some(l) = v.iter_mut().find(|l| l.id == id) {
            l.role = Role::Error;
        }
    });
}

/// Whether a tool result string looks like a failure. Tool errors are persisted
/// as a `{"error": "…"}` JSON object in the result row's content (the backend has
/// no separate error flag before the §Phase-3 migration), so this is a heuristic:
/// a JSON object with a top-level `error` key. A tool that legitimately returns an
/// `error` field is a false positive — accepted until the persisted flag lands.
fn looks_like_error(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content.trim())
        .ok()
        .and_then(|v| v.get("error").map(|_| ()))
        .is_some()
}

/// App authoring calls mount the affected definition inline; they do not also
/// need a generic tool card. Keep live streaming and replay on one predicate.
fn is_ui_authoring_tool(name: &str) -> bool {
    matches!(
        name,
        "present_ui" | "create_ui_components" | "edit_ui_components" | "edit_ui"
    )
}

/// A short label for a tool call's status glyph (live spinner / ✓ / ✗).
fn tool_status_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "spinner",
        ToolStatus::Ok => "✓",
        ToolStatus::Err => "✗",
    }
}

/// Format a tool call's duration as a compact readout (e.g. `412ms`, `1.8s`).
fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Format a per-turn LLM cost as a compact USD readout for the chat bubble. A
/// nonzero cost below the 4-decimal resolution shows as `<$0.0001` rather than a
/// misleading `$0.0000`.
fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.0001 {
        "<$0.0001".to_string()
    } else {
        format!("${usd:.4}")
    }
}

/// Group a token count into thousands (`12345` → `12,345`) for the readouts.
fn fmt_int(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + (len.saturating_sub(1)) / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Build the hover tooltip for a turn's token info-icon: this turn's input/output
/// split, a cache line (only when the provider cached anything), and the running
/// conversation total up to and including this turn. Newline-separated so the
/// native `title` tooltip renders it as multiple lines.
fn fmt_token_tooltip(t: &TurnTokens, cumulative_total: u32) -> String {
    let mut s = format!(
        "This turn: {} in + {} out = {} tokens",
        fmt_int(t.prompt_tokens),
        fmt_int(t.completion_tokens),
        fmt_int(t.total_tokens),
    );
    if t.cached_tokens > 0 || t.cache_creation_tokens > 0 {
        s.push_str(&format!(
            "\nCache: {} read · {} written",
            fmt_int(t.cached_tokens),
            fmt_int(t.cache_creation_tokens),
        ));
    }
    s.push_str(&format!(
        "\nTotal up to here: {} tokens",
        fmt_int(cumulative_total)
    ));
    s
}

/// A recognized composer slash command (SOUL §12/§23).
#[derive(Clone, Debug, PartialEq, Eq)]
enum SlashCmd {
    /// `/new` — start a fresh chat.
    New,
    /// `/<skill>` — invoke the named skill (the canonical `Skill::name`).
    Skill(String),
}

/// Parse a draft that names a slash command: a leading `/` followed by `new` or
/// a skill name, then optional arguments. Returns the command plus its trimmed
/// arguments, or `None` to send the draft as typed (a message may legitimately
/// start with a path like `/etc/hosts`).
///
/// `new` is checked first, so it shadows a same-named skill. Skill names may
/// contain spaces, so matching tries the longest exact name prefix first (byte
/// slicing on the raw draft stays safe that way), then falls back to a
/// case-insensitive match on the first whitespace-delimited token.
fn parse_slash_command(draft: &str, skills: &[Skill]) -> Option<(SlashCmd, String)> {
    let rest = draft.trim().strip_prefix('/')?;
    let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (token, tail) = rest.split_at(token_end);
    if token.eq_ignore_ascii_case("new") {
        return Some((SlashCmd::New, tail.trim().to_string()));
    }
    let exact = skills
        .iter()
        .filter(|s| {
            rest == s.name
                || rest
                    .strip_prefix(&s.name)
                    .is_some_and(|r| r.starts_with(char::is_whitespace))
        })
        .max_by_key(|s| s.name.len());
    let skill = exact.or_else(|| skills.iter().find(|s| s.name.eq_ignore_ascii_case(token)))?;
    let args = rest
        .strip_prefix(&skill.name)
        .unwrap_or(tail)
        .trim()
        .to_string();
    Some((SlashCmd::Skill(skill.name.clone()), args))
}

/// Derive a conversation title from its opening message: the first non-empty
/// line, trimmed to 50 chars. Falls back to "New chat" when empty.
fn derive_title(content: &str) -> String {
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let title: String = first.trim().chars().take(50).collect();
    if title.is_empty() {
        "New chat".to_string()
    } else {
        title
    }
}

/// Current wall-clock time as Unix epoch milliseconds (the browser clock). Only
/// reached from the live component, never from native unit tests.
fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// The browser's local-time offset from UTC in milliseconds — the value to *add*
/// to a UTC epoch timestamp to get local wall-clock time (`getTimezoneOffset`
/// counts the other way). Only reached from the live component, never from
/// native unit tests.
fn local_tz_offset_ms() -> i64 {
    -(js_sys::Date::new_0().get_timezone_offset() as i64) * 60_000
}

/// A collision-resistant object key for a chat upload: `chat/<ms>-<rand>-<name>`
/// (SOUL §9). Mirrors the calendar attachment key scheme — the millisecond stamp
/// orders uploads and the random component keeps same-selection keys distinct; the
/// storage route percent-encodes each segment, so a crafted filename can't escape.
fn chat_upload_key(name: &str) -> String {
    let rand = (js_sys::Math::random() * 1_000_000_000.0) as u64;
    format!("chat/{}-{}-{}", now_ms(), rand, name.trim_matches('/'))
}

/// Full month names, indexed by `month - 1`.
const MONTH_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Abbreviated month names, indexed by `month - 1`.
const MONTH_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Filter conversations by a case-insensitive substring of their title. An empty
/// (or whitespace-only) `query` returns all; an untitled conversation matches only
/// the empty query. Order is preserved, so a later [`group_sessions`] still sees
/// newest-first input.
fn filter_conversations(convs: &[Conversation], query: &str) -> Vec<Conversation> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return convs.to_vec();
    }
    convs
        .iter()
        .filter(|c| {
            c.title
                .as_deref()
                .is_some_and(|t| t.to_lowercase().contains(&q))
        })
        .cloned()
        .collect()
}

/// Group conversations into labelled recency runs: "Today", "Yesterday",
/// "Previous 7 days", then one group per week (1–8 weeks old), then one per
/// month. Input is assumed newest-first (the API's `created_at DESC` ordering),
/// so each [`Bucket`] forms a single contiguous run and groups come out in the
/// right order. `tz_offset_ms` shifts UTC timestamps to the user's local
/// wall-clock so day boundaries match their calendar (see [`local_tz_offset_ms`]).
fn group_sessions(convs: &[Conversation], now_ms: i64, tz_offset_ms: i64) -> Vec<SessionGroup> {
    let mut groups: Vec<SessionGroup> = Vec::new();
    let mut current: Option<Bucket> = None;
    for c in convs {
        let bucket = match parse_unix_ms(&c.created_at) {
            Some(ms) => classify(ms, now_ms, tz_offset_ms),
            None => Bucket::Unknown,
        };
        if current.as_ref() != Some(&bucket) {
            groups.push(SessionGroup {
                label: bucket_label(&bucket),
                items: Vec::new(),
            });
            current = Some(bucket);
        }
        groups
            .last_mut()
            .expect("a group was just pushed")
            .items
            .push(c.clone());
    }
    groups
}

/// A change-sensitive key for a [`SessionGroup`] in the sidebar's keyed `<For>`.
///
/// A keyed `<For>` never re-renders a child whose key is unchanged, and each
/// group child captures its `items` once (the inner `<For>` reads a plain `Vec`,
/// not a signal). Keying only by the (stable) recency label — "Last 7 days" etc.
/// — therefore froze a group's session list: a conversation created, renamed, or
/// deleted within an existing bucket never re-rendered (the "no chat is added to
/// the sidebar" bug). Folding each conversation's id + title into the key makes
/// the key change whenever the group's contents change, so the group re-renders
/// with a fresh list. Active-row highlighting stays reactive per row (its class
/// reads `active_id`), so it needn't be part of the key.
fn group_key(g: &SessionGroup) -> String {
    let mut key = g.label.clone();
    for c in &g.items {
        key.push('\u{1f}'); // unit separator — can't appear in id/title
        key.push_str(&c.id);
        key.push('\u{1e}'); // record separator
        key.push_str(c.title.as_deref().unwrap_or(""));
    }
    key
}

/// Classify a conversation's start time into a recency [`Bucket`]. All
/// boundaries are local calendar days (not rolling 24h windows): `tz_offset_ms`
/// is added to the UTC timestamps before computing the day number.
fn classify(created_ms: i64, now_ms: i64, tz_offset_ms: i64) -> Bucket {
    const DAY: i64 = 86_400_000;
    let z = (created_ms + tz_offset_ms).div_euclid(DAY);
    let today = (now_ms + tz_offset_ms).div_euclid(DAY);
    match today - z {
        // A future-dated item (clock skew) still reads as "Today".
        i64::MIN..=0 => Bucket::Today,
        1 => Bucket::Yesterday,
        2..=6 => Bucket::Last7,
        7..=55 => {
            // The Monday of the item's week (Unix day 0 = Thursday, so
            // Monday-based weekday index is `(z + 3) mod 7`).
            let monday = z - (z + 3).rem_euclid(7);
            Bucket::Week(monday)
        }
        _ => {
            let (y, m, _) = civil_from_days(z);
            Bucket::Month(y, m)
        }
    }
}

/// The heading shown for a [`Bucket`].
fn bucket_label(bucket: &Bucket) -> String {
    match bucket {
        Bucket::Today => "Today".to_string(),
        Bucket::Yesterday => "Yesterday".to_string(),
        Bucket::Last7 => "Previous 7 days".to_string(),
        Bucket::Week(monday) => {
            let (_, m, d) = civil_from_days(*monday);
            format!("Week of {} {}", MONTH_ABBR[(m - 1) as usize], d)
        }
        Bucket::Month(y, m) => format!("{} {}", MONTH_FULL[(*m - 1) as usize], y),
        Bucket::Unknown => "Earlier".to_string(),
    }
}

/// Parse an RFC 3339 / ISO-8601 UTC timestamp (`YYYY-MM-DDТHH:MM:SS…`, as the API
/// emits for `created_at`) into Unix epoch milliseconds. Lenient: it reads the
/// date and `HH:MM:SS`, ignoring any fractional seconds and treating the value as
/// UTC (the API always serializes `Z`). Returns `None` on a shape it can't read.
fn parse_unix_ms(s: &str) -> Option<i64> {
    let sep = s.find(['T', 't', ' '])?;
    let (date, rest) = (&s[..sep], &s[sep + 1..]);

    let mut dp = date.split('-');
    let y: i64 = dp.next()?.trim().parse().ok()?;
    let mo: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) {
        return None;
    }

    let hh: i64 = rest.get(0..2)?.parse().ok()?;
    let mm: i64 = rest.get(3..5).and_then(|x| x.parse().ok()).unwrap_or(0);
    let ss: i64 = rest.get(6..8).and_then(|x| x.parse().ok()).unwrap_or(0);

    let days = days_from_civil(y, mo, d);
    Some(days * 86_400_000 + hh * 3_600_000 + mm * 60_000 + ss * 1000)
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian civil date.
/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// The civil `(year, month, day)` for a count of days since the Unix epoch.
/// Howard Hinnant's `civil_from_days` (inverse of [`days_from_civil`]).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(created_at: &str) -> Conversation {
        Conversation {
            id: created_at.to_string(),
            workspace_id: String::new(),
            title: Some(created_at.to_string()),
            origin: String::new(),
            created_at: created_at.to_string(),
            agent_profile_id: None,
            model: None,
            reasoning_effort: None,
            tags: Vec::new(),
            title_manual: false,
        }
    }

    fn skill(name: &str) -> Skill {
        Skill {
            id: name.to_string(),
            workspace_id: String::new(),
            name: name.to_string(),
            description: String::new(),
            instructions_md: String::new(),
            tools: Vec::new(),
            code: None,
            advertised: true,
        }
    }

    #[test]
    fn transient_rest_errors_retry_definitive_ones_do_not() {
        use crate::rest::RestError;
        assert!(transient_rest_error(&RestError::Transport(
            "offline".into()
        )));
        assert!(transient_rest_error(&RestError::Status {
            status: 502,
            message: String::new(),
        }));
        assert!(transient_rest_error(&RestError::Status {
            status: 429,
            message: String::new(),
        }));
        // A definitive rejection (no STT model, bad audio) fails identically on
        // retry — it must surface at once.
        assert!(!transient_rest_error(&RestError::Status {
            status: 400,
            message: String::new(),
        }));
        assert!(!transient_rest_error(&RestError::Status {
            status: 415,
            message: String::new(),
        }));
        assert!(!transient_rest_error(&RestError::Decode("bad json".into())));
    }

    #[test]
    fn every_app_authoring_tool_mounts_inline_without_a_tool_card() {
        assert!(is_ui_authoring_tool("present_ui"));
        assert!(is_ui_authoring_tool("create_ui_components"));
        assert!(is_ui_authoring_tool("edit_ui_components"));
        assert!(is_ui_authoring_tool("edit_ui"));
        assert!(!is_ui_authoring_tool("read_ui"));
    }

    #[test]
    fn voice_input_compression_shortens_and_downmixes_pcm() {
        let left = vec![0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
        let right = vec![0.0, -0.2, -0.4, -0.6, -0.8, -1.0];
        let compressed = compress_pcm(&[left, right], 1.5);
        assert_eq!(compressed.len(), 4, "six frames at 1.5× become four");
        assert!(compressed.iter().all(|sample| sample.abs() < f32::EPSILON));

        let mono = compress_pcm(&[(0..9).map(|n| n as f32 / 10.0).collect()], 1.5);
        assert_eq!(mono.len(), 6);
        assert!((mono[1] - 0.15).abs() < 0.001);
    }

    #[test]
    fn compressed_pcm_encodes_a_valid_mono_wav() {
        let wav = pcm16_wav(&[-1.0, 0.0, 1.0], 48_000).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(wav.len(), 50);
    }

    #[test]
    fn speech_capability_is_not_ready_until_a_nonempty_catalog_arrives() {
        assert!(!SpeechCapability::Checking.ready());
        assert!(!SpeechCapability::from_model_count(0).ready());
        assert!(SpeechCapability::from_model_count(1).ready());
    }

    #[test]
    fn tool_only_rounds_do_not_render_an_empty_prose_bubble() {
        assert!(!should_render_message_text("", false, true));
        // The bubble also disappears live as soon as the tool card arrives.
        assert!(!should_render_message_text(" \n ", true, true));
    }

    #[test]
    fn empty_streaming_assistant_keeps_its_initial_waiting_bubble() {
        assert!(should_render_message_text("", true, false));
        assert!(should_render_message_text(
            "Calling the search tool…",
            false,
            true
        ));
    }

    #[test]
    fn chat_bottom_detection_allows_rounding_but_not_a_reader_scroll_up() {
        assert!(chat_is_at_bottom(500, 1_000, 500));
        assert!(chat_is_at_bottom(500 - CHAT_BOTTOM_SLOP_PX, 1_000, 500));
        assert!(!chat_is_at_bottom(499 - CHAT_BOTTOM_SLOP_PX, 1_000, 500));
    }

    #[test]
    fn parse_slash_command_recognizes_new_with_and_without_opener() {
        assert_eq!(
            parse_slash_command("/new", &[]),
            Some((SlashCmd::New, String::new()))
        );
        assert_eq!(
            parse_slash_command("  /NEW plan my week  ", &[]),
            Some((SlashCmd::New, "plan my week".to_string()))
        );
    }

    #[test]
    fn parse_slash_command_matches_skills_and_extracts_args() {
        let skills = [skill("summarize"), skill("triage inbox")];
        assert_eq!(
            parse_slash_command("/summarize", &skills),
            Some((SlashCmd::Skill("summarize".to_string()), String::new()))
        );
        assert_eq!(
            parse_slash_command("/summarize the meeting notes", &skills),
            Some((
                SlashCmd::Skill("summarize".to_string()),
                "the meeting notes".to_string()
            ))
        );
        // A spaced name matches whole, not just its first token.
        assert_eq!(
            parse_slash_command("/triage inbox from today", &skills),
            Some((
                SlashCmd::Skill("triage inbox".to_string()),
                "from today".to_string()
            ))
        );
        // Hand-typed case still resolves to the canonical name.
        assert_eq!(
            parse_slash_command("/Summarize this", &skills),
            Some((SlashCmd::Skill("summarize".to_string()), "this".to_string()))
        );
    }

    #[test]
    fn parse_slash_command_prefers_longest_name_and_new_over_a_skill() {
        let skills = [skill("sum"), skill("sum it up"), skill("new")];
        assert_eq!(
            parse_slash_command("/sum it up quickly", &skills),
            Some((
                SlashCmd::Skill("sum it up".to_string()),
                "quickly".to_string()
            ))
        );
        // The built-in wins over a skill that happens to be named "new".
        assert_eq!(
            parse_slash_command("/new", &skills),
            Some((SlashCmd::New, String::new()))
        );
    }

    #[test]
    fn parse_slash_command_passes_through_non_commands() {
        let skills = [skill("summarize")];
        assert_eq!(parse_slash_command("hello", &skills), None);
        assert_eq!(parse_slash_command("/etc/hosts is odd", &skills), None);
        assert_eq!(parse_slash_command("/", &skills), None);
    }

    #[test]
    fn fmt_cost_formats_and_floors_tiny_costs() {
        assert_eq!(fmt_cost(0.0), "$0.0000");
        assert_eq!(fmt_cost(0.0123), "$0.0123");
        assert_eq!(fmt_cost(1.5), "$1.5000");
        // A nonzero cost below 4-decimal resolution is flagged, not rounded to zero.
        assert_eq!(fmt_cost(0.00002), "<$0.0001");
    }

    #[test]
    fn fmt_int_groups_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(42), "42");
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(12_345), "12,345");
        assert_eq!(fmt_int(1_234_567), "1,234,567");
    }

    #[test]
    fn token_tooltip_includes_turn_cache_and_running_total() {
        let t = TurnTokens {
            prompt_tokens: 1_200,
            completion_tokens: 340,
            total_tokens: 1_540,
            cached_tokens: 800,
            cache_creation_tokens: 0,
        };
        let tip = fmt_token_tooltip(&t, 5_402);
        assert_eq!(
            tip,
            "This turn: 1,200 in + 340 out = 1,540 tokens\nCache: 800 read · 0 written\nTotal up to here: 5,402 tokens"
        );
    }

    #[test]
    fn token_tooltip_omits_cache_line_when_no_caching() {
        let t = TurnTokens {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: 0,
            cache_creation_tokens: 0,
        };
        let tip = fmt_token_tooltip(&t, 150);
        // No cache activity → the cache line is dropped, leaving turn + running total.
        assert_eq!(
            tip,
            "This turn: 100 in + 50 out = 150 tokens\nTotal up to here: 150 tokens"
        );
    }

    #[test]
    fn civil_round_trips() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (1999, 12, 31), (2026, 6, 18)] {
            let z = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(z), (y, m, d), "round trip {y}-{m}-{d}");
        }
    }

    #[test]
    fn epoch_day_zero_is_1970() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn monday_of_week_is_correct() {
        // 2026-06-18 is a Thursday; its week's Monday is 2026-06-15.
        let z = days_from_civil(2026, 6, 18);
        let monday = z - (z + 3).rem_euclid(7);
        assert_eq!(civil_from_days(monday), (2026, 6, 15));
    }

    #[test]
    fn parses_rfc3339_to_epoch_ms() {
        let ms = parse_unix_ms("2026-06-18T09:30:00Z").unwrap();
        let expected = days_from_civil(2026, 6, 18) * 86_400_000 + 9 * 3_600_000 + 30 * 60_000;
        assert_eq!(ms, expected);
        // Tolerates a space separator and fractional seconds; rejects garbage.
        assert!(parse_unix_ms("2026-06-18 09:30:00.123Z").is_some());
        assert!(parse_unix_ms("not-a-date").is_none());
        assert!(parse_unix_ms("").is_none());
    }

    #[test]
    fn classify_splits_today_yesterday_last7_week_month() {
        const DAY: i64 = 86_400_000;
        let now = days_from_civil(2026, 6, 18) * DAY + 12 * 3_600_000;
        assert_eq!(classify(now - 3_600_000, now, 0), Bucket::Today);
        assert_eq!(classify(now + 3_600_000, now, 0), Bucket::Today); // clock skew
        assert_eq!(classify(now - DAY, now, 0), Bucket::Yesterday);
        assert_eq!(classify(now - 2 * DAY, now, 0), Bucket::Last7);
        assert_eq!(classify(now - 6 * DAY, now, 0), Bucket::Last7);
        assert!(matches!(classify(now - 10 * DAY, now, 0), Bucket::Week(_)));
        assert!(matches!(
            classify(now - 100 * DAY, now, 0),
            Bucket::Month(2026, 3)
        ));
    }

    #[test]
    fn classify_uses_local_day_boundaries() {
        const DAY: i64 = 86_400_000;
        const HOUR: i64 = 3_600_000;
        // Now is 00:30 UTC; the item was created an hour earlier, on the
        // previous UTC day. At UTC+2 both fall on the same local day ("Today");
        // at UTC that hour crosses midnight ("Yesterday").
        let now = days_from_civil(2026, 6, 18) * DAY + 30 * 60_000;
        let created = now - HOUR;
        assert_eq!(classify(created, now, 2 * HOUR), Bucket::Today);
        assert_eq!(classify(created, now, 0), Bucket::Yesterday);
    }

    #[test]
    fn group_sessions_labels_and_orders() {
        const DAY: i64 = 86_400_000;
        let now = days_from_civil(2026, 6, 18) * DAY + 12 * 3_600_000;
        let convs = vec![
            conv("2026-06-18T09:00:00Z"), // same day -> Today
            conv("2026-06-17T09:00:00Z"), // 1 day    -> Yesterday
            conv("2026-06-16T09:00:00Z"), // 2 days   -> Previous 7 days
            conv("2026-06-13T09:00:00Z"), // 5 days   -> Previous 7 days
            conv("2026-06-05T09:00:00Z"), // 13 days  -> a week bucket
            conv("2026-03-01T09:00:00Z"), // 109 days -> March 2026
        ];
        let groups = group_sessions(&convs, now, 0);
        // Five distinct groups, in newest-first order.
        assert_eq!(groups.len(), 5);
        assert_eq!(groups[0].label, "Today");
        assert_eq!(groups[1].label, "Yesterday");
        assert_eq!(groups[2].label, "Previous 7 days");
        assert_eq!(groups[2].items.len(), 2);
        assert!(groups[3].label.starts_with("Week of"));
        assert_eq!(groups[4].label, "March 2026");
    }

    #[test]
    fn group_sessions_handles_unparseable_created_at() {
        let groups = group_sessions(&[conv("")], 0, 0);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].label, "Earlier");
    }

    #[test]
    fn group_key_changes_when_group_contents_change() {
        // The sidebar's keyed <For> won't re-render a group whose key is
        // unchanged, so the key must reflect the group's contents — otherwise a
        // newly-added / renamed / removed conversation in an existing recency
        // bucket never appears (the "no chat is added to the sidebar" bug).
        let g = |items: Vec<Conversation>| SessionGroup {
            label: "Last 7 days".to_string(),
            items,
        };
        let a = conv("2026-06-18T09:00:00Z");
        let b = conv("2026-06-17T09:00:00Z");

        // Same label, different membership → different key.
        let one = group_key(&g(vec![a.clone()]));
        let two = group_key(&g(vec![b.clone(), a.clone()]));
        assert_ne!(one, two, "adding a conversation must change the key");

        // A rename (same id, new title) must change the key.
        let mut a_renamed = a.clone();
        a_renamed.title = Some("renamed".to_string());
        assert_ne!(
            group_key(&g(vec![a.clone()])),
            group_key(&g(vec![a_renamed])),
            "renaming a conversation must change the key"
        );

        // Identical contents → identical key (so unchanged groups stay put).
        assert_eq!(group_key(&g(vec![a.clone()])), group_key(&g(vec![a])));
    }

    #[test]
    fn filter_conversations_matches_title_case_insensitively() {
        let convs = vec![conv("Alpha plan"), conv("beta NOTES"), conv("gamma")];
        // Empty / whitespace query → everything (no filtering).
        assert_eq!(filter_conversations(&convs, "  ").len(), 3);
        // Case-insensitive substring on the title (uppercase query, lowercase title).
        let m: Vec<_> = filter_conversations(&convs, "notes")
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(m, ["beta NOTES"]);
        assert!(filter_conversations(&convs, "zzz").is_empty());
        // An untitled conversation matches only the empty query.
        let untitled = Conversation {
            id: "u".into(),
            workspace_id: String::new(),
            title: None,
            origin: String::new(),
            created_at: String::new(),
            agent_profile_id: None,
            model: None,
            reasoning_effort: None,
            tags: Vec::new(),
            title_manual: false,
        };
        assert!(filter_conversations(std::slice::from_ref(&untitled), "u").is_empty());
        assert_eq!(filter_conversations(&[untitled], "").len(), 1);
    }

    #[test]
    fn derive_title_trims_and_falls_back() {
        assert_eq!(derive_title("  hello world  "), "hello world");
        assert_eq!(derive_title("\n\nsecond line\nthird"), "second line");
        assert_eq!(derive_title("   "), "New chat");
        let long = "x".repeat(80);
        assert_eq!(derive_title(&long).chars().count(), 50);
    }
}
