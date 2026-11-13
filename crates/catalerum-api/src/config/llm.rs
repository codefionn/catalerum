//! The LLM gateway and memory curation.

use serde::{Deserialize, Serialize};

use super::Secret;

/// Background memory auto-curation (SOUL §22). When `enabled` (default off), each
/// chat turn enqueues an `extract_memories` job and the binary attaches a curate
/// context to the worker. `model` overrides the LLM used to extract (empty →
/// fall back to `[llm].default_model`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CurationConfig {
    /// Whether to mine conversations for durable memories.
    pub enabled: bool,
    /// Extraction model; empty falls back to `[llm].default_model`.
    pub model: String,
}

/// llmleaf endpoint (SOUL §7). llmleaf is multi-modal: chat, embeddings, TTS, and
/// STT all go through this one OpenRouter-shaped endpoint (`base_url` + `api_key`);
/// only the per-modality model differs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    /// llmleaf origin, **without** the `/v1` version prefix (e.g.
    /// `http://localhost:8088`). The `llmleaf-client` SDK appends the versioned
    /// path itself (`/v1/responses`, `/v1/embeddings`, …), so a `…/v1`
    /// here double-prefixes (`/v1/v1/…`) and every call 404s.
    pub base_url: String,
    pub api_key: Secret,
    /// Bearer accepted by the loopback llmleaf topology pull endpoint. Empty
    /// disables that machine endpoint; inject it in all-in-one/operator setups.
    pub control_token: Secret,
    /// Enable Catalerum's dynamic llmleaf provider/route control plane. Off by
    /// default: ordinary deployments keep llmleaf topology in its own immutable
    /// configuration. The all-in-one profile opts in so it can offer guided
    /// provider and route management from the workbench.
    pub control_plane_enabled: bool,
    /// Model for chat (`/responses`).
    pub default_model: String,
    /// Model for typed decisions (`/decisions`); unset requires a caller override.
    pub decision_model: Option<String>,
    /// Model for embeddings (`/embeddings`); the vectors feed Qdrant (SOUL §6.4).
    pub embedding_model: String,
    /// Optional embedding output dimensionality (Matryoshka truncation); `None`
    /// (or `0` in TOML via the absence of the key) uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<u32>,
    /// Model for text-to-speech (`/audio/speech`).
    pub speech_model: String,
    /// Default voice for text-to-speech (e.g. `alloy`).
    pub speech_voice: String,
    /// Model for speech-to-text (`/audio/transcriptions`).
    pub transcription_model: String,
    /// Model ids to force-treat as accepting **image** input even when the gateway
    /// catalog doesn't advertise it (SOUL §7/§9) — the admin/global layer of the
    /// per-user `llm_settings.image_input_models` override. A chat inlines an image
    /// attachment for a model in this list (or in the user's own) regardless of what
    /// the catalog reports. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_input_models: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Dev defaults target the bundled echo-llmleaf, whose echo provider serves
        // every modality deterministically with no keys (config/llmleaf.dev.toml).
        Self {
            base_url: "http://localhost:8088".to_string(),
            // Base64("catalerum-dev:dev-echo-key") — the HTTP-Basic-shaped bearer
            // the bundled echo-llmleaf expects (config/llmleaf.dev.toml [[keys]]).
            api_key: "Y2F0YWxlcnVtLWRldjpkZXYtZWNoby1rZXk=".into(),
            control_token: Secret::default(),
            control_plane_enabled: false,
            default_model: "echo".to_string(),
            decision_model: None,
            embedding_model: "echo".to_string(),
            embedding_dimensions: None,
            speech_model: "echo".to_string(),
            speech_voice: "alloy".to_string(),
            transcription_model: "echo".to_string(),
            image_input_models: Vec::new(),
        }
    }
}
