//! Chat WebSocket transport (SOUL §12: SSE→StreamEvent→WS→ChatPanel).
//!
//! Wraps [`gloo_net::websocket::futures::WebSocket`] for the `/ws/chat`
//! endpoint: builds the auth'd URL, sends [`ClientChatMessage`] turns as JSON
//! text frames, and exposes the inbound frames as a stream of [`StreamUpdate`]s
//! the [`crate::components::chat`] panel reduces into the message list.
//!
//! The connection is opened lazily on first send and reused for the rest of the
//! session (one socket per [`ChatSocket`]).

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use gloo_net::websocket::futures::WebSocket;
use gloo_net::websocket::Message;

use serde::Deserialize;

use crate::api::{
    api_base, frame_seq, parse_frame, ClientChatMessage, StreamUpdate, WS_CHAT_PATH, WS_SPEECH_PATH,
};
use crate::strip_ansi::AnsiStripper;

/// A half-duplex pair over the chat WebSocket: a sink for outbound turns and a
/// stream of decoded inbound [`StreamUpdate`]s.
pub struct ChatSocket {
    sink: SplitSink<WebSocket, Message>,
    stream: SplitStream<WebSocket>,
    /// The stream-entry id (`seq`) of the last frame received, tracked so a
    /// reconnect can resume the turn's Valkey buffer exactly where it left off
    /// (SOUL §7/§12). `None` until the first seq-stamped frame arrives.
    last_seq: Option<String>,
}

/// Errors opening or driving the chat socket.
#[derive(Debug)]
pub enum ChatSocketError {
    /// The WebSocket handshake could not be initiated.
    Open(String),
    /// Sending a frame failed (socket closed / errored).
    Send(String),
}

impl std::fmt::Display for ChatSocketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatSocketError::Open(e) => write!(f, "failed to open chat socket: {e}"),
            ChatSocketError::Send(e) => write!(f, "failed to send chat message: {e}"),
        }
    }
}

impl std::error::Error for ChatSocketError {}

impl ChatSocket {
    /// Open the chat WebSocket, attaching `token` (if any) as a `?token=` query
    /// parameter so the API can authenticate the handshake (browsers can't set
    /// headers on a WS upgrade).
    pub fn connect(token: Option<&str>) -> Result<Self, ChatSocketError> {
        let url = chat_ws_url(token);
        let ws = WebSocket::open(&url).map_err(|e| ChatSocketError::Open(e.to_string()))?;
        let (sink, stream) = ws.split();
        Ok(Self {
            sink,
            stream,
            last_seq: None,
        })
    }

    /// The last resume cursor seen on this socket (the `seq` of the most recent
    /// frame), for reattaching after a reconnect.
    #[must_use]
    pub fn last_seq(&self) -> Option<String> {
        self.last_seq.clone()
    }

