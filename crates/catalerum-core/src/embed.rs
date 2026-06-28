//! Provider-agnostic embeddings request/response shapes (SOUL §6.4/§7).
//!
//! `catalerum` generates embeddings through the **same llmleaf proxy** as chat:
//! llmleaf is multi-modal (chat, embeddings, TTS, STT) over one
//! OpenAI-compatible endpoint. These types are what the
//! [`Embedder`](crate::provider::Embedder) trait consumes and produces;
//! `catalerum-llm` maps them to/from llmleaf's `POST /v1/embeddings`. The
//! resulting vectors feed the derived Qdrant index (`catalerum-vector`, §6.4).

use serde::{Deserialize, Serialize};

use crate::stream::Usage;

/// A request to embed one or more text inputs (SOUL §6.4). Order is preserved
/// end to end: response [`Embedding`] indices line up with `input` positions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    /// Model id (or an llmleaf routing alias).
    pub model: String,
    /// The texts to embed (a single-string call is just a one-element vector).
    pub input: Vec<String>,
    /// Optional output dimensionality (Matryoshka truncation, e.g. OpenAI
    /// `text-embedding-3-*`); `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
}

impl EmbeddingRequest {
    /// A request to embed `inputs` with `model`.
    #[must_use]
    pub fn new(model: impl Into<String>, inputs: Vec<String>) -> Self {
        Self {
            model: model.into(),
            input: inputs,
            dimensions: None,
        }
    }

    /// A request to embed a single string.
    #[must_use]
    pub fn single(model: impl Into<String>, input: impl Into<String>) -> Self {
        Self::new(model, vec![input.into()])
    }

    /// Request a specific output dimensionality.
    #[must_use]
    pub fn with_dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = Some(dimensions);
        self
    }
}

/// One embedding vector and the index of the input it corresponds to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Embedding {
    pub index: u32,
    pub vector: Vec<f32>,
}

/// The result of an [`EmbeddingRequest`]: one [`Embedding`] per input, in input
/// order, plus optional provider-reported token usage.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    /// The model that actually served (a provider may report its own upstream id).
    pub model: String,
    pub embeddings: Vec<Embedding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl EmbeddingResponse {
    /// The dimensionality of the returned vectors, if any (all vectors from one
    /// model share a width).
    #[must_use]
    pub fn dimensions(&self) -> Option<usize> {
        self.embeddings.first().map(|e| e.vector.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_is_one_input_no_dimensions() {
        let req = EmbeddingRequest::single("m", "hello");
        assert_eq!(req.input, vec!["hello".to_string()]);
        assert_eq!(req.dimensions, None);
    }

    #[test]
    fn with_dimensions_sets_field() {
        let req = EmbeddingRequest::new("m", vec!["a".into(), "b".into()]).with_dimensions(256);
        assert_eq!(req.dimensions, Some(256));
        assert_eq!(req.input.len(), 2);
    }

    #[test]
    fn dimensions_reads_first_vector_width() {
        let resp = EmbeddingResponse {
            model: "m".into(),
            embeddings: vec![Embedding {
                index: 0,
                vector: vec![0.1, 0.2, 0.3],
            }],
            usage: None,
        };
        assert_eq!(resp.dimensions(), Some(3));
        assert_eq!(EmbeddingResponse::default().dimensions(), None);
    }

    #[test]
    fn request_round_trips_through_json() {
        let req = EmbeddingRequest::single("m", "hi").with_dimensions(8);
        let json = serde_json::to_string(&req).unwrap();
        let back: EmbeddingRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, back);
    }
}
