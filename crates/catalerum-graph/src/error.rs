//! Error type for the Neo4j graph store (SOUL §6.3).

/// One Neo4j Cypher error, as returned in the `errors` array of a
/// `/tx/commit` response. Neo4j answers HTTP `200` even when a statement fails,
/// reporting the failure here — so a non-empty `errors` array is a failed
/// transaction.
#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize)]
pub struct Neo4jError {
    /// Status code, e.g. `Neo.ClientError.Statement.SyntaxError`.
    pub code: String,
    /// Human-readable message.
    #[serde(default)]
    pub message: String,
}

/// Errors raised by [`GraphStore`](crate::GraphStore) operations.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// The configured Neo4j `url` could not be parsed as a base URL.
    #[error("invalid Neo4j url: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// The HTTP transport failed (connection refused, timeout, TLS, …).
    #[error("neo4j transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// Neo4j answered with a non-success HTTP status (auth failure, 5xx, …).
    /// `body` is the raw response.
    #[error("neo4j returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code Neo4j returned.
        status: u16,
        /// Raw response body.
        body: String,
    },

    /// The transaction committed at the HTTP layer but one or more statements
    /// reported a Cypher error (Neo4j returns these with HTTP `200`).
    #[error("neo4j rejected the transaction: {}", format_errors(.0))]
    Cypher(Vec<Neo4jError>),

    /// A Neo4j response could not be deserialized into the expected shape.
    #[error("malformed neo4j response: {0}")]
    Malformed(String),
}

fn format_errors(errors: &[Neo4jError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", e.code, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Result alias for graph-store operations.
pub type Result<T> = std::result::Result<T, GraphError>;