    /// (Re)attach to an in-flight turn's live stream (SOUL §7/§12): sends
    /// `{ attach, user_message_id, resume_after? }`. The server forwards the
    /// turn's replayable Valkey buffer from `resume_after` (or the start), so a
    /// reconnecting client resumes the exact stream with no gap.
    pub async fn send_attach(
        &mut self,
        conversation_id: &str,
        user_message_id: &str,
        resume_after: Option<&str>,
    ) -> Result<(), ChatSocketError> {
        let mut payload = serde_json::json!({
            "attach": conversation_id,
            "user_message_id": user_message_id,
        });
        if let Some(after) = resume_after {
            payload["resume_after"] = serde_json::Value::String(after.to_string());
        }
        self.sink
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| ChatSocketError::Send(e.to_string()))
    }

    /// Send one user turn as a JSON text frame.
    pub async fn send(&mut self, msg: &ClientChatMessage) -> Result<(), ChatSocketError> {
        let payload =
            serde_json::to_string(msg).map_err(|e| ChatSocketError::Send(e.to_string()))?;
        self.sink
            .send(Message::Text(payload))
            .await
            .map_err(|e| ChatSocketError::Send(e.to_string()))
    }

    /// Ask the server to stop the currently streaming turn (SOUL §12): sends
    /// `{ "stop": true, "conversation_id": … }` over the same socket. The server
    /// cancels the agent loop and ends the turn with a `message_done` flagged
    /// `stopped: true`. Naming the conversation lets a stop whose socket is NOT
    /// the one streaming (a reconnect / another pod behind the load balancer)
    /// still reach the streaming pod over the cross-pod control channel (SOUL
    /// §16 M7); a stop landing after the turn ended is a harmless no-op.
    pub async fn send_stop(
        &mut self,
        conversation_id: Option<&str>,
    ) -> Result<(), ChatSocketError> {
        let payload = match conversation_id {
            Some(c) => serde_json::json!({ "stop": true, "conversation_id": c }).to_string(),
            None => r#"{"stop":true}"#.to_string(),
        };
        self.sink
            .send(Message::Text(payload))
            .await
            .map_err(|e| ChatSocketError::Send(e.to_string()))
    }

    /// Reply to a guarded tool call's approval prompt (SOUL §19): sends
    /// `{ approval_id, approved }` over the *same* socket the turn is streaming on,
    /// which unblocks the paused dispatch server-side (allow on `true`, deny on
    /// `false`). Sent mid-turn — the server's socket loop reads it while streaming.
    pub async fn send_approval(
        &mut self,
        approval_id: &str,
        approved: bool,
        conversation_id: &str,
        user_message_id: &str,
    ) -> Result<(), ChatSocketError> {
        let payload = serde_json::json!({
            "approval_id": approval_id,
            "approved": approved,
            "conversation_id": conversation_id,
            "user_message_id": user_message_id,
        });
        self.sink
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| ChatSocketError::Send(e.to_string()))
    }

    /// Await the next inbound event, decoded into a [`StreamUpdate`].
    ///
    /// Returns `None` when the socket closes — including on a transport-level
    /// read error (an abnormal close like 1006, a dropped network): the socket
    /// is equally dead either way, and reporting it as closed routes the
    /// caller into its reconnect path instead of a terminal "stream error"
    /// (which is reserved for the server's own `error` frames). Undecodable
    /// binary frames surface as [`StreamUpdate::Ignore`].
    pub async fn next_update(&mut self) -> Option<StreamUpdate> {
        match self.stream.next().await? {
            Ok(Message::Text(text)) => {
                // Record the resume cursor (a transport-level `seq` the server
                // stamps on every forwarded frame) before decoding the update.
                if let Some(seq) = frame_seq(&text) {
                    self.last_seq = Some(seq);
                }
                Some(parse_frame(&text))
            }
            Ok(Message::Bytes(bytes)) => match std::str::from_utf8(&bytes) {
                Ok(text) => {
                    if let Some(seq) = frame_seq(text) {
                        self.last_seq = Some(seq);
                    }
                    Some(parse_frame(text))
                }
                Err(_) => Some(StreamUpdate::Ignore),
            },
            // A read error means the connection is gone (gloo yields errors
            // only for terminal conditions, never mid-stream blips).
            Err(_) => None,
        }
    }
}

/// The `ws(s)://…` root every WS endpoint URL builds on, derived from
/// [`api_base`] (`https`→`wss`, otherwise `ws`). Pure so the URL builders stay
/// unit-testable without web-sys.
fn ws_base() -> String {
    let base_owned = api_base();
    let base = base_owned.trim_end_matches('/');
    if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        // Already a ws(s):// or scheme-relative base.
        base.to_string()
    }
}

/// Build the absolute `ws(s)://…/ws/chat[?token=…]` URL for the chat endpoint.
#[must_use]
pub fn chat_ws_url(token: Option<&str>) -> String {
    let mut url = format!("{}{WS_CHAT_PATH}", ws_base());
    if let Some(tok) = token {
        if !tok.is_empty() {
            url.push_str("?token=");
            url.push_str(&encode_query_component(tok));
        }
    }
    url
}

