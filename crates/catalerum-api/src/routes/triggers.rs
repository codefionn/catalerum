//! Named-signal trigger sources (SOUL §11/§12) — fire a workspace's `trigger`
//! automations on demand over HTTP.
//!
//! Three surfaces sit here, all around a `{ "kind": "trigger", "name": … }` trigger:
//!
//! - `POST /triggers/{name}` — **authenticated** fire. Reachable by any principal
//!   (session or workspace service token, §18), scoped to that principal's workspace,
//!   gated on `automation:write`. Builds a [`TriggerEvent::Trigger`] and dispatches:
//!   every enabled automation with a matching trigger gets a durable
//!   `run_automation` job (the same bridge the webhook / Kanban sources use, §24). An
//!   optional JSON request body is carried as the run's trigger `payload`.
//!
//! - `POST /triggers/mint/{name}` — **authenticated** mint: hands back a signed,
//!   short-lived public URL (`POST /triggers/fire/{token}`) for the named signal.
//!   Same `automation:write` gate; the twin of the `trigger_link` tool.
//!
//! - `POST /triggers/fire/{token}` — **public, unauthenticated** redeem: the one
//!   surface here with no `Auth` extractor, because the HMAC-signed
//!   [`TriggerClaims`](crate::trigger_link::TriggerClaims) token *is* its own
//!   authorization (§19). It re-verifies the signature + expiry, then fires exactly
//!   the one signal, in the one workspace, the claims name. Every verify failure
//!   collapses to a flat `404` so a probe learns nothing.
//!
//! Firing gates *triggering* only — each automation then runs under its own §19
//! authority, so a leaked link/token is bounded by what those automations already do.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use catalerum_automation::TriggerEvent;
use catalerum_core::capability::Action;
use catalerum_core::WorkspaceId;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::trigger_link::TriggerClaims;

/// Default trigger-link lifetime (1 hour) and the clamp bounds — mirrors the
/// download-link TTL policy so both signed-link surfaces expire on the same terms.
const DEFAULT_LINK_TTL_SECS: u64 = 60 * 60;
const MIN_LINK_TTL_SECS: u64 = 60;
const MAX_LINK_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Mount the trigger routes (authed fire + mint, public redeem).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/triggers/{name}", post(fire))
        .route("/triggers/mint/{name}", post(mint))
        .route("/triggers/fire/{token}", post(redeem))
}

/// The result of firing a named signal: how many automations matched and the durable
/// `run_automation` jobs enqueued for them (one per match).
#[derive(Debug, Serialize)]
pub struct FireResult {
    /// Number of enabled automations whose trigger matched this signal name.
    pub matched: usize,
    /// The enqueued `run_automation` job ids (one per matched automation).
    pub jobs: Vec<uuid::Uuid>,
}

/// `POST /triggers/{name}` — authenticated fire. Gated on `automation:write`, scoped
/// to the caller's workspace; an optional JSON body rides along as the `payload`.
async fn fire(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<FireResult>)> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let name = normalize_name(&name)?;
    let payload = parse_payload(&body)?;
    let result = dispatch(&state, p.workspace_id, name, payload).await?;
    Ok((StatusCode::ACCEPTED, Json(result)))
}

/// Query params for the mint route: an optional link lifetime.
#[derive(Debug, Deserialize)]
struct MintParams {
    /// Link lifetime in seconds (default 3600; clamped to `[60, 604800]`).
    ttl_secs: Option<u64>,
}

/// A minted trigger-fire link.
#[derive(Debug, Serialize)]
pub struct TriggerLink {
    /// The public URL to `POST` to fire the signal (no login needed).
    pub url: String,
    /// The signed token embedded in the URL (also usable directly).
    pub token: String,
    /// The signal name the link fires.
    pub name: String,
    /// RFC 3339 absolute expiry.
    pub expires_at: String,
}

/// `POST /triggers/mint/{name}` — authenticated mint of a public trigger-fire link.
/// Same `automation:write` gate as firing; the REST twin of the `trigger_link` tool.
async fn mint(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Query(params): Query<MintParams>,
) -> ApiResult<Json<TriggerLink>> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let name = normalize_name(&name)?;
    let ttl = params
        .ttl_secs
        .unwrap_or(DEFAULT_LINK_TTL_SECS)
        .clamp(MIN_LINK_TTL_SECS, MAX_LINK_TTL_SECS);
    let exp = chrono::Utc::now().timestamp() + ttl as i64;
    let claims = TriggerClaims {
        workspace_id: p.workspace_id,
        name: name.clone(),
        exp,
    };
    let token = state.trigger_signer().mint(&claims);
    let base = state.config().server.effective_base_url();
    let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    Ok(Json(TriggerLink {
        url: format!("{base}/triggers/fire/{token}"),
        token,
        name,
        expires_at,
    }))
}

