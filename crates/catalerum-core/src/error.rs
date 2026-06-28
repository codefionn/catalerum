//! Crate-wide error and result types.
//!
//! `catalerum-core` is the dependency root, so its [`Error`] is the shared
//! vocabulary every other crate maps its own failures into (or wraps). It is
//! intentionally provider-agnostic (SOUL §3.2): no variant names a concrete
//! backend.

use thiserror::Error;

/// The canonical catalerum error.
///
/// Provider crates surface backend-specific failures through the broad
/// [`Provider`](Error::Provider) / [`Io`](Error::Io) variants or by mapping
/// into a precise variant (e.g. [`NotFound`](Error::NotFound),
/// [`Unauthorized`](Error::Unauthorized)).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A requested object does not exist.
    #[error("not found")]
    NotFound,

    /// The caller is authenticated but lacks the capability for this action
    /// (SOUL §19, deny-by-default).
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The call cleared the capability check but was blocked by a profile's
    /// programmable **tool guard** (SOUL §19) — a Boa/LLM classifier or a user
    /// rejecting it. Distinct from [`Unauthorized`](Error::Unauthorized) so the
    /// message reads as a policy denial, not a missing grant.
    #[error("denied by policy: {0}")]
    Denied(String),

    /// The request/input was malformed or failed validation.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// A conflict with existing state (unique constraint, optimistic-lock,
    /// stale ETag/sequence, …).
    #[error("conflict: {0}")]
    Conflict(String),

    /// The operation isn't supported by this provider/backend (e.g. write to a
    /// read-only calendar).
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// A timeout elapsed.
    #[error("timed out")]
    Timeout,

    /// A failure originating in an external provider/backend, kept opaque so
    /// core stays provider-agnostic.
    #[error("provider error: {0}")]
    Provider(String),

    /// Serialization / deserialization failure.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// Underlying I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A failure to parse a strongly-typed identifier from a string.
    #[error("invalid id: {0}")]
    InvalidId(#[from] uuid::Error),

    /// A catch-all for errors not yet modelled. Prefer a precise variant.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Construct an [`Error::Other`] from any displayable value.
    pub fn other(msg: impl std::fmt::Display) -> Self {
        Self::Other(msg.to_string())
    }

    /// Construct an [`Error::Provider`] from any displayable value.
    pub fn provider(msg: impl std::fmt::Display) -> Self {
        Self::Provider(msg.to_string())
    }

    /// Construct an [`Error::Invalid`] from any displayable value.
    pub fn invalid(msg: impl std::fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }

    /// Construct an [`Error::Unauthorized`] from any displayable value.
    pub fn unauthorized(msg: impl std::fmt::Display) -> Self {
        Self::Unauthorized(msg.to_string())
    }

    /// Construct an [`Error::Denied`] from any displayable value.
    pub fn denied(msg: impl std::fmt::Display) -> Self {
        Self::Denied(msg.to_string())
    }
}

/// Crate-wide result alias over [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
