//! IAM error type.
//!
//! Wraps [`catalerum_core::Error`] (the shared domain error) and the storage
//! layer (`sqlx`). Most IAM failures map cleanly onto the core variants
//! (`NotFound`, `Unauthorized`, `Conflict`, …); we keep a thin newtype so call
//! sites get a single `iam::Error` and a single [`Result`] alias.

use thiserror::Error;

/// Errors raised by `catalerum-iam`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A referenced row (user, workspace, session, token) does not exist.
    #[error("not found")]
    NotFound,

    /// Authentication failed: missing, malformed, expired, or revoked token.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// The caller is authenticated but the action is not permitted — e.g.
    /// establishing a session into an **archived** workspace (SOUL §18). Distinct
    /// from [`Unauthorized`](Self::Unauthorized) so API surfaces map it to `403`.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// A uniqueness or invariant conflict (e.g. duplicate slug/email).
    #[error("conflict: {0}")]
    Conflict(String),

    /// Invalid input (bad role string, malformed id, empty field).
    #[error("invalid: {0}")]
    Invalid(String),

    /// A one-time login token was already redeemed.
    #[error("login token already consumed")]
    TokenConsumed,

    /// The database layer failed.
    #[error("store: {0}")]
    Store(#[from] sqlx::Error),

    /// Bubbled-up core domain error.
    #[error(transparent)]
    Core(#[from] catalerum_core::Error),
}

impl Error {
    /// Construct an [`Error::Unauthorized`] from any displayable value.
    pub fn unauthorized(msg: impl std::fmt::Display) -> Self {
        Self::Unauthorized(msg.to_string())
    }

    /// Construct an [`Error::Forbidden`] from any displayable value.
    pub fn forbidden(msg: impl std::fmt::Display) -> Self {
        Self::Forbidden(msg.to_string())
    }

    /// Construct an [`Error::Invalid`] from any displayable value.
    pub fn invalid(msg: impl std::fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }

    /// Construct an [`Error::Conflict`] from any displayable value.
    pub fn conflict(msg: impl std::fmt::Display) -> Self {
        Self::Conflict(msg.to_string())
    }
}

/// Map an IAM error onto the shared [`catalerum_core::Error`] for API surfaces
/// that speak the core error type.
impl From<Error> for catalerum_core::Error {
    fn from(e: Error) -> Self {
        use catalerum_core::Error as C;
        match e {
            Error::NotFound => C::NotFound,
            Error::Unauthorized(m) => C::Unauthorized(m),
            // Core has no `Forbidden`; `Denied` is its permission-refused analog.
            Error::Forbidden(m) => C::Denied(m),
            Error::Conflict(m) => C::Conflict(m),
            Error::Invalid(m) => C::Invalid(m),
            Error::TokenConsumed => C::Unauthorized("login token already consumed".into()),
            Error::Store(e) => C::Other(format!("store: {e}")),
            Error::Core(c) => c,
        }
    }
}

/// IAM result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;
