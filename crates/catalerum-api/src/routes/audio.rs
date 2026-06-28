//! Speech audio REST + WS (SOUL §7) — the chat composer's microphone button and
//! the voice-conversation overlay's speaker.
//!
//! `POST /audio/transcriptions` transcribes a recorded-audio request body to text
//! and returns it. Unlike the `speech_to_text` tool (which reads an already-stored
//! object by key), this transcribes bytes straight from the request: the browser
//! records with `MediaRecorder`, POSTs the blob, and drops the returned transcript
//! into the composer.
//!
//! `GET /ws/speech` is the synthesis direction: a WebSocket the voice overlay
//! holds open for its session, sending one JSON [`SpeakFrame`] per assistant reply
//! and receiving `speech_start` (with the provider's **actual** content type — a
//! model may ignore the requested format and answer in another container) →
//! binary audio chunks → `speech_end`. A per-request client `id` is echoed on
//! every frame so a client can discard a stale reply it no longer wants. A
//! WebSocket rather than a plain POST so the session pays the handshake once and
//! the frame protocol can grow chunked/streaming synthesis without a new surface.
//!
//! Both are authenticated and **capability-gated** (SOUL §19): STT/TTS burn LLM
//! provider quota, so they require `conversation:write` — the authority the chat
//! turn they serve already requires. A Viewer or an empty grant-scoped token
//! cannot spend quota here. Each resolves the caller's per-user model override
//! (→ the `[llm]` config default), exactly as the `speech_to_text`/
//! `text_to_speech` tools do, so both honour the settings picks.

use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{any, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;

use catalerum_core::audio::{SpeechRequest, TranscriptionRequest};
use catalerum_core::capability::Action;
use catalerum_core::provider::{SpeechSynthesizer, Transcriber};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Cap the buffered audio body. Whisper-class endpoints reject bodies over ~25 MiB,
/// and a browser Opus recording is only a few KB/s, so this is generous headroom
/// while still refusing an unbounded-upload OOM before the bytes are read (axum's
/// global default is a too-small 2 MiB for a minute-long clip).
const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

/// Cap a synthesis request's text. OpenAI-dialect TTS endpoints reject inputs
/// over ~4096 chars; this is headroom above that so the provider (not us) stays
/// the authority, while still refusing a pathological megabyte of text.
const MAX_SPEECH_INPUT_CHARS: usize = 8 * 1024;

/// Outbound binary-frame size for synthesized audio. Small enough to interleave
/// with the browser's event loop, large enough that a typical reply is a handful
/// of frames.
const SPEECH_CHUNK_BYTES: usize = 64 * 1024;
/// A recording/result only bridges short mobile disconnects; it is not durable
/// user storage and expires automatically from Valkey.
const TRANSCRIPTION_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const TRANSCRIPTION_ID_HEADER: &str = "x-catalerum-transcription-id";

/// Mount the speech-to-text route and the synthesis WebSocket.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/audio/transcriptions", post(transcribe))
        .route("/ws/speech", any(ws_speech))
        .layer(DefaultBodyLimit::max(MAX_AUDIO_BYTES))
}

/// The transcript plus the metadata the provider reported (mirrors the
/// `speech_to_text` tool's JSON, minus the storage key).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TranscriptionResult {
    /// The recognized text.
    pub text: String,
    /// Detected/declared language, when the provider reports it.
    pub language: Option<String>,
    /// Audio duration in seconds, when reported.
    pub duration: Option<f32>,
    /// The STT model the transcription actually ran through.
    pub model: String,
}