/// Build the absolute `ws(s)://…/ws/speech[?token=…]` URL for the synthesis
/// endpoint (SOUL §7/§12).
#[must_use]
pub fn speech_ws_url(token: Option<&str>) -> String {
    let mut url = format!("{}{WS_SPEECH_PATH}", ws_base());
    if let Some(tok) = token {
        if !tok.is_empty() {
            url.push_str("?token=");
            url.push_str(&encode_query_component(tok));
        }
    }
    url
}

/// Build the `ws(s)://…/terminals/sessions/{id}/output[?token=…]` URL for the
/// read-only terminal output stream (SOUL §20). Same scheme derivation as
/// [`chat_ws_url`]; pure for unit testing.
#[must_use]
pub fn terminal_ws_url(session_id: &str, token: Option<&str>) -> String {
    let mut url = format!(
        "{}/terminals/sessions/{}/output",
        ws_base(),
        encode_query_component(session_id)
    );
    if let Some(tok) = token {
        if !tok.is_empty() {
            url.push_str("?token=");
            url.push_str(&encode_query_component(tok));
        }
    }
    url
}

/// A read-only stream of a terminal session's live output (SOUL §20). Binary PTY
/// frames are decoded through an [`AnsiStripper`] into readable text chunks; a
/// server-side error arrives as a text frame and is passed through verbatim.
pub struct TerminalSocket {
    ws: WebSocket,
    stripper: AnsiStripper,
}

impl TerminalSocket {
    /// Open the output WebSocket for `session_id`, attaching `token` as `?token=`.
    pub fn connect(session_id: &str, token: Option<&str>) -> Result<Self, ChatSocketError> {
        let url = terminal_ws_url(session_id, token);
        let ws = WebSocket::open(&url).map_err(|e| ChatSocketError::Open(e.to_string()))?;
        Ok(Self {
            ws,
            stripper: AnsiStripper::default(),
        })
    }

    /// Await the next readable output chunk. `None` when the socket closes.
    pub async fn next_chunk(&mut self) -> Option<String> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Bytes(bytes)) => {
                    let text = self.stripper.push(&bytes);
                    if !text.is_empty() {
                        return Some(text);
                    }
                }
                Ok(Message::Text(text)) => return Some(text),
                Err(_) => return None,
            }
        }
    }
}

/// One decoded inbound `/ws/speech` event (SOUL §7/§12). Audio bytes ride
/// binary frames bracketed by `Start`/`End`; `id` echoes the speak request's
/// correlation so the voice overlay can discard a reply it abandoned.
#[derive(Debug)]
pub enum SpeechEvent {
    /// Synthesis succeeded; binary chunks follow. `content_type` is what the
    /// provider **actually** produced — a model may ignore the requested format
    /// and answer in another container, so playback decodes by sniffing, never
    /// by assumption.
    Start {
        id: Option<u64>,
        content_type: String,
    },
    /// One binary audio chunk of the current reply.
    Chunk(Vec<u8>),
    /// The current reply's audio is complete.
    End { id: Option<u64> },
    /// The request failed server-side; the socket stays usable.
    Error { id: Option<u64>, message: String },
}

/// The server's `/ws/speech` text control frames (`speech_start`/`speech_end`/
/// `error`); unknown types decode to `Unknown` and are skipped, so the protocol
/// can grow without breaking older clients.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SpeechControlFrame {
    SpeechStart {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        content_type: String,
    },
    SpeechEnd {
        #[serde(default)]
        id: Option<u64>,
    },
    Error {
        #[serde(default)]
        id: Option<u64>,
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Unknown,
}

/// The voice overlay's synthesis channel (SOUL §7/§12): held open for the
/// overlay's whole session, one JSON speak frame per assistant reply out,
/// [`SpeechEvent`]s in.
pub struct SpeechSocket {
    sink: SplitSink<WebSocket, Message>,
    stream: SplitStream<WebSocket>,
}

