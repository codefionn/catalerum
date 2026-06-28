//! The llmleaf embeddings client adapter (SOUL §6.4/§7).
//!
//! Implements [`Embedder`] on [`OpenRouterClient`] by delegating to
//! `llmleaf-client`'s `POST /v1/embeddings` SDK call. The vectors feed the
//! derived Qdrant index (`catalerum-vector`).

use async_trait::async_trait;

use catalerum_core::embed::{Embedding, EmbeddingRequest, EmbeddingResponse};
use catalerum_core::error::Result;
use catalerum_core::provider::Embedder;

use crate::client::{map_sdk_error, map_usage, OpenRouterClient};

#[async_trait]
impl Embedder for OpenRouterClient {
    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse> {
        let mut sdk_request = llmleaf_client::EmbeddingRequest::new(
            request.model,
            llmleaf_client::EmbeddingInput::Many(request.input),
        );
        sdk_request.dimensions = request.dimensions;
        // Catalerum consumes float vectors directly. The SDK can decode base64,
        // but asking for floats keeps the wire shape aligned with the old client.
        sdk_request.encoding_format = Some("float".to_string());

        let mut response = self
            .sdk()?
            .embeddings(sdk_request)
            .await
            .map_err(map_sdk_error)?;

        response.data.sort_by_key(|e| e.index);

        Ok(EmbeddingResponse {
            model: response.model,
            embeddings: response
                .data
                .into_iter()
                .map(|e| Embedding {
                    index: e.index,
                    vector: e.embedding,
                })
                .collect(),
            usage: response.usage.map(map_usage),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_reads_first_vector_width() {
        let resp = EmbeddingResponse {
            model: "m".into(),
            embeddings: vec![Embedding {
                index: 0,
                vector: vec![0.1, 0.2],
            }],
            usage: None,
        };
        assert_eq!(resp.dimensions(), Some(2));
    }
}
