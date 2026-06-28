//! Webhook trigger source (SOUL §11) — fire a workspace's `Webhook` automations
//! over HTTP.
//!
//! `POST /webhooks/{*path}` builds a [`TriggerEvent::Webhook`] whose `path` is the
//! URL path (leading slash included, e.g. `POST /webhooks/deploy-done` →
//! `path = "/deploy-done"`) and dispatches it: every **enabled** automation in the
//! workspace with a matching `{ "kind": "webhook", "path": "…" }` trigger gets a
//! durable `run_automation` job enqueued (the same bridge the Kanban `TaskMoved`
//! source uses, §24). The worker then runs each under its own §19 authority — the
//! caller's role gates *triggering*, not what the automation may *do*.
//!
//! **Authenticated + workspace-scoped first cut (SOUL §18/§19).** Reachable only
//! by an authenticated principal (a session or a workspace-bound service token,
//! §18), scoped to that principal's workspace — cross-workspace reach is
//! impossible by construction. Gated on `automation:write` via the shared
//! [`Auth::require`] gate (a Viewer is `403`, deny-by-default). The *unauthenticated*
//! public-webhook shape (an external provider POSTing with a per-automation secret
//! path/token) is a later hardening; this keeps the same auth + capability scoping
//! as every other surface — no backdoor (principle 15).
//!
//! The request body, if any, is accepted and ignored: [`TriggerEvent::Webhook`]
//! carries only the path today, so payload forwarding into the run is a later
//! enhancement. The endpoint reports how many automations matched + the enqueued
//! job ids, and `202 Accepted` (the actions run out-of-band on the worker).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;

use catalerum_automation::TriggerEvent;
use catalerum_core::capability::Action;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the webhook trigger route.
pub fn router() -> Router<AppState> {
    Router::new().route("/webhooks/{*path}", post(fire))
}

/// The result of firing a webhook: how many automations matched and the durable
/// `run_automation` jobs enqueued for them.
#[derive(Debug, Serialize)]
pub struct WebhookResult {
    /// Number of enabled automations whose trigger matched this path.
    pub matched: usize,
    /// The enqueued `run_automation` job ids (one per matched automation).
    pub jobs: Vec<uuid::Uuid>,
}

async fn fire(
    State(state): State<AppState>,
    auth: Auth,
    Path(path): Path<String>,
) -> ApiResult<(StatusCode, Json<WebhookResult>)> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let path = normalize_path(&path)?;
    let event = TriggerEvent::Webhook { path };
    let jobs = catalerum_ingest::dispatch_trigger_event(state.store(), p.workspace_id, &event)
        .await
        .map_err(|e| ApiError::internal(format!("dispatching webhook automations: {e}")))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(WebhookResult {
            matched: jobs.len(),
            jobs,
        }),
    ))
}

/// Normalize the captured URL path into the trigger `path` it matches against:
/// exactly one leading slash, no trailing slash. `POST /webhooks/run` and
/// `/webhooks//run/` both match a trigger authored as `{ "path": "/run" }`. An
/// empty path is rejected (`400`) — a webhook trigger always names a path.
fn normalize_path(raw: &str) -> ApiResult<String> {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        return Err(ApiError::bad_request("webhook path must not be empty"));
    }
    Ok(format!("/{trimmed}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_forces_a_single_leading_slash() {
        assert_eq!(normalize_path("run").unwrap(), "/run");
        assert_eq!(normalize_path("/run").unwrap(), "/run");
        assert_eq!(normalize_path("run/").unwrap(), "/run");
        assert_eq!(normalize_path("//run//").unwrap(), "/run");
        // Multi-segment catch-all paths are preserved (inner slashes kept).
        assert_eq!(normalize_path("ci/deploy/done").unwrap(), "/ci/deploy/done");
    }

    #[test]
    fn normalize_path_rejects_empty() {
        assert!(normalize_path("").is_err());
        assert!(normalize_path("   ").is_err());
        assert!(normalize_path("/").is_err());
        assert!(normalize_path("///").is_err());
    }
}
