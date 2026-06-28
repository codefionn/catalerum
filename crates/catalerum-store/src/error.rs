//! Store error type. Wraps `sqlx::Error` and converts into
//! [`catalerum_core::Error`] so repositories can surface domain-level errors.

use catalerum_core::Error as CoreError;

/// Errors raised by the store layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A row was expected but none was found.
    #[error("not found")]
    NotFound,

    /// A unique/foreign-key constraint was violated.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A stored value could not be parsed into a domain type.
    #[error("invalid data: {0}")]
    Decode(String),

    /// A caller-supplied value was rejected by a repository invariant (e.g. a
    /// self-referential link). Distinct from [`Decode`](Self::Decode), which is
    /// about *stored* data.
    #[error("invalid: {0}")]
    Invalid(String),

    /// Encryption or decryption of a stored secret failed (bad key, tampered
    /// ciphertext, or a malformed nonce). Never carries the plaintext.
    #[error("crypto error: {0}")]
    Crypto(String),

    /// Any other database error.
    #[error(transparent)]
    Sqlx(#[from] sqlx::Error),

    /// A migration failed to apply.
    #[error(transparent)]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Convenience result alias for store operations.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

impl StoreError {
    /// Classify a raw `sqlx::Error`, mapping `RowNotFound` and unique/FK
    /// constraint violations onto the dedicated variants.
    #[must_use]
    pub fn from_sqlx(err: sqlx::Error) -> Self {
        match &err {
            sqlx::Error::RowNotFound => Self::NotFound,
            sqlx::Error::Database(db) => {
                // Prefer sqlx's backend-neutral classification (PostgreSQL SQLSTATE
                // and SQLite extended result codes differ). Keep the 23xxx fallback
                // for PostgreSQL constraint kinds sqlx does not classify itself.
                if matches!(
                    db.kind(),
                    sqlx::error::ErrorKind::UniqueViolation
                        | sqlx::error::ErrorKind::ForeignKeyViolation
                ) || db.code().as_deref().is_some_and(|c| c.starts_with("23"))
                {
                    Self::Conflict(db.message().to_owned())
                } else {
                    Self::Sqlx(err)
                }
            }
            _ => Self::Sqlx(err),
        }
    }

    /// Build a decode error from any displayable cause.
    #[must_use]
    pub fn decode(msg: impl std::fmt::Display) -> Self {
        Self::Decode(msg.to_string())
    }

    /// Build an invalid-input error from any displayable cause.
    #[must_use]
    pub fn invalid(msg: impl std::fmt::Display) -> Self {
        Self::Invalid(msg.to_string())
    }
}

impl From<StoreError> for CoreError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::NotFound => CoreError::NotFound,
            StoreError::Conflict(m) => CoreError::Conflict(m),
            StoreError::Decode(m) => CoreError::Invalid(m),
            StoreError::Invalid(m) => CoreError::Invalid(m),
            StoreError::Crypto(m) => CoreError::other(m),
            StoreError::Sqlx(e) => CoreError::other(e.to_string()),
            StoreError::Migrate(e) => CoreError::other(e.to_string()),
        }
    }
}
