//! Management REST for user-authored, Boa-scripted MCP endpoints (SOUL §26).
//!
//! CRUD over the caller's workspace endpoints, plus minting a signed, shareable
//! **scoped token** so an external agent can reach one endpoint's tools with no
//! login (verified by `POST /mcp/s/{token}`). Everything is workspace-scoped: the
//! principal only ever sees / edits / deletes endpoints in its own workspace, and
//! a minted token is bound to that workspace + endpoint.
//!
//! **Authorization (SOUL §19):** every handler is capability-gated on the `mcp`
//! domain, mirroring `routes::mcp_servers` — reads need `mcp:read`, writes need
//! `mcp:write` **and** a workspace-administrator role (no base role implies the
//! `mcp` domain, so in practice management is Owner/Admin-only, and a
//! grant-scoped token can never write). Pinning a `grant_id` to an endpoint is
//! attenuation-checked: the grant's capabilities must be ⊆ the caller's own
//! effective authority (never an escalation).
//!
//! Minted share tokens are HMAC-signed **and** recorded server-side (hash only),
//! so they are individually revocable (`DELETE …/tokens/{token_id}`) and die
//! with their endpoint.
//!
//! - `GET  /mcp-endpoints`          — list the workspace's endpoints
//! - `POST /mcp-endpoints`          — create one (authored by the caller)
//! - `GET  /mcp-endpoints/{id}`     — fetch one
//! - `PUT  /mcp-endpoints/{id}`     — update one
//! - `DELETE /mcp-endpoints/{id}`   — delete one
//! - `POST /mcp-endpoints/{id}/token` — mint a scoped token (shown once)
//! - `GET  /mcp-endpoints/{id}/tokens` — list the endpoint's minted tokens
//! - `DELETE /mcp-endpoints/{id}/tokens/{token_id}` — revoke one

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::model::{Author, Grant, McpEndpoint};
use catalerum_core::{GrantId, McpEndpointId, WorkspaceId};
use catalerum_store::{McpEndpointInput, McpEndpointToken};

use crate::auth::{grant_within_authority, Auth};
use crate::error::{ApiError, ApiResult};
use crate::mcp_endpoint_link::EndpointClaims;
use crate::state::AppState;

/// Default scoped-token lifetime (days) when the client omits one.
const DEFAULT_TOKEN_TTL_DAYS: i64 = 90;
/// Maximum scoped-token lifetime (days) — a year.
const MAX_TOKEN_TTL_DAYS: i64 = 365;

/// Mount the management routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mcp-endpoints", get(list).post(create))
        .route(
            "/mcp-endpoints/{id}",
            get(get_one).put(update).delete(delete),
        )
        .route("/mcp-endpoints/{id}/token", post(mint_token))
        .route("/mcp-endpoints/{id}/tokens", get(list_tokens))
        .route(
            "/mcp-endpoints/{id}/tokens/{token_id}",
            axum::routing::delete(revoke_token),
        )
}

/// The capability gate every handler passes first (SOUL §19): reads hold
/// `mcp:read`, writes hold `mcp:write` **and** a workspace-administrator role —
/// the exact posture of `routes::mcp_servers`. No base role implies the `mcp`
/// domain, so in practice this surface is Owner/Admin-only.
fn require_read(auth: &Auth) -> ApiResult<()> {
    auth.require(Action::Read, "mcp")
}

/// The write gate: `mcp:write` plus workspace-admin (which a grant-scoped token
/// never passes — a scoped token is strictly less than its minter, SOUL §19).
fn require_write(auth: &Auth) -> ApiResult<()> {
    auth.require(Action::Write, "mcp")?;
    auth.require_workspace_admin()
}

/// Resolve + attenuation-check a `grant_id` an endpoint should be pinned to
/// (SOUL §19): the grant must exist **in this workspace** (a foreign id is a
/// `400`, never a dangling cross-workspace pin) and its capabilities must be ⊆
/// the caller's own effective authority — an endpoint can never run under more
/// authority than the principal pinning the grant.
async fn resolve_pin_grant(
    state: &AppState,
    auth: &Auth,
    workspace_id: WorkspaceId,
    grant_id: Option<GrantId>,
) -> ApiResult<Option<GrantId>> {
    let Some(gid) = grant_id else {
        return Ok(None);
    };
    let grant: Grant = state
        .store()
        .grants()
        .get(workspace_id, gid)
        .await
        .map_err(|_| ApiError::bad_request("grant not found in this workspace"))?;
    if !grant_within_authority(&auth.capabilities(), &grant) {
        return Err(ApiError::Forbidden(
            "grant exceeds your own authority; an endpoint cannot be pinned to it".to_string(),
        ));
    }
    Ok(Some(grant.id))
}