impl SpeechSocket {
    /// Open the speech WebSocket, attaching `token` (if any) as `?token=` —
    /// browsers can't set headers on a WS upgrade.
    pub fn connect(token: Option<&str>) -> Result<Self, ChatSocketError> {
        let url = speech_ws_url(token);
        let ws = WebSocket::open(&url).map_err(|e| ChatSocketError::Open(e.to_string()))?;
        let (sink, stream) = ws.split();
        Ok(Self { sink, stream })
    }

    /// Request synthesis of `input`, tagged with the caller's correlation `id`.
    /// Model/voice/format stay server-resolved (the user's speech settings), so
    /// the overlay speaks with whatever the settings pickers chose.
    pub async fn speak(&mut self, id: u64, input: &str) -> Result<(), ChatSocketError> {
        let payload = serde_json::json!({ "id": id, "input": input });
        self.sink
            .send(Message::Text(payload.to_string()))
            .await
            .map_err(|e| ChatSocketError::Send(e.to_string()))
    }

    /// Await the next decoded [`SpeechEvent`]. `None` when the socket closes;
    /// unknown control frames are skipped.
    pub async fn next_event(&mut self) -> Option<SpeechEvent> {
        loop {
            return match self.stream.next().await? {
                Ok(Message::Bytes(bytes)) => Some(SpeechEvent::Chunk(bytes)),
                Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                    Ok(SpeechControlFrame::SpeechStart { id, content_type }) => {
                        Some(SpeechEvent::Start { id, content_type })
                    }
                    Ok(SpeechControlFrame::SpeechEnd { id }) => Some(SpeechEvent::End { id }),
                    Ok(SpeechControlFrame::Error { id, message }) => {
                        Some(SpeechEvent::Error { id, message })
                    }
                    Ok(SpeechControlFrame::Unknown) | Err(_) => continue,
                },
                Err(e) => Some(SpeechEvent::Error {
                    id: None,
                    message: e.to_string(),
                }),
            };
        }
    }
}

/// Percent-encode a value for safe inclusion in a query string. Encodes
/// everything outside the unreserved set; ASCII-only, allocation-light.
fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0f));
            }
        }
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_has_ws_scheme_and_path() {
        let url = chat_ws_url(None);
        assert!(
            url.starts_with("ws://") || url.starts_with("wss://"),
            "{url}"
        );
        assert!(url.ends_with("/ws/chat"), "{url}");
    }

    #[test]
    fn url_appends_token() {
        let url = chat_ws_url(Some("tok 1+2"));
        assert!(url.contains("/ws/chat?token="));
        assert!(url.contains("tok%201%2B2"), "{url}");
    }

    #[test]
    fn empty_token_no_query() {
        assert!(!chat_ws_url(Some("")).contains('?'));
    }

    #[test]
    fn speech_url_has_ws_scheme_and_path() {
        let url = speech_ws_url(Some("t"));
        assert!(
            url.starts_with("ws://") || url.starts_with("wss://"),
            "{url}"
        );
        assert!(url.contains("/ws/speech?token=t"), "{url}");
    }

    #[test]
    fn speech_control_frames_decode() {
        let f: SpeechControlFrame =
            serde_json::from_str(r#"{"type":"speech_start","id":3,"content_type":"audio/ogg"}"#)
                .unwrap();
        assert!(matches!(
            f,
            SpeechControlFrame::SpeechStart { id: Some(3), ref content_type }
                if content_type == "audio/ogg"
        ));
        let f: SpeechControlFrame = serde_json::from_str(r#"{"type":"speech_end"}"#).unwrap();
        assert!(matches!(f, SpeechControlFrame::SpeechEnd { id: None }));
        let f: SpeechControlFrame =
            serde_json::from_str(r#"{"type":"error","message":"boom"}"#).unwrap();
        assert!(matches!(f, SpeechControlFrame::Error { ref message, .. } if message == "boom"));
        // A frame type this client doesn't know yet is skippable, not an error.
        let f: SpeechControlFrame = serde_json::from_str(r#"{"type":"speech_meta"}"#).unwrap();
        assert!(matches!(f, SpeechControlFrame::Unknown));
    }
}
