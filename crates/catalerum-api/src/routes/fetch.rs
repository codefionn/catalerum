//! Web fetch REST (SOUL §12, §27).
//!
//! `POST /fetch` retrieves a web page and returns it as AI-friendly Markdown (or
//! html/text), via the configured [`WebFetcher`](catalerum_core::provider::WebFetcher)
//! backend (`catalerum-fetch`: HTTP / browser-CDP / Firecrawl). This is the
//! scoped endpoint the LLM's `fetch_url` tool is a thin client of (SOUL §7); the
//! capability is `web:read` and the SSRF guard lives in the backend (SOUL §19).
//!
//! Bearer-authenticated and workspace-scoped like every other route.

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};

use catalerum_core::capability::Action;
use catalerum_core::provider::{FetchRequest, FetchedPage};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the fetch route.
pub fn router() -> Router<AppState> {
    Router::new().route("/fetch", post(fetch))
}

async fn fetch(
    State(state): State<AppState>,
    auth: Auth,
    Json(request): Json<FetchRequest>,
) -> ApiResult<Json<FetchedPage>> {
    // Deny-by-default: fetching the web requires `web:read` (SOUL §19/§27) — the
    // same capability the `fetch_url` tool dispatches under. A narrower grant can
    // attenuate it per-host later; the SSRF guard is enforced in the backend.
    auth.require(Action::Read, "web")?;

    let fetcher = state
        .fetcher()
        .ok_or_else(|| ApiError::internal("web fetch backend is not configured"))?;

    if request.url.trim().is_empty() {
        return Err(ApiError::bad_request("`url` is required"));
    }

    let page = fetcher.fetch(request).await?;
    Ok(Json(page))
}