async fn transcribe(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<TranscriptionResult>> {
    // Quota-spend gate (SOUL §19): STT is a paid LLM call serving the chat
    // composer — require the chat turn's own write authority.
    auth.require(Action::Write, "conversation")?;
    let p = auth.principal();
    let request_id = transcription_request_id(&headers)?;
    let result_key = request_id.as_ref().map(|id| {
        format!(
            "cat:transcription:result:{}:{}:{id}",
            p.workspace_id, p.user_id
        )
    });
    let audio_key = request_id.as_ref().map(|id| {
        format!(
            "cat:transcription:audio:{}:{}:{id}",
            p.workspace_id, p.user_id
        )
    });
    if let Some(key) = &result_key {
        if let Ok(Some(bytes)) = state.bus().registry().lookup(key).await {
            if let Ok(cached) = serde_json::from_slice::<TranscriptionResult>(&bytes) {
                return Ok(Json(cached));
            }
        }
    }
    // Effective STT model: the caller's per-user `transcription_model` override →
    // the `[llm]` config default (the same resolution `speech_to_text` uses, so the
    // mic transcribes with whatever the settings picker chose).
    let model = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await
        .ok()
        .and_then(|s| s.transcription_model)
        .unwrap_or_else(|| state.config().llm.transcription_model.clone());
    // The filename extension is the container/codec hint the transcription endpoint
    // keys on; derive it from the request Content-Type (a `MediaRecorder` emits
    // webm/ogg/mp4 depending on the browser).
    let (audio, filename) = if body.is_empty() {
        let Some(key) = &audio_key else {
            return Err(ApiError::bad_request("empty audio body"));
        };
        let cached = state
            .bus()
            .registry()
            .lookup(key)
            .await
            .ok()
            .flatten()
            .ok_or(ApiError::NotFound)?;
        decode_cached_audio(&cached).ok_or(ApiError::NotFound)?
    } else {
        let filename = filename_for(&headers);
        let audio = body.to_vec();
        if let Some(key) = &audio_key {
            let _ = state
                .bus()
                .registry()
                .announce(
                    key,
                    encode_cached_audio(&filename, &audio),
                    TRANSCRIPTION_CACHE_TTL,
                )
                .await;
        }
        (audio, filename)
    };
    // Fence concurrent retries of the same recording. A retry whose original
    // request is still transcribing waits for its cached result instead of
    // starting a second provider call; bus failure degrades to uncoordinated work.
    let mut cache_guard = None;
    if let (Some(id), Some(key)) = (request_id.as_deref(), result_key.as_deref()) {
        let resource = format!("transcription:{}:{}:{id}", p.workspace_id, p.user_id);
        for _ in 0..80 {
            match state
                .bus()
                .lock()
                .try_acquire(&resource, Duration::from_secs(120))
                .await
            {
                Ok(Some(guard)) => {
                    cache_guard = Some(guard);
                    break;
                }
                Ok(None) => {
                    if let Ok(Some(bytes)) = state.bus().registry().lookup(key).await {
                        if let Ok(cached) = serde_json::from_slice::<TranscriptionResult>(&bytes) {
                            return Ok(Json(cached));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(_) => break,
            }
        }
        // The holder could have completed between our final lookup and lock
        // acquisition; check once more before spending provider tokens.
        if cache_guard.is_some() {
            if let Ok(Some(bytes)) = state.bus().registry().lookup(key).await {
                if let Ok(cached) = serde_json::from_slice::<TranscriptionResult>(&bytes) {
                    if let Some(guard) = cache_guard.take() {
                        let _ = state.bus().lock().release(&guard).await;
                    }
                    return Ok(Json(cached));
                }
            }
        }
    }
    let request = TranscriptionRequest::new(&model, audio, filename);
    let response = match state.llm().transcribe(request).await {
        Ok(response) => response,
        Err(e) => {
            if let Some(guard) = cache_guard.take() {
                let _ = state.bus().lock().release(&guard).await;
            }
            return Err(e.into());
        }
    };
    let result = TranscriptionResult {
        text: response.text,
        language: response.language,
        duration: response.duration,
        model,
    };
    if let Some(key) = &result_key {
        if let Ok(bytes) = serde_json::to_vec(&result) {
            let _ = state
                .bus()
                .registry()
                .announce(key, bytes, TRANSCRIPTION_CACHE_TTL)
                .await;
        }
    }
    if let Some(guard) = cache_guard.take() {
        let _ = state.bus().lock().release(&guard).await;
    }
    Ok(Json(result))
}

fn transcription_request_id(headers: &HeaderMap) -> ApiResult<Option<String>> {
    let Some(raw) = headers.get(TRANSCRIPTION_ID_HEADER) else {
        return Ok(None);
    };
    let id = raw
        .to_str()
        .map_err(|_| ApiError::bad_request("invalid transcription id"))?;
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ApiError::bad_request("invalid transcription id"));
    }
    Ok(Some(id.to_string()))
}

/// Compact binary cache envelope: filename length (u16 BE), filename, raw audio.
fn encode_cached_audio(filename: &str, audio: &[u8]) -> Vec<u8> {
    let name = filename.as_bytes();
    let len = u16::try_from(name.len()).unwrap_or(0);
    let mut out = Vec::with_capacity(2 + name.len() + audio.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(name);
    out.extend_from_slice(audio);
    out
}

fn decode_cached_audio(bytes: &[u8]) -> Option<(Vec<u8>, String)> {
    let head: [u8; 2] = bytes.get(..2)?.try_into().ok()?;
    let len = usize::from(u16::from_be_bytes(head));
    let filename = std::str::from_utf8(bytes.get(2..2 + len)?)
        .ok()?
        .to_string();
    let audio = bytes.get(2 + len..)?.to_vec();
    (!audio.is_empty()).then_some((audio, filename))
}

/// Map the request `Content-Type` to an `audio.<ext>` filename whose extension is
/// the container hint the transcription endpoint reads. Unknown/absent types fall
/// back to `webm` — the default a browser `MediaRecorder` produces.
fn filename_for(headers: &HeaderMap) -> String {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    // The subtype (before any `;codecs=…` parameter) is what identifies the
    // container; match the tokens a `MediaRecorder` or a plain upload can set.
    let ext = if ct.contains("webm") {
        "webm"
    } else if ct.contains("ogg") || ct.contains("opus") {
        "ogg"
    } else if ct.contains("mp4") || ct.contains("m4a") || ct.contains("aac") {
        "m4a"
    } else if ct.contains("wav") || ct.contains("wave") {
        "wav"
    } else if ct.contains("mpeg") || ct.contains("mp3") {
        "mp3"
    } else if ct.contains("flac") {
        "flac"
    } else {
        "webm"
    };
    format!("audio.{ext}")
}

/// One synthesis request over `/ws/speech`: speak `input` and stream the audio
/// back. `id` is an opaque client correlation echoed on every reply frame — the
/// voice overlay bumps it per request and discards frames for an id it has
/// abandoned (a skipped reply whose audio was still in flight).
#[derive(Debug, Deserialize)]
struct SpeakFrame {
    /// The text to speak.
    input: String,
    /// Client request correlation, echoed back verbatim.
    #[serde(default)]
    id: Option<u64>,
    /// TTS model override; omitted → the caller's `speech_model` setting → the
    /// `[llm].speech_model` config default.
    #[serde(default)]
    model: Option<String>,
    /// Voice override; omitted → the caller's `speech_voice` setting → the
    /// `[llm].speech_voice` config default.
    #[serde(default)]
    voice: Option<String>,
    /// Requested container/codec (mp3 [default]/opus/aac/flac/wav). A **request**
    /// only — the provider may answer in another container; `speech_start`
    /// carries the actual content type.
    #[serde(default)]
    format: Option<String>,
    /// Playback speed multiplier.
    #[serde(default)]
    speed: Option<f32>,
}

/// Outbound `/ws/speech` text frames. The audio itself rides *binary* frames
/// between `speech_start` and `speech_end`; these bracket and correlate them.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SpeechFrame {
    /// Synthesis succeeded; binary audio chunks follow. `content_type` is what
    /// the provider actually produced (a model may ignore the requested format),
    /// so the client must decode by sniffing/`content_type`, never by assumption.
    SpeechStart {
        id: Option<u64>,
        content_type: String,
    },
    /// All of this request's audio chunks have been sent.
    SpeechEnd { id: Option<u64> },
    /// The request failed (bad frame, oversized input, provider error). The
    /// socket stays open — the client may retry or move on.
    Error { id: Option<u64>, message: String },
}

/// The requested-format whitelist. `pcm` is deliberately excluded: it is
/// headerless, and the browser's `decodeAudioData` (the voice overlay's decoder)
/// cannot sniff it.
fn speech_format(requested: Option<&str>) -> Result<&'static str, String> {
    match requested {
        None => Ok("mp3"),
        Some("mp3") => Ok("mp3"),
        Some("opus") => Ok("opus"),
        Some("aac") => Ok("aac"),
        Some("flac") => Ok("flac"),
        Some("wav") => Ok("wav"),
        Some(other) => Err(format!("unsupported speech format {other:?}")),
    }
}

/// The content type reported to the client: the provider's own when it named
/// one, else a fallback derived from the *requested* format (best effort — a
/// provider that both omits the type and switches container is undetectable
/// here; the browser decoder sniffs the bytes anyway).
fn speech_content_type(provider: &str, format: &str) -> String {
    if !provider.is_empty() {
        return provider.to_string();
    }
    match format {
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        _ => "audio/mpeg",
    }
    .to_string()
}

/// The WS upgrade handler for `/ws/speech`. Authentication runs *before* the
/// upgrade via the [`Auth`] extractor (the `access_token`/`token` query param or
/// an `Authorization` header — same as `/ws/chat`); an unauthenticated handshake
/// is rejected with `401`.
async fn ws_speech(
    State(state): State<AppState>,
    auth: Auth,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    // Quota-spend gate (SOUL §19), checked pre-upgrade: TTS is a paid LLM call
    // serving the voice overlay — require the chat turn's own write authority.
    auth.require(Action::Write, "conversation")?;
    let principal = auth.principal();
    Ok(ws.on_upgrade(move |socket| handle_speech_socket(socket, state, principal)))
}

/// Drive the authenticated synthesis socket: one [`SpeakFrame`] in → one
/// `speech_start` + binary chunks + `speech_end` out, strictly in order, until
/// the client closes. Errors answer with an [`SpeechFrame::Error`] and keep the
/// socket alive.
async fn handle_speech_socket(
    socket: WebSocket,
    state: AppState,
    principal: catalerum_iam::Principal,
) {
    let (mut sink, mut stream) = socket.split();
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => break,
        };
        let frame: SpeakFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                let err = SpeechFrame::Error {
                    id: None,
                    message: format!("bad speak frame: {e}"),
                };
                if send_speech_frame(&mut sink, &err).await.is_err() {
                    break;
                }
                continue;
            }
        };
        let id = frame.id;
        match synthesize_speech(&state, &principal, frame).await {
            Ok((content_type, data)) => {
                let start = SpeechFrame::SpeechStart { id, content_type };
                if send_speech_frame(&mut sink, &start).await.is_err() {
                    break;
                }
                let mut closed = false;
                for chunk in data.chunks(SPEECH_CHUNK_BYTES) {
                    if sink
                        .send(WsMessage::Binary(chunk.to_vec().into()))
                        .await
                        .is_err()
                    {
                        closed = true;
                        break;
                    }
                }
                if closed
                    || send_speech_frame(&mut sink, &SpeechFrame::SpeechEnd { id })
                        .await
                        .is_err()
                {
                    break;
                }
            }
            Err(message) => {
                let err = SpeechFrame::Error { id, message };
                if send_speech_frame(&mut sink, &err).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Serialize + send one text control frame; `Err` means the socket is gone.
async fn send_speech_frame(
    sink: &mut futures::stream::SplitSink<WebSocket, WsMessage>,
    frame: &SpeechFrame,
) -> Result<(), ()> {
    let json = serde_json::to_string(frame).map_err(|_| ())?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|_| ())
}

/// Validate one speak request and run it through the effective TTS model +
/// voice (explicit override → per-user `speech_model`/`speech_voice` setting →
/// the `[llm]` config defaults — the `text_to_speech` tool's exact resolution).
/// Returns the reported content type + the complete audio bytes.
async fn synthesize_speech(
    state: &AppState,
    principal: &catalerum_iam::Principal,
    frame: SpeakFrame,
) -> Result<(String, Vec<u8>), String> {
    if frame.input.trim().is_empty() {
        return Err("empty speech input".to_string());
    }
    if frame.input.chars().count() > MAX_SPEECH_INPUT_CHARS {
        return Err(format!(
            "speech input too long (max {MAX_SPEECH_INPUT_CHARS} characters)"
        ));
    }
    let format = speech_format(frame.format.as_deref())?;
    // One settings lookup covers both the model and the voice fallback.
    let settings = match (&frame.model, &frame.voice) {
        (Some(_), Some(_)) => None,
        _ => state
            .store()
            .llm_settings()
            .get(principal.workspace_id, principal.user_id)
            .await
            .ok(),
    };
    let model = frame
        .model
        .or_else(|| settings.as_ref().and_then(|s| s.speech_model.clone()))
        .unwrap_or_else(|| state.config().llm.speech_model.clone());
    let voice = frame
        .voice
        .or_else(|| settings.as_ref().and_then(|s| s.speech_voice.clone()))
        .unwrap_or_else(|| state.config().llm.speech_voice.clone());
    let mut request = SpeechRequest::new(&model, &frame.input, &voice).with_format(format);
    if let Some(speed) = frame.speed {
        request = request.with_speed(speed);
    }
    let audio = state
        .llm()
        .synthesize(request)
        .await
        .map_err(|e| format!("speech synthesis failed: {e}"))?;
    if audio.data.is_empty() {
        return Err("the speech provider returned no audio".to_string());
    }
    Ok((speech_content_type(&audio.content_type, format), audio.data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_whitelist_accepts_known_rejects_pcm() {
        assert_eq!(speech_format(None).unwrap(), "mp3");
        for ok in ["mp3", "opus", "aac", "flac", "wav"] {
            assert_eq!(speech_format(Some(ok)).unwrap(), ok);
        }
        assert!(speech_format(Some("pcm")).is_err());
        assert!(speech_format(Some("midi")).is_err());
    }

    #[test]
    fn content_type_prefers_provider_then_format() {
        assert_eq!(speech_content_type("audio/ogg", "mp3"), "audio/ogg");
        assert_eq!(speech_content_type("", "mp3"), "audio/mpeg");
        assert_eq!(speech_content_type("", "wav"), "audio/wav");
        assert_eq!(speech_content_type("", "opus"), "audio/ogg");
    }

    #[test]
    fn speak_frame_parses_minimal_and_full() {
        let f: SpeakFrame = serde_json::from_str(r#"{"input":"hi"}"#).unwrap();
        assert_eq!(f.input, "hi");
        assert!(f.id.is_none() && f.model.is_none() && f.voice.is_none());
        let f: SpeakFrame = serde_json::from_str(
            r#"{"input":"hi","id":7,"model":"m","voice":"v","format":"wav","speed":1.25}"#,
        )
        .unwrap();
        assert_eq!(f.id, Some(7));
        assert_eq!(f.format.as_deref(), Some("wav"));
    }

    #[test]
    fn cached_audio_envelope_round_trips_without_expanding_bytes() {
        let audio = vec![0, 1, 2, 127, 255];
        let encoded = encode_cached_audio("audio.webm", &audio);
        assert_eq!(encoded.len(), 2 + "audio.webm".len() + audio.len());
        assert_eq!(
            decode_cached_audio(&encoded),
            Some((audio, "audio.webm".to_string()))
        );
        assert!(decode_cached_audio(&[0]).is_none());
    }
}
