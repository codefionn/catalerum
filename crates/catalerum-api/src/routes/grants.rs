//! §19 capability-grant management (SOUL §19/§13) — define and manage the named
//! capability bundles an automation (later, an agent) runs **under**. A grant is
//! `{ name, capabilities, constraints }`; it confers an explicit, attenuated
//! authority rather than a role's full base set.
//!
//! - `POST   /grants`      create-or-replace a grant (idempotent by name)
//! - `GET    /grants`      list the workspace's grants
//! - `GET    /grants/{id}` fetch one grant
//! - `DELETE /grants/{id}` remove a grant (an automation referencing it is detached)
//!
//! **Admin-only:** managing authorization config is gated on `grant:read`/`write`,
//! which no base role implies (`grant` is not a member domain) — only an Owner/Admin
//! `*` covers it (deny-by-default, §19). Every route is workspace-scoped (§18).
//!
//! **Definitions only (for now):** this surface persists grants; the runtime
//! **enforcement** — the action runner resolving an automation's `grant_id` into its
//! `ToolContext` capabilities — lands in a follow-up slice. `CreateAutomation` still
//! doesn't accept a `grant_id` (the policy engine assigns it).
//!
//! **`POST` is create-or-replace** (idempotent by `(workspace, name)`, keeping the
//! id). Re-posting a name **mutates that grant's capabilities in place** — once
//! enforcement lands, any automation already bound to it runs under the new set
//! without re-binding. The enforcement slice should decide whether widening an
//! in-use grant must be an explicit, audited `PUT` rather than a silent replace.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::{attenuate, Action, Capability, Constraints};
use catalerum_core::model::Grant;
use catalerum_core::GrantId;
use catalerum_iam::base_capabilities;
use catalerum_store::StoreError;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the grant-management routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/grants", post(create).get(list))
        .route("/grants/{id}", get(get_one).delete(delete))
}

/// Body for `POST /grants`. `capabilities` is the bundle the grant confers;
/// `constraints` are its global limits (both default to empty).
#[derive(Debug, Deserialize)]
pub struct CreateGrant {
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub constraints: Constraints,
}

fn map_grant_err(e: StoreError) -> ApiError {
    match e {
        StoreError::NotFound => ApiError::NotFound,
        other => {
            tracing::error!(error = %other, "grant lookup");
            ApiError::internal("grant lookup failed")
        }
    }
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateGrant>,
) -> ApiResult<(StatusCode, Json<Grant>)> {
    let p = auth.principal();
    auth.require(Action::Write, "grant")?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("grant name must not be empty"));
    }
    // §19 attenuation: a grant may never confer more than the creator's own
    // authority. This is a no-op for an Owner/Admin (their `*` covers everything,
    // and only they reach this gate today), but enforcing it explicitly keeps the
    // invariant safe if grant-creation is ever opened to a non-`*` role — a
    // wildcard-grant escalation would otherwise live here.
    let base = base_capabilities(p.role);
    for cap in &body.capabilities {
        if attenuate(&base, cap).is_err() {
            return Err(ApiError::Forbidden(
                "grant capability exceeds your own authority".to_string(),
            ));
        }
    }
    let grant = state
        .store()
        .grants()
        .upsert(p.workspace_id, name, &body.capabilities, &body.constraints)
        .await
        .map_err(|e| {
            // Log the detail server-side; never leak SQL/Postgres text to the client.
            tracing::error!(error = %e, "creating grant");
            ApiError::internal("creating grant failed")
        })?;
    Ok((StatusCode::CREATED, Json(grant)))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Grant>>> {
    let p = auth.principal();
    auth.require(Action::Read, "grant")?;
    let grants = state
        .store()
        .grants()
        .list_by_workspace(p.workspace_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "listing grants");
            ApiError::internal("listing grants failed")
        })?;
    Ok(Json(grants))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Json<Grant>> {
    let p = auth.principal();
    auth.require(Action::Read, "grant")?;
    let id: GrantId = id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid grant id"))?;
    let grant = state
        .store()
        .grants()
        .get(p.workspace_id, id)
        .await
        .map_err(map_grant_err)?;
    Ok(Json(grant))
}

async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "grant")?;
    let id: GrantId = id
        .parse()
        .map_err(|_| ApiError::bad_request("invalid grant id"))?;
    state
        .store()
        .grants()
        .delete(p.workspace_id, id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "deleting grant");
            ApiError::internal("deleting grant failed")
        })?;
    Ok(StatusCode::NO_CONTENT)
}