/// `POST /triggers/fire/{token}` — public redeem. Unauthenticated: the signed token
/// is the capability. Any verify problem → an opaque `404` (never reveal whether a
/// token was forged, expired, or simply matched nothing).
async fn redeem(
    State(state): State<AppState>,
    Path(token): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<FireResult>)> {
    let now = chrono::Utc::now().timestamp();
    let claims = state
        .trigger_signer()
        .verify(&token, now)
        .map_err(|_| ApiError::NotFound)?;
    // Fail closed + opaque (SOUL §18): an archived workspace is treated exactly
    // like a bad token here — a flat `404` that reveals nothing (never that the
    // workspace exists but is archived). The shared dispatch bridge already
    // matches-nothing for an archived workspace; this collapses the public
    // surface's response to the same opaque 404 as a forged/expired token.
    reject_archived_workspace(state.store(), claims.workspace_id).await?;
    let payload = parse_payload(&body)?;
    let result = dispatch(&state, claims.workspace_id, claims.name, payload).await?;
    Ok((StatusCode::ACCEPTED, Json(result)))
}

/// Build and dispatch a [`TriggerEvent::Trigger`] for `name`/`payload` in `ws`,
/// enqueuing a durable `run_automation` job per matching automation.
async fn dispatch(
    state: &AppState,
    ws: WorkspaceId,
    name: String,
    payload: Option<Value>,
) -> ApiResult<FireResult> {
    let event = TriggerEvent::Trigger { name, payload };
    let jobs = catalerum_ingest::dispatch_trigger_event(state.store(), ws, &event)
        .await
        .map_err(|e| ApiError::internal(format!("dispatching trigger automations: {e}")))?;
    Ok(FireResult {
        matched: jobs.len(),
        jobs,
    })
}

/// Fail closed on an **archived** workspace (SOUL §18) for the public redeem
/// surface: an archived workspace is indistinguishable from a bad token — a flat
/// `404` that never reveals the workspace exists but is archived. A live (or
/// vanished) workspace passes; only an archived row is rejected. `get` returns
/// archived rows by design, so we test the flag explicitly.
async fn reject_archived_workspace(
    store: &catalerum_store::Store,
    ws: WorkspaceId,
) -> ApiResult<()> {
    if let Ok(w) = store.workspaces().get(ws).await {
        if w.archived_at.is_some() {
            return Err(ApiError::NotFound);
        }
    }
    Ok(())
}

/// Normalize the signal name from the URL: trimmed, and rejected (`400`) if empty —
/// a named-signal trigger always has a name.
fn normalize_name(raw: &str) -> ApiResult<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("trigger name must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// Parse an optional JSON request body into a trigger `payload`: an empty/whitespace
/// body (or an explicit `null`) is `None`; any other value is carried verbatim;
/// malformed JSON is a `400` (a caller that sends a body must send valid JSON).
fn parse_payload(body: &axum::body::Bytes) -> ApiResult<Option<Value>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| ApiError::bad_request(format!("request body must be JSON: {e}")))?;
    Ok(if value.is_null() { None } else { Some(value) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// The public redeem surface fails closed on an archived workspace with an
    /// **opaque 404** — identical to a forged/expired token, revealing nothing.
    /// A live workspace passes; only the archived flag rejects (SOUL §18).
    #[tokio::test]
    async fn redeem_rejects_archived_workspace_as_opaque_404() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping archived-redeem test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("trig", &format!("trig-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");

        // Live: the guard passes.
        reject_archived_workspace(&store, ws.id)
            .await
            .expect("a live workspace passes the redeem guard");

        // Archived: the guard collapses to an opaque 404.
        store.workspaces().archive(ws.id).await.expect("archive");
        let err = reject_archived_workspace(&store, ws.id)
            .await
            .expect_err("an archived workspace is rejected");
        assert!(
            matches!(err, ApiError::NotFound),
            "archived → opaque 404 (never reveals archive state), got {err:?}"
        );
    }

    #[test]
    fn normalize_name_trims_and_rejects_empty() {
        assert_eq!(
            normalize_name("  rebuild-report ").unwrap(),
            "rebuild-report"
        );
        assert!(normalize_name("").is_err());
        assert!(normalize_name("   ").is_err());
    }

    #[test]
    fn parse_payload_handles_empty_null_and_json() {
        assert_eq!(parse_payload(&b"".to_vec().into()).unwrap(), None);
        assert_eq!(parse_payload(&b"   \n".to_vec().into()).unwrap(), None);
        assert_eq!(parse_payload(&b"null".to_vec().into()).unwrap(), None);
        assert_eq!(
            parse_payload(&b"{\"row\":3}".to_vec().into()).unwrap(),
            Some(json!({ "row": 3 }))
        );
        // A non-object JSON value is still a valid payload (carried verbatim).
        assert_eq!(
            parse_payload(&b"\"hello\"".to_vec().into()).unwrap(),
            Some(json!("hello"))
        );
        // Malformed JSON is a 400.
        assert!(parse_payload(&b"{not json".to_vec().into()).is_err());
    }
}
