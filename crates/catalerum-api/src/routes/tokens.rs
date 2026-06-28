//! API-key (bearer token) management REST surface (SOUL §18) — the Settings
//! "API keys" panel.
//!
//! A "token" is a workspace-scoped bearer session bound to `{user, workspace,
//! role, ttl}` — the same primitive the dev magic-link and `catalerum token`
//! mint. These routes let a signed-in user manage **their own** long-lived
//! tokens for scripting / CI / MCP clients:
//!
//! - `GET /tokens` — list the caller's active tokens in the current workspace
//! - `POST /tokens` — issue a new token (the raw secret is returned **once**)
//! - `DELETE /tokens/{id}` — revoke one of the caller's tokens by id
//!
//! Everything is **self-scoped**: the principal only ever sees, mints, or revokes
//! tokens for its own `user_id` in its own `workspace_id` — there is no path to
//! another principal's tokens (a token for another user/workspace 404s on delete,
//! and the listing filters to the caller's workspace). The stored token is a
//! SHA-256 **hash** (SOUL §13), so a listing carries only id + timestamps — the
//! raw secret exists only in the create response and is never recoverable after.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::model::Grant;
use catalerum_core::{GrantId, WorkspaceId};
use catalerum_store::StoreError;

use crate::auth::{grant_within_authority, Auth};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Default token lifetime (days) when the client omits one.
const DEFAULT_TTL_DAYS: i64 = 90;
/// Maximum token lifetime (days) — a year, matching the `catalerum token` CLI cap.
const MAX_TTL_DAYS: i64 = 365;

/// Mount the token routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{id}", axum::routing::delete(revoke_token))
}

/// A token as shown in a listing — id + timestamps (+ any bound grant). The
/// secret is never included (only a hash is stored; SOUL §13).
#[derive(Debug, Serialize)]
pub struct TokenView {
    /// The session row id (the handle used to revoke).
    pub id: uuid::Uuid,
    /// The named §19 grant this token is scoped to, if any (SOUL §19/§26) — the
    /// token then acts under the grant's attenuated authority rather than the
    /// caller's full role. `None` = a role-derived token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// When the token was issued.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the token expires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Body for `POST /tokens`. `ttl_days` defaults to [`DEFAULT_TTL_DAYS`] and is
/// clamped to `[1, 365]`.
#[derive(Debug, Default, Deserialize)]
pub struct CreateToken {
    /// Requested lifetime in days.
    #[serde(default)]
    pub ttl_days: Option<i64>,
    /// Optionally **scope** the token to a named §19 grant (SOUL §19/§26), by
    /// grant id or name. The minted bearer then carries the grant's attenuated
    /// authority — the mint is gated so the grant must be ⊆ the caller's own
    /// authority (never an escalation). Absent = today's role-derived token.
    #[serde(default)]
    pub grant: Option<String>,
}

/// Response for `POST /tokens` — the raw token, shown **once** (it is not
/// recoverable afterwards).
#[derive(Debug, Serialize)]
pub struct CreatedToken {
    /// The raw bearer secret. Copy it now — it is never shown again.
    pub token: String,
    /// The named §19 grant this token was scoped to, if any (echoes the request).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// When it was issued.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it expires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Clamp a requested TTL (or the default when absent) into the allowed range.
fn resolve_ttl_days(requested: Option<i64>) -> i64 {
    requested.unwrap_or(DEFAULT_TTL_DAYS).clamp(1, MAX_TTL_DAYS)
}

async fn list_tokens(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<TokenView>>> {
    let p = auth.principal();
    let sessions = state.store().sessions().list_by_user(p.user_id).await?;
    // Resolve grant ids → names for this workspace so the listing can label a
    // scoped token. Best-effort: if the grants list can't be read, names are
    // simply omitted (the token still lists). Self-scoped throughout.
    let grant_names: HashMap<GrantId, String> = state
        .store()
        .grants()
        .list_by_workspace(p.workspace_id)
        .await
        .map(|gs| gs.into_iter().map(|g| (g.id, g.name)).collect())
        .unwrap_or_default();
    let views = sessions
        .into_iter()
        // Self-scoped: only this workspace's tokens (the session list is per-user
        // across all workspaces the user belongs to).
        .filter(|s| s.workspace_id() == p.workspace_id)
        .map(|s| TokenView {
            id: s.id,
            grant: s.grant_id().and_then(|gid| grant_names.get(&gid).cloned()),
            created_at: s.created_at,
            expires_at: s.expires_at,
        })
        .collect();
    Ok(Json(views))
}

/// Resolve a `POST /tokens` `grant` reference (a grant id **or** name) to a grant
/// in the caller's workspace. A grant that doesn't exist here is a `400` (never a
/// cross-workspace grant — the lookups are all workspace-scoped, SOUL §18).
async fn resolve_grant_ref(
    state: &AppState,
    workspace_id: WorkspaceId,
    reference: &str,
) -> ApiResult<Grant> {
    let reference = reference.trim();
    // A grant id wins when the reference parses as one and exists here.
    if let Ok(id) = reference.parse::<GrantId>() {
        match state.store().grants().get(workspace_id, id).await {
            Ok(g) => return Ok(g),
            Err(StoreError::NotFound) => {} // fall through to a name match
            Err(e) => {
                tracing::error!(error = %e, "resolving token grant by id");
                return Err(ApiError::internal("resolving grant failed"));
            }
        }
    }
    let grants = state
        .store()
        .grants()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "listing grants for token mint");
            ApiError::internal("resolving grant failed")
        })?;
    grants
        .into_iter()
        .find(|g| g.name == reference)
        .ok_or_else(|| ApiError::bad_request("grant not found in this workspace"))
}

