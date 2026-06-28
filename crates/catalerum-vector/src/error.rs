//! Error type for the Qdrant vector store (SOUL §6.4).

/// Errors raised by [`VectorStore`](crate::VectorStore) operations.
#[derive(Debug, thiserror::Error)]
pub enum VectorError {
    /// The configured Qdrant `url` could not be parsed as a base URL.
    #[error("invalid Qdrant url: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// The HTTP transport failed (connection refused, timeout, TLS, …).
    #[error("qdrant transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Qdrant answered with a non-success status. `body` is the raw response.
    #[error("qdrant returned {status}: {body}")]
    Api {
        /// HTTP status code Qdrant returned.
        status: u16,
        /// Raw response body (Qdrant's `{"status":{"error":...}}` payload).
        body: String,
    },

    /// A collection already exists with a different vector width than requested.
    /// Recreating it would silently drop data, so we refuse and surface it.
    #[error(
        "collection {collection} has vector width {found}, but {expected} was requested \
         (drop the collection to rebuild at the new width)"
    )]
    DimensionMismatch {
        /// The Qdrant collection name.
        collection: String,
        /// The width the caller asked for.
        expected: u64,
        /// The width the existing collection actually has.
        found: u64,
    },

    /// A Qdrant response could not be deserialized into the expected shape.
    #[error("malformed qdrant response: {0}")]
    Malformed(String),
}

/// Result alias for vector-store operations.
pub type Result<T> = std::result::Result<T, VectorError>;
