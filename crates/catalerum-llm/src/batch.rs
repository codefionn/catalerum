//! Batch chat completions over llmleaf (SOUL §7): submit many requests as one
//! job (`POST /v1/batches`), poll its status, cancel it, and stream the results.
//!
//! Batches trade latency for throughput/cost — useful for offline curation or
//! bulk enrichment where the answers aren't needed live. The SDK's wire shapes
//! are mapped to provider-neutral types; per-item results fold into the same
//! [`CollectedTurn`] the streaming path yields.

use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use catalerum_core::error::Result;
use catalerum_core::llm::ChatRequest;

use crate::client::{collected_from_response, map_sdk_error, CollectedTurn, OpenRouterClient};

/// Lifecycle state of a batch job (mirrors llmleaf's `BatchStatus`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchState {
    Validating,
    InProgress,
    Finalizing,
    Completed,
    Failed,
    Expired,
    Canceling,
    Canceled,
}

impl From<llmleaf_client::BatchStatus> for BatchState {
    fn from(s: llmleaf_client::BatchStatus) -> Self {
        match s {
            llmleaf_client::BatchStatus::Validating => BatchState::Validating,
            llmleaf_client::BatchStatus::InProgress => BatchState::InProgress,
            llmleaf_client::BatchStatus::Finalizing => BatchState::Finalizing,
            llmleaf_client::BatchStatus::Completed => BatchState::Completed,
            llmleaf_client::BatchStatus::Failed => BatchState::Failed,
            llmleaf_client::BatchStatus::Expired => BatchState::Expired,
            llmleaf_client::BatchStatus::Canceling => BatchState::Canceling,
            llmleaf_client::BatchStatus::Canceled => BatchState::Canceled,
        }
    }
}

/// Aggregate per-item progress counts on a [`BatchJob`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCounts {
    pub total: u64,
    pub processing: u64,
    pub succeeded: u64,
    pub errored: u64,
    pub canceled: u64,
    pub expired: u64,
}

impl From<llmleaf_client::BatchCounts> for BatchCounts {
    fn from(c: llmleaf_client::BatchCounts) -> Self {
        BatchCounts {
            total: c.total,
            processing: c.processing,
            succeeded: c.succeeded,
            errored: c.errored,
            canceled: c.canceled,
            expired: c.expired,
        }
    }
}

/// A submitted batch job handle (mapped from llmleaf's `BatchHandle`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchJob {
    /// Server-assigned batch id (pass to [`OpenRouterClient::get_batch`] etc.).
    pub id: String,
    /// Current lifecycle state.
    pub status: BatchState,
    /// Per-item progress counts.
    pub counts: BatchCounts,
}

impl From<llmleaf_client::BatchHandle> for BatchJob {
    fn from(h: llmleaf_client::BatchHandle) -> Self {
        BatchJob {
            id: h.id,
            status: h.status.into(),
            counts: h.counts.into(),
        }
    }
}

/// One line of a batch's results: the caller's `custom_id` and the outcome —
/// either the folded completion or the per-item error message.
#[derive(Debug)]
pub struct BatchResult {
    /// The `custom_id` supplied when the request was submitted.
    pub custom_id: String,
    /// The completion (`Ok`) or the gateway's per-item error (`Err`).
    pub outcome: std::result::Result<CollectedTurn, String>,
}

fn map_result_line(line: llmleaf_client::BatchResultLine) -> BatchResult {
    let outcome = match (line.response, line.error) {
        (Some(resp), _) => Ok(collected_from_response(resp.body)),
        (None, Some(err)) => Err(format!("{}: {}", err.code, err.message)),
        (None, None) => Err("batch item had neither a response nor an error".to_string()),
    };
    BatchResult {
        custom_id: line.custom_id,
        outcome,
    }
}

impl OpenRouterClient {
    /// Submit a batch of `(custom_id, request)` chat completions as one job.
    /// Provider routing / `models` defaults are applied to each item exactly as
    /// for a live chat request.
    pub async fn create_batch(&self, requests: Vec<(String, ChatRequest)>) -> Result<BatchJob> {
        let sdk_request = llmleaf_client::BatchCreateRequest {
            requests: requests
                .into_iter()
                .map(|(custom_id, req)| llmleaf_client::BatchRequestItem {
                    custom_id,
                    body: self.to_chat_request(&req),
                })
                .collect(),
        };
        let handle = self
            .sdk()?
            .create_batch(sdk_request)
            .await
            .map_err(map_sdk_error)?;
        Ok(handle.into())
    }

    /// Retrieve a batch job's current status (`GET /v1/batches/{id}`).
    pub async fn get_batch(&self, id: &str) -> Result<BatchJob> {
        let handle = self.sdk()?.get_batch(id).await.map_err(map_sdk_error)?;
        Ok(handle.into())
    }

    /// Request cancellation of a batch job (`POST /v1/batches/{id}/cancel`).
    pub async fn cancel_batch(&self, id: &str) -> Result<BatchJob> {
        let handle = self.sdk()?.cancel_batch(id).await.map_err(map_sdk_error)?;
        Ok(handle.into())
    }

    /// Stream a completed batch's results (`GET /v1/batches/{id}/results`), one
    /// [`BatchResult`] per item, in the order the gateway returns them.
    pub async fn batch_results(&self, id: &str) -> Result<impl Stream<Item = Result<BatchResult>>> {
        let lines = self.sdk()?.batch_results(id).await.map_err(map_sdk_error)?;
        Ok(lines.map(|line| line.map(map_result_line).map_err(map_sdk_error)))
    }
}
