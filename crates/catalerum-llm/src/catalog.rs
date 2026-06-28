//! Catalog discovery over llmleaf (SOUL §7): the model catalog
//! (`GET /v1/models`) and the TTS voice list (`GET /v1/audio/voices`).
//!
//! These are read-only metadata endpoints — a UI can populate a model picker or a
//! voice selector without hard-coding the gateway's offerings. The SDK's wire
//! shapes are mapped to small, provider-neutral types so callers never depend on
//! `llmleaf-client` directly.

use serde::{Deserialize, Serialize};

use catalerum_core::error::Result;

use crate::client::{map_sdk_error, OpenRouterClient};

/// Which class of models to list (`type` filter on `GET /v1/models`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    /// Every model the gateway exposes (no filter).
    #[default]
    All,
    /// Chat / completion models.
    Chat,
    /// Text-to-speech models.
    Tts,
    /// Speech-to-text models.
    Stt,
    /// Embedding models.
    Embedding,
}

impl ModelKind {
    /// The SDK filter, or `None` for [`ModelKind::All`] (omit the `type` param).
    fn to_sdk(self) -> Option<llmleaf_client::ModelType> {
        match self {
            ModelKind::All => None,
            ModelKind::Chat => Some(llmleaf_client::ModelType::Llm),
            ModelKind::Tts => Some(llmleaf_client::ModelType::Tts),
            ModelKind::Stt => Some(llmleaf_client::ModelType::Stt),
            ModelKind::Embedding => Some(llmleaf_client::ModelType::Embedding),
        }
    }
}

/// One model in the gateway catalog (mapped from llmleaf's `ModelEntry`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model id / routing alias to pass as `ChatRequest::model`.
    pub id: String,
    /// Human-friendly display name.
    pub name: String,
    /// Free-text description.
    pub description: String,
    /// Max context window in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Accepted input modalities (e.g. `text`, `image`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    /// Produced output modalities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    /// Prompt price per token (decimal string, USD), when priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_price: Option<String>,
    /// Completion price per token (decimal string, USD), when priced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_price: Option<String>,
    /// Request parameters this model supports (e.g. `tools`, `reasoning`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_parameters: Vec<String>,
}

impl ModelInfo {
    fn from_entry(entry: llmleaf_client::ModelEntry) -> Self {
        // Top-level `context_length`, else the top-provider's.
        let context_length = entry
            .context_length
            .or_else(|| entry.top_provider.as_ref().and_then(|t| t.context_length));
        let (input_modalities, output_modalities) = entry
            .architecture
            .map(|a| (a.input_modalities, a.output_modalities))
            .unwrap_or_default();
        let (prompt_price, completion_price) = entry
            .pricing
            .map(|p| (Some(p.prompt), Some(p.completion)))
            .unwrap_or_default();
        ModelInfo {
            id: entry.id,
            name: entry.name,
            description: entry.description,
            context_length,
            input_modalities,
            output_modalities,
            prompt_price,
            completion_price,
            supported_parameters: entry.supported_parameters,
        }
    }
}

/// A TTS voice offered by a speech model (mapped from llmleaf's `Voice`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceInfo {
    /// Voice id to pass as `SpeechRequest::voice`.
    pub id: String,
    /// Display name, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Languages this voice supports (BCP-47 / ISO codes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
}

impl From<llmleaf_client::Voice> for VoiceInfo {
    fn from(v: llmleaf_client::Voice) -> Self {
        VoiceInfo {
            id: v.id,
            name: v.name,
            languages: v.languages,
        }
    }
}

impl OpenRouterClient {
    /// List the gateway's model catalog, optionally filtered by [`ModelKind`] and
    /// a substring `search` over the catalog (llmleaf `GET /v1/models`).
    pub async fn list_models(
        &self,
        kind: ModelKind,
        search: Option<&str>,
    ) -> Result<Vec<ModelInfo>> {
        let resp = self
            .sdk()?
            .list_models(kind.to_sdk(), search)
            .await
            .map_err(map_sdk_error)?;
        Ok(resp.data.into_iter().map(ModelInfo::from_entry).collect())
    }

    /// List the voices a TTS `model` exposes (llmleaf `GET /v1/audio/voices`).
    pub async fn voices(&self, model: &str) -> Result<Vec<VoiceInfo>> {
        let resp = self.sdk()?.voices(model).await.map_err(map_sdk_error)?;
        Ok(resp.voices.into_iter().map(VoiceInfo::from).collect())
    }
}