/// Create/update body. `name` is a workspace-unique URL slug; `bucket_name` +
/// `key_prefix` pin the endpoint's search scope; `grant_id` is the optional §19
/// authority the script runs under.
#[derive(Debug, Default, Deserialize)]
pub struct EndpointBody {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub script: String,
    #[serde(default)]
    pub bucket_name: Option<String>,
    #[serde(default)]
    pub key_prefix: Option<String>,
    #[serde(default)]
    pub grant_id: Option<uuid::Uuid>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

impl EndpointBody {
    fn into_input(self) -> McpEndpointInput {
        McpEndpointInput {
            name: self.name,
            description: self.description,
            script: self.script,
            bucket_name: self.bucket_name,
            key_prefix: self.key_prefix,
            grant_id: self.grant_id.map(GrantId::from_uuid),
            enabled: self.enabled,
        }
    }
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<McpEndpoint>>> {
    require_read(&auth)?;
    let p = auth.principal();
    let endpoints = state
        .store()
        .mcp_endpoints()
        .list_by_workspace(p.workspace_id)
        .await?;
    Ok(Json(endpoints))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<EndpointBody>,
) -> ApiResult<(StatusCode, Json<McpEndpoint>)> {
    require_write(&auth)?;
    let p = auth.principal();
    let mut input = body.into_input();
    // Attenuation-gate the grant pin before it is persisted (SOUL §19).
    input.grant_id = resolve_pin_grant(&state, &auth, p.workspace_id, input.grant_id).await?;
    let author = Author::User { id: p.user_id };
    let endpoint = state
        .store()
        .mcp_endpoints()
        .create(p.workspace_id, author, &input)
        .await?;
    Ok((StatusCode::CREATED, Json(endpoint)))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<McpEndpoint>> {
    require_read(&auth)?;
    let p = auth.principal();
    let endpoint = state
        .store()
        .mcp_endpoints()
        .get(p.workspace_id, McpEndpointId::from_uuid(id))
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(Json(endpoint))
}

async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<EndpointBody>,
) -> ApiResult<Json<McpEndpoint>> {
    require_write(&auth)?;
    let p = auth.principal();
    let mut input = body.into_input();
    // Attenuation-gate the grant pin before it is persisted (SOUL §19).
    input.grant_id = resolve_pin_grant(&state, &auth, p.workspace_id, input.grant_id).await?;
    let endpoint = state
        .store()
        .mcp_endpoints()
        .update(p.workspace_id, McpEndpointId::from_uuid(id), &input)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(Json(endpoint))
}

async fn delete(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<StatusCode> {
    require_write(&auth)?;
    let p = auth.principal();
    state
        .store()
        .mcp_endpoints()
        .delete(p.workspace_id, McpEndpointId::from_uuid(id))
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body for minting a scoped token.
#[derive(Debug, Default, Deserialize)]
pub struct MintToken {
    /// Requested lifetime in days (clamped to `[1, 365]`, default 90).
    #[serde(default)]
    pub ttl_days: Option<i64>,
}

/// Response for a minted scoped token — the raw token + the path to POST it to.
#[derive(Debug, Serialize)]
pub struct MintedToken {
    /// The opaque scoped token. Shown once; carry it in the `/mcp/s/{token}` path.
    pub token: String,
    /// The ready-to-use serve path (`/mcp/s/{token}`).
    pub path: String,
    /// Expiry, Unix seconds.
    pub expires_at: i64,
}

async fn mint_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
    body: Option<Json<MintToken>>,
) -> ApiResult<Json<MintedToken>> {
    require_write(&auth)?;
    let p = auth.principal();
    // Confirm the endpoint exists in the caller's workspace before minting a token
    // bound to its name (a token for another workspace's endpoint is unmintable).
    let endpoint = state
        .store()
        .mcp_endpoints()
        .get(p.workspace_id, McpEndpointId::from_uuid(id))
        .await
        .map_err(|_| ApiError::NotFound)?;

    let ttl_days = body
        .and_then(|Json(b)| b.ttl_days)
        .unwrap_or(DEFAULT_TOKEN_TTL_DAYS)
        .clamp(1, MAX_TOKEN_TTL_DAYS);
    let exp = chrono::Utc::now().timestamp() + ttl_days * 86_400;
    let token = state.endpoint_signer().mint(&EndpointClaims {
        workspace_id: p.workspace_id,
        endpoint: endpoint.name.clone(),
        exp,
    });
    // Record the token (hash only) so it is individually revocable and dies with
    // its endpoint; the serve path requires this live row in addition to a valid
    // signature. The expiry is recorded to the second, matching the claims.
    let expires_at = chrono::DateTime::from_timestamp(exp, 0)
        .ok_or_else(|| ApiError::internal("token expiry out of range"))?;
    state
        .store()
        .mcp_endpoint_tokens()
        .create(
            p.workspace_id,
            endpoint.id,
            &catalerum_iam::token::hash_token(&token),
            expires_at,
        )
        .await?;
    let path = format!("/mcp/s/{token}");
    Ok(Json(MintedToken {
        token,
        path,
        expires_at: exp,
    }))
}

/// A minted share token as shown in a listing — id + timestamps + revocation
/// state. The raw token is never included (only a hash is stored; SOUL §13).
#[derive(Debug, Serialize)]
pub struct EndpointTokenView {
    /// The token row id (the handle used to revoke).
    pub id: uuid::Uuid,
    /// When the token was minted.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When it expires.
    pub expires_at: chrono::DateTime<chrono::Utc>,
    /// When it was revoked, if it has been.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<McpEndpointToken> for EndpointTokenView {
    fn from(t: McpEndpointToken) -> Self {
        Self {
            id: t.id,
            created_at: t.created_at,
            expires_at: t.expires_at,
            revoked_at: t.revoked_at,
        }
    }
}

/// `GET /mcp-endpoints/{id}/tokens` — the endpoint's minted share tokens
/// (newest first), so the panel can show + revoke them.
async fn list_tokens(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<uuid::Uuid>,
) -> ApiResult<Json<Vec<EndpointTokenView>>> {
    require_read(&auth)?;
    let p = auth.principal();
    let endpoint_id = McpEndpointId::from_uuid(id);
    // Existence check keeps a foreign endpoint id indistinguishable (404).
    state
        .store()
        .mcp_endpoints()
        .get(p.workspace_id, endpoint_id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let tokens = state
        .store()
        .mcp_endpoint_tokens()
        .list_by_endpoint(p.workspace_id, endpoint_id)
        .await?;
    Ok(Json(tokens.into_iter().map(Into::into).collect()))
}

/// `DELETE /mcp-endpoints/{id}/tokens/{token_id}` — revoke one share token,
/// immediately: the serve path (`POST /mcp/s/{token}`) requires a live row, so
/// the token stops working at once, not at expiry. Idempotent.
async fn revoke_token(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, token_id)): Path<(uuid::Uuid, uuid::Uuid)>,
) -> ApiResult<StatusCode> {
    require_write(&auth)?;
    let p = auth.principal();
    state
        .store()
        .mcp_endpoint_tokens()
        .revoke(p.workspace_id, McpEndpointId::from_uuid(id), token_id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::capability::{Capability, Resource};
    use catalerum_core::model::Role;
    use catalerum_core::UserId;

    fn auth(role: Role) -> Auth {
        Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            role,
        ))
    }

