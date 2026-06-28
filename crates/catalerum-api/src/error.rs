//! API error type and its HTTP rendering.
//!
//! Every handler returns [`ApiResult`]; an [`ApiError`] maps onto a JSON body
//! `{"error": "...", "kind": "..."}` with an appropriate status code. Errors
//! from the lower crates (`catalerum-core`, `catalerum-store`, `catalerum-iam`,
//! `catalerum-bus`) convert in via `From`, so handlers use `?` freely.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// Convenience alias for handler results.
pub type ApiResult<T> = Result<T, ApiError>;

/// A request-scoped error with a stable machine `kind` and a human `message`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Authentication missing or invalid (401).
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// Authenticated but not permitted (403).
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// Resource not found (404).
    #[error("not found")]
    NotFound,
    /// Malformed input / validation failure (400).
    #[error("bad request: {0}")]
    BadRequest(String),
    /// State conflict, e.g. duplicate (409).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Anything else (500).
    #[error("internal error: {0}")]
    Internal(String),
}

impl ApiError {
    /// 401 helper.
    pub fn unauthorized(msg: impl std::fmt::Display) -> Self {
        Self::Unauthorized(msg.to_string())
    }
    /// 400 helper.
    pub fn bad_request(msg: impl std::fmt::Display) -> Self {
        Self::BadRequest(msg.to_string())
    }
    /// 500 helper.
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }

    /// The HTTP status this error renders as.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound => StatusCode::NOT_FOUND,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The stable machine-readable `kind` token.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            ApiError::Unauthorized(_) => "unauthorized",
            ApiError::Forbidden(_) => "forbidden",
            ApiError::NotFound => "not_found",
            ApiError::BadRequest(_) => "bad_request",
            ApiError::Conflict(_) => "conflict",
            ApiError::Internal(_) => "internal",
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    kind: &'static str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: self.to_string(),
            kind: self.kind(),
        };
        (self.status(), Json(body)).into_response()
    }
}

impl From<catalerum_core::Error> for ApiError {
    fn from(e: catalerum_core::Error) -> Self {
        use catalerum_core::Error as C;
        match e {
            C::NotFound => ApiError::NotFound,
            C::Unauthorized(m) => ApiError::Unauthorized(m),
            C::Invalid(m) => ApiError::BadRequest(m),
            C::Conflict(m) => ApiError::Conflict(m),
            C::InvalidId(e) => ApiError::BadRequest(e.to_string()),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<catalerum_store::StoreError> for ApiError {
    fn from(e: catalerum_store::StoreError) -> Self {
        use catalerum_store::StoreError as S;
        match e {
            S::NotFound => ApiError::NotFound,
            S::Conflict(m) => ApiError::Conflict(m),
            S::Decode(m) => ApiError::BadRequest(m),
            S::Invalid(m) => ApiError::BadRequest(m),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<catalerum_iam::Error> for ApiError {
    fn from(e: catalerum_iam::Error) -> Self {
        use catalerum_iam::Error as I;
        match e {
            I::NotFound => ApiError::NotFound,
            I::Unauthorized(m) => ApiError::Unauthorized(m),
            I::Forbidden(m) => ApiError::Forbidden(m),
            I::Conflict(m) => ApiError::Conflict(m),
            I::Invalid(m) => ApiError::BadRequest(m),
            I::TokenConsumed => ApiError::Unauthorized("login token already used".to_string()),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<catalerum_bus::BusError> for ApiError {
    fn from(e: catalerum_bus::BusError) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<catalerum_ingest::IngestError> for ApiError {
    fn from(e: catalerum_ingest::IngestError) -> Self {
        use catalerum_ingest::IngestError as I;
        // Preserve the store's / provider's status mapping (via the existing
        // conversions); fold the derived-index faults (vector/embed/graph) into 500.
        match e {
            I::Store(s) => s.into(),
            I::Provider(c) => c.into(),
            other => ApiError::Internal(other.to_string()),
        }
    }
}