async fn create_token(
    State(state): State<AppState>,
    auth: Auth,
    body: Option<Json<CreateToken>>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let p = auth.principal();
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let ttl_days = resolve_ttl_days(body.ttl_days);

    // Optionally scope the token to a named §19 grant (SOUL §19/§26). The mint is
    // **attenuation-gated**: a scoped token is strictly *less* than the caller,
    // never more (preserving the self-scoped/no-escalation invariant, SOUL §18).
    let grant = match body.grant {
        Some(ref reference) if !reference.trim().is_empty() => {
            let grant = resolve_grant_ref(&state, p.workspace_id, reference).await?;
            // §19 attenuation: the grant's capabilities must be ⊆ the CALLER's own
            // *effective* authority — the grant caps if the caller is itself
            // grant-scoped, else the role's base set. Never trust the grant row
            // alone: the caller could have been demoted (or is itself attenuated)
            // since the grant was defined. This mirrors how `routes::grants` /
            // `routes::agent_profiles` gate a grant binding.
            let ceiling = auth.capabilities();
            if !grant_within_authority(&ceiling, &grant) {
                return Err(ApiError::Forbidden(
                    "grant exceeds your own authority; a token cannot widen it".to_string(),
                ));
            }
            Some(grant)
        }
        _ => None,
    };
    let grant_id = grant.as_ref().map(|g| g.id);
    let grant_name = grant.map(|g| g.name);

    // Issue under the caller's role, optionally scoped to the grant — a token can
    // never carry more authority than the principal that minted it (SOUL §18/§19).
    let session = state
        .iam()
        .issue_session_with_ttl_days(p.workspace_id, p.user_id, p.role, grant_id, ttl_days)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            token: session.token,
            grant: grant_name,
            created_at: session.created_at,
            expires_at: session.expires_at,
        }),
    ))
}

async fn revoke_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    let sessions = state.store().sessions();
    // Resolve the token and confirm it belongs to the caller before deleting —
    // a token for another user/workspace is reported as not-found (never leaked).
    let session = sessions.get(id).await.map_err(|_| ApiError::NotFound)?;
    if session.user_id() != p.user_id || session.workspace_id() != p.workspace_id {
        return Err(ApiError::NotFound);
    }
    sessions.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_clamps_and_defaults() {
        assert_eq!(resolve_ttl_days(None), DEFAULT_TTL_DAYS);
        assert_eq!(resolve_ttl_days(Some(7)), 7);
        assert_eq!(resolve_ttl_days(Some(0)), 1);
        assert_eq!(resolve_ttl_days(Some(-5)), 1);
        assert_eq!(resolve_ttl_days(Some(99999)), MAX_TTL_DAYS);
    }

    fn grant_with(caps: Vec<catalerum_core::capability::Capability>) -> Grant {
        Grant {
            id: catalerum_core::GrantId::new(),
            workspace_id: catalerum_core::WorkspaceId::new(),
            name: "g".into(),
            capabilities: caps,
            constraints: Default::default(),
        }
    }

    #[test]
    fn mint_gate_rejects_a_grant_that_exceeds_the_callers_authority() {
        use catalerum_core::capability::{Action, Capability, Resource};
        use catalerum_core::model::Role;
        use catalerum_iam::base_capabilities;

        let notes_write = grant_with(vec![Capability::new(
            Action::Write,
            Resource::domain("notes"),
        )]);
        let notes_delete = grant_with(vec![Capability::new(
            Action::Delete,
            Resource::domain("notes"),
        )]);

        // An Owner (`*`) may bind either grant — both are ⊆ its authority.
        let owner = base_capabilities(Role::Owner);
        assert!(grant_within_authority(&owner, &notes_write));
        assert!(grant_within_authority(&owner, &notes_delete));

        // A Member holds `notes:write` but NOT `notes:delete` (a protected scope,
        // §19): it may bind the write grant, but binding the delete grant is
        // rejected (surfaces as `403` at the route).
        let member = base_capabilities(Role::Member);
        assert!(grant_within_authority(&member, &notes_write));
        assert!(!grant_within_authority(&member, &notes_delete));

        // A Viewer (read-only) cannot bind even the write grant.
        let viewer = base_capabilities(Role::Viewer);
        assert!(!grant_within_authority(&viewer, &notes_write));

        // A grant-scoped caller's ceiling is the grant's OWN caps (not a role): a
        // `notes:write`-only ceiling cannot mint a `notes:delete` token — a scoped
        // token can never widen itself.
        let scoped_ceiling = notes_write.capabilities.clone();
        assert!(!grant_within_authority(&scoped_ceiling, &notes_delete));
        assert!(grant_within_authority(&scoped_ceiling, &notes_write));
    }
}