    #[test]
    fn read_gate_is_owner_admin_only_in_practice() {
        // No base role implies the `mcp` domain: Owner/Admin pass via their
        // wildcard; Member/Viewer are denied (deny-by-default, SOUL §19) — the
        // exact posture of `routes::mcp_servers`.
        assert!(require_read(&auth(Role::Owner)).is_ok());
        assert!(require_read(&auth(Role::Admin)).is_ok());
        assert!(matches!(
            require_read(&auth(Role::Member)),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_read(&auth(Role::Viewer)),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn write_gate_denies_members_viewers_and_grant_scoped_tokens() {
        assert!(require_write(&auth(Role::Owner)).is_ok());
        assert!(require_write(&auth(Role::Admin)).is_ok());
        assert!(matches!(
            require_write(&auth(Role::Member)),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            require_write(&auth(Role::Viewer)),
            Err(ApiError::Forbidden(_))
        ));

        // A grant-scoped token never writes — even one minted by an Owner with
        // an `mcp:write` capability (workspace-admin is role-derived, SOUL §19).
        let ws = WorkspaceId::new();
        let grant = Grant {
            id: GrantId::new(),
            workspace_id: ws,
            name: "mcp-write".into(),
            capabilities: vec![Capability::new(Action::Write, Resource::domain("mcp"))],
            constraints: Default::default(),
        };
        let scoped = Auth::with_grant(
            catalerum_iam::Principal::new(UserId::new(), ws, Role::Owner),
            grant,
        );
        assert!(matches!(
            require_write(&scoped),
            Err(ApiError::Forbidden(_))
        ));
    }

    #[test]
    fn endpoint_body_grant_id_round_trips_into_input() {
        let gid = uuid::Uuid::new_v4();
        let body: EndpointBody = serde_json::from_str(&format!(
            r#"{{"name":"wiki","script":"// x","grant_id":"{gid}"}}"#
        ))
        .unwrap();
        let input = body.into_input();
        assert_eq!(input.grant_id, Some(GrantId::from_uuid(gid)));
        // Absent grant → None (the serve-time read-only default applies).
        let body: EndpointBody = serde_json::from_str(r#"{"name":"wiki"}"#).unwrap();
        assert_eq!(body.into_input().grant_id, None);
    }
}
