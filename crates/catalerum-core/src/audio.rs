//! Provider-agnostic audio shapes: text-to-speech (TTS) and speech-to-text
//! (STT), SOUL §7.
//!
//! Generated through the **same llmleaf proxy** as chat and embeddings (llmleaf
//! is multi-modal over one OpenAI-compatible endpoint). These types are what the
//! [`SpeechSynthesizer`](crate::provider::SpeechSynthesizer) and
//! [`Transcriber`](crate::provider::Transcriber) traits consume and produce;
//! `catalerum-llm` maps them to/from llmleaf's `POST /v1/audio/speech` and
//! `POST /v1/audio/transcriptions`.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Text-to-speech (TTS)
// ---------------------------------------------------------------------------

/// A text-to-speech request (`/v1/audio/speech`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeechRequest {
    /// Model id (or an llmleaf routing alias).
    pub model: String,
    /// The text to speak.
    pub input: String,
    /// Provider voice id (e.g. `alloy`). Required by the OpenAI dialect.
    pub voice: String,
    /// Container/codec: `mp3` (default), `opus`, `aac`, `flac`, `wav`, `pcm`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    /// Playback speed multiplier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f32>,
}

impl SpeechRequest {
    /// Speak `input` with `model` and `voice` (provider-default format).
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        input: impl Into<String>,
        voice: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            input: input.into(),
            voice: voice.into(),
            response_format: None,
            speed: None,
        }
    }

    /// Request a specific audio container/codec.
    #[must_use]
    pub fn with_format(mut self, format: impl Into<String>) -> Self {
        self.response_format = Some(format.into());
        self
    }

    /// Set the playback speed multiplier.
    #[must_use]
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = Some(speed);
        self
    }
}

/// Synthesized audio: the raw bytes plus the MIME type the provider produced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechAudio {
    /// The audio MIME type (e.g. `audio/mpeg`).
    pub content_type: String,
    /// The complete audio payload.
    pub data: Vec<u8>,
}

impl SpeechAudio {
    /// Number of audio bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True if no audio bytes were returned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Speech-to-text (STT)
// ---------------------------------------------------------------------------

/// A speech-to-text request (`/v1/audio/transcriptions`). The audio rides
/// in-band as bytes; `filename` carries the container hint (its extension).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TranscriptionRequest {
    /// Model id (or an llmleaf routing alias).
    pub model: String,
    /// Raw audio bytes. Skipped from serialization so logs/events never dump the
    /// blob (transparent about transformations, not about leaking megabytes).
    #[serde(skip)]
    pub audio: Vec<u8>,
    /// Upload filename — its extension hints the container/codec.
    pub filename: String,
    /// ISO-639-1 language hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Optional prompt to bias decoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Sampling temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

impl TranscriptionRequest {
    /// Transcribe `audio` (named `filename`) with `model`.
    #[must_use]
    pub fn new(model: impl Into<String>, audio: Vec<u8>, filename: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            audio,
            filename: filename.into(),
            language: None,
            prompt: None,
            temperature: None,
        }
    }

    /// Add an ISO-639-1 language hint.
    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Add a decoding-bias prompt.
    #[must_use]
    pub fn with_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.prompt = Some(prompt.into());
        self
    }
}

/// The result of a [`TranscriptionRequest`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TranscriptionResponse {
    /// The transcript text.
    pub text: String,
    /// Detected/declared language, when the provider reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Audio duration in seconds, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_builders_set_fields() {
        let req = SpeechRequest::new("tts", "hello", "alloy")
            .with_format("opus")
            .with_speed(1.25);
        assert_eq!(req.voice, "alloy");
        assert_eq!(req.response_format.as_deref(), Some("opus"));
        assert_eq!(req.speed, Some(1.25));
    }

    #[test]
    fn speech_audio_len_and_empty() {
        let a = SpeechAudio {
            content_type: "audio/mpeg".into(),
            data: vec![1, 2, 3],
        };
        assert_eq!(a.len(), 3);
        assert!(!a.is_empty());
        assert!(SpeechAudio::default().is_empty());
    }

    #[test]
    fn transcription_audio_is_not_serialized() {
        let req =
            TranscriptionRequest::new("whisper", vec![0xde, 0xad], "clip.wav").with_language("en");
        let json = serde_json::to_string(&req).unwrap();
        // The audio blob must never leak into serialized form.
        assert!(!json.contains("audio"));
        assert!(json.contains("clip.wav"));
        assert!(json.contains("\"en\""));
    }

    #[test]
    fn transcription_response_round_trips() {
        let resp = TranscriptionResponse {
            text: "hello world".into(),
            language: Some("en".into()),
            duration: Some(1.5),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: TranscriptionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }
}
