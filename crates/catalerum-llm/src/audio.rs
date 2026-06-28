//! The llmleaf audio client adapters (SOUL §7): text-to-speech and
//! speech-to-text.
//!
//! Both implementations delegate to `llmleaf-client`, keeping catalerum's core
//! audio traits decoupled from the concrete HTTP wire.

use async_trait::async_trait;

use catalerum_core::audio::{
    SpeechAudio, SpeechRequest, TranscriptionRequest, TranscriptionResponse,
};
use catalerum_core::error::Result;
use catalerum_core::provider::{SpeechSynthesizer, Transcriber};

use crate::client::{map_sdk_error, OpenRouterClient};

#[async_trait]
impl SpeechSynthesizer for OpenRouterClient {
    async fn synthesize(&self, request: SpeechRequest) -> Result<SpeechAudio> {
        let sdk_request = llmleaf_client::SpeechRequest {
            model: request.model,
            input: request.input,
            voice: request.voice,
            response_format: request.response_format,
            speed: request.speed,
            extra: None,
        };

        let (data, content_type) = self
            .sdk()?
            .speech(sdk_request)
            .await
            .map_err(map_sdk_error)?;

        Ok(SpeechAudio {
            content_type,
            data: data.to_vec(),
        })
    }
}

#[async_trait]
impl Transcriber for OpenRouterClient {
    async fn transcribe(&self, request: TranscriptionRequest) -> Result<TranscriptionResponse> {
        let mut sdk_request = llmleaf_client::TranscriptionRequest::new(request.model);
        sdk_request.language = request.language;
        sdk_request.prompt = request.prompt;
        sdk_request.temperature = request.temperature;
        // Preserve the old adapter's behavior: request structured metadata when
        // the provider can report language and duration.
        sdk_request.response_format = Some("verbose_json".to_string());

        let response = self
            .sdk()?
            .transcribe(sdk_request, request.filename, request.audio)
            .await
            .map_err(map_sdk_error)?;

        Ok(match response {
            llmleaf_client::Transcription::Json(json) => TranscriptionResponse {
                text: json.text,
                language: json.language,
                duration: json.duration,
            },
            llmleaf_client::Transcription::Text(text) => TranscriptionResponse {
                text,
                language: None,
                duration: None,
            },
        })
    }
}
