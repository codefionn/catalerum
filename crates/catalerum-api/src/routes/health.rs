//! Health endpoint (SOUL §16, M1 "health endpoint").

use axum::routing::get;
use axum::Router;

use crate::state::AppState;

/// `GET /healthz` -> `"ok"`. Unauthenticated liveness probe.
pub fn router() -> Router<AppState> {
    Router::new().route("/healthz", get(healthz))
}

async fn healthz() -> &'static str {
    "ok"
}
