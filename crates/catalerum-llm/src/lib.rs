//! catalerum-llm — the llmleaf client (SOUL §7). One client, every modality.
//!
//! llmleaf is multi-modal over one OpenAI-compatible base URL + bearer, so the
//! single [`OpenRouterClient`] (alias [`LlmleafClient`]) implements all four core
//! modality traits:
//! - [`LlmClient`](catalerum_core::provider::LlmClient) — chat: POSTs the
//!   OpenAI Responses dialect (`model`, `input` items, flat `tools`,
//!   `tool_choice`, `reasoning`, plus the `provider { order, allow_fallbacks }`
//!   / `models` routing passthrough) to `{base_url}/v1/responses` and folds the
//!   typed SSE events into
//!   [`StreamEvent`](catalerum_core::stream::StreamEvent)s — text deltas,
//!   tool-call deltas assembled across events, and a single guaranteed terminal
//!   `Done` (the `message_done` contract). Only the batch API still speaks
//!   chat-completions shapes (the SDK's batch contract).
//! - [`Embedder`](catalerum_core::provider::Embedder) — `POST /embeddings`
//!   (`embed.rs`); the vectors feed Qdrant (`catalerum-vector`, §6.4).
//! - [`SpeechSynthesizer`](catalerum_core::provider::SpeechSynthesizer) —
//!   text-to-speech, `POST /audio/speech` (`audio.rs`).
//! - [`Transcriber`](catalerum_core::provider::Transcriber) — speech-to-text,
//!   `POST /audio/transcriptions` (`audio.rs`).
//!
//! Three layers:
//! - [`OpenRouterClient`] — the streaming client + a collected, non-streaming
//!   helper ([`OpenRouterClient::chat`]).
//! - [`run_agent`] — the agent loop: stream a turn, dispatch any `tool_calls`
//!   through a [`ToolRegistry`](catalerum_core::tool::ToolRegistry), append the
//!   results, and loop until a normal finish.
//! - [`wire`] — the OpenRouter request/response serde types.
//!
//! ```no_run
//! # async fn demo() -> catalerum_core::Result<()> {
//! use catalerum_llm::OpenRouterClient;
//! use catalerum_core::llm::{ChatMessage, ChatRequest};
//! use futures::StreamExt;
//!
//! // Origin only — the client adds the versioned path (`/v1/responses`, …).
//! let client = OpenRouterClient::new("http://llmleaf:8080", "sk-…");
//! let req = ChatRequest::new("gpt-4o", vec![ChatMessage::user("hi")]);
//!
//! // Streaming:
//! let mut stream = client.chat_stream_owned(req.clone()).await?;
//! while let Some(ev) = stream.next().await { let _ev = ev?; }
//!
//! // Or collected:
//! let turn = client.chat(req).await?;
//! println!("{}", turn.content);
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod agent;
pub mod audio;
pub mod batch;
pub mod catalog;
pub mod client;
pub mod compact;
pub mod embed;
pub mod ocr;
pub mod sse;
mod trace;
pub mod wire;

pub use agent::{
    run_agent, run_agent_streaming, AgentConfig, AgentOutcome, CompletedMessage, ToolInvocation,
    TurnObserver,
};
pub use compact::{CompactionConfig, COMPACTION_SUMMARY_PREFIX, DEFAULT_CONTEXT_WINDOW};
// Re-export the agent loop's stop signal ([`AgentConfig::cancel`]) so callers
// wiring a Stop control don't need their own `tokio-util` dependency.
pub use batch::{BatchCounts, BatchJob, BatchResult, BatchState};
pub use catalog::{ModelInfo, ModelKind, VoiceInfo};
pub use client::{CollectedTurn, OpenRouterClient, OpenRouterClientBuilder};
pub use ocr::VisionOcr;
pub use tokio_util::sync::CancellationToken;
pub use trace::{LlmTraceConfig, TraceContent, LANGFUSE_LLM_TARGET, OTEL_LLM_TARGET};
pub use wire::ProviderRouting;

/// The modality-neutral name for the llmleaf client. The same struct speaks chat
/// ([`LlmClient`]), embeddings ([`Embedder`]), text-to-speech
/// ([`SpeechSynthesizer`]), and speech-to-text ([`Transcriber`]) over one base
/// URL + bearer.
pub type LlmleafClient = OpenRouterClient;

// Re-export the core modality traits so downstream code can `use catalerum_llm::…`.
pub use catalerum_core::provider::{Embedder, LlmClient, SpeechSynthesizer, Transcriber};

// Re-export the modality request/response types for ergonomic downstream use.
pub use catalerum_core::audio::{
    SpeechAudio, SpeechRequest, TranscriptionRequest, TranscriptionResponse,
};
pub use catalerum_core::embed::{Embedding, EmbeddingRequest, EmbeddingResponse};

impl OpenRouterClient {
    /// Convenience alias for [`LlmClient::chat_stream`] usable without importing
    /// the trait (mirrors [`Self::stream`] but named to match the trait method).
    pub async fn chat_stream_owned(
        &self,
        request: catalerum_core::llm::ChatRequest,
    ) -> catalerum_core::Result<
        futures::stream::BoxStream<
            'static,
            catalerum_core::Result<catalerum_core::stream::StreamEvent>,
        >,
    > {
        self.stream(request).await
    }
}
