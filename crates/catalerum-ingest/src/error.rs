//! Ingest error type.
//!
//! Sync orchestration touches three failure domains — the calendar provider
//! ([`catalerum_core::Error`]), the Postgres store ([`catalerum_store::StoreError`]),
//! and JSON (de)serialization of connection `config` / job payloads. This type
//! folds them into one so the worker can decide, per error, whether to retry.

use thiserror::Error;

/// An error raised while ingesting (syncing) a connection or running a job.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IngestError {
    /// A failure from a calendar provider (parse, network, read-only write, …).
    #[error("provider: {0}")]
    Provider(#[from] catalerum_core::Error),

    /// A failure from the Postgres store (the source of truth).
    #[error("store: {0}")]
    Store(#[from] catalerum_store::StoreError),

    /// A failure from the Qdrant vector index (the derived embedding store).
    #[error("vector: {0}")]
    Vector(#[from] catalerum_vector::VectorError),

    /// A failure from the Neo4j graph (the derived projection).
    #[error("graph: {0}")]
    Graph(#[from] catalerum_graph::GraphError),

    /// The embedder returned a result inconsistent with the request — a wrong
    /// vector count or an unexpected dimensionality. A bug or provider fault,
    /// not a transient error.
    #[error("embed: {0}")]
    Embed(String),

    /// A job payload or connection `config` could not be (de)serialized.
    #[error("payload: {0}")]
    Payload(#[from] serde_json::Error),

    /// A job referenced a kind this worker does not handle.
    #[error("unknown job kind: {0}")]
    UnknownKind(String),

    /// A job's payload was missing a required field or otherwise invalid.
    #[error("invalid job: {0}")]
    InvalidJob(String),

    /// A job is not **authorized** to do what it asked — e.g. a collect poll whose
    /// automation's §19 grant does not cover the connection it pulls from (SOUL
    /// §11/§19). A permanent authorization failure: the run **fails closed** (a
    /// clear error, never a silent skip), not a transient condition worth retrying.
    #[error("forbidden: {0}")]
    Forbidden(String),
}

impl IngestError {
    /// Build an [`IngestError::InvalidJob`] from any displayable cause.
    pub fn invalid_job(msg: impl std::fmt::Display) -> Self {
        Self::InvalidJob(msg.to_string())
    }

    /// Build an [`IngestError::Forbidden`] from any displayable cause — an
    /// authorization denial that fails the run closed (SOUL §19).
    pub fn forbidden(msg: impl std::fmt::Display) -> Self {
        Self::Forbidden(msg.to_string())
    }
}

/// Result alias over [`IngestError`].
pub type Result<T, E = IngestError> = std::result::Result<T, E>;
