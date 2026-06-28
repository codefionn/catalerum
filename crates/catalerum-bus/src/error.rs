//! Error type for the bus. Wraps `redis::RedisError` and serde, and converts
//! into [`catalerum_core::Error`] so callers can use `?` against the core result.

use catalerum_core::Error as CoreError;

/// Convenient alias for bus operations.
pub type BusResult<T, E = BusError> = std::result::Result<T, E>;

/// Errors surfaced by the bus.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BusError {
    /// Underlying Valkey/Redis transport or protocol error.
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),

    /// (De)serialization of a payload (e.g. a `StreamEvent`) failed.
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),

    /// A lock release/refresh was attempted by a holder that no longer owns the
    /// lock (token mismatch or already expired). Safe to ignore in most paths.
    #[error("lock not held: {0}")]
    LockNotHeld(String),

    /// The subscription channel closed before a value arrived (publisher gone).
    #[error("relay channel closed")]
    RelayClosed,

    /// Anything else.
    #[error("{0}")]
    Other(String),
}

impl BusError {
    /// Build a free-form error.
    pub fn other(msg: impl std::fmt::Display) -> Self {
        BusError::Other(msg.to_string())
    }
}

impl From<BusError> for CoreError {
    fn from(e: BusError) -> Self {
        match e {
            BusError::Serde(e) => CoreError::Serde(e),
            other => CoreError::Other(other.to_string()),
        }
    }
}
