//! Dev magic-link auth (SOUL §17/§18).
//!
//! `GET /auth/magic?token=...` redeems a one-time dev login token (minted by
//! [`catalerum_iam::IamService::ensure_dev_login`]/`issue_login_token`) and
//! establishes a session.
//!
//! **Session delivery contract: two-step handoff, never a bearer in a URL.**
//! After consuming the one-time token the endpoint mints a short-lived
//! (5-minute) one-time **handoff code** and **302-redirects to the web
//! workbench** (`[server].web_url`, default `http://localhost:8080`) with
//! `?code=<handoff>`. The SPA scrubs the code from its URL and exchanges it
//! over `POST /auth/exchange` for the real session bearer (returned in a JSON
//! body) — so the long-lived session token never appears in a URL, where it
//! would linger in browser history, referers, and access logs.
//!
//! Non-browser clients (curl, the e2e harness) can opt out of the redirect and
//! get the session as JSON ([`SessionResponse`]) directly by appending
//! `&format=json`.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::WorkspaceId;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the auth routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/magic", get(magic))
        .route("/auth/setup", get(setup_status).post(setup))
        .route("/auth/password", post(password_login))
        .route("/users", get(list_users).post(create_user))
        .route("/users/{id}/password", post(reset_password))
        .route("/workspaces", get(list_workspaces))
        .route("/auth/switch", post(switch_workspace))
        .route("/auth/exchange", post(exchange_handoff))
        .route("/auth/logout", post(logout))
}

#[derive(Debug, Serialize)]
pub struct SetupStatus {
    pub enabled: bool,
    pub required: bool,
}

async fn setup_status(State(state): State<AppState>) -> ApiResult<Json<SetupStatus>> {
    let enabled = state.config().auth.password_login;
    let required = if enabled {
        state.store().password_auth().setup_required().await?
    } else {
        false
    };
    Ok(Json(SetupStatus { enabled, required }))
}

#[derive(Debug, Deserialize)]
pub struct SetupRequest {
    pub email: String,
    pub display_name: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct PasswordLoginRequest {
    pub email: String,
    pub password: String,
}

fn validate_password(password: &str) -> ApiResult<()> {
    if password.len() < 12 {
        return Err(ApiError::bad_request(
            "password must contain at least 12 characters",
        ));
    }
    if password.len() > 1024 {
        return Err(ApiError::bad_request("password is too long"));
    }
    Ok(())
}

async fn hash_password(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || {
        let mut salt_bytes = [0_u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut salt_bytes)
            .map_err(|_| ApiError::internal("password salt generation failed"))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| ApiError::internal(format!("password salt encoding failed: {e}")))?;
        argon2::Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|e| ApiError::internal(format!("password hashing failed: {e}")))
    })
    .await
    .map_err(|e| ApiError::internal(format!("password task failed: {e}")))?
}

async fn verify_password(password: String, encoded: String) -> bool {
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&encoded).is_ok_and(|hash| {
            argon2::Argon2::default()
                .verify_password(password.as_bytes(), &hash)
                .is_ok()
        })
    })
    .await
    .unwrap_or(false)
}

async fn setup(
    State(state): State<AppState>,
    Json(body): Json<SetupRequest>,
) -> ApiResult<Json<SessionResponse>> {
    if !state.config().auth.password_login {
        return Err(ApiError::NotFound);
    }
    let email = body.email.trim();
    let name = body.display_name.trim();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::bad_request("a valid email address is required"));
    }
    if name.is_empty() {
        return Err(ApiError::bad_request("display_name is required"));
    }
    validate_password(&body.password)?;
    let hash = hash_password(body.password).await?;
    let account = state
        .store()
        .password_auth()
        .bootstrap(email, name, &hash)
        .await?;
    let session = state
        .iam()
        .issue_session(account.workspace_id, account.user_id)
        .await?;
    Ok(Json(session_response(session)))
}

async fn password_login(
    State(state): State<AppState>,
    Json(body): Json<PasswordLoginRequest>,
) -> ApiResult<Json<SessionResponse>> {
    if !state.config().auth.password_login {
        return Err(ApiError::NotFound);
    }
    // Never permit login before the instance owner has atomically completed setup.
    if state.store().password_auth().setup_required().await? {
        return Err(ApiError::Conflict("instance setup is required".to_string()));
    }
    let account = state
        .store()
        .password_auth()
        .get_by_email(body.email.trim())
        .await
        .map_err(|_| ApiError::unauthorized("invalid email or password"))?;
    if !verify_password(body.password, account.password_hash).await {
        return Err(ApiError::unauthorized("invalid email or password"));
    }
    let session = state
        .iam()
        .issue_session(
            WorkspaceId::from_uuid(account.workspace_id),
            catalerum_core::UserId::from_uuid(account.user_id),
        )
        .await?;
    Ok(Json(session_response(session)))
}

fn session_response(session: catalerum_iam::Session) -> SessionResponse {
    SessionResponse {
        token: session.token,
        workspace_id: session.workspace_id,
        user_id: session.user_id,
        role: catalerum_iam::role_str(session.role).to_string(),
        expires_at: session.expires_at,
    }
}

#[derive(Debug, Serialize)]
struct ManagedUser {
    id: catalerum_core::UserId,
    email: String,
    display_name: String,
    role: String,
}

async fn list_users(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<ManagedUser>>> {
    auth.require_workspace_admin()?;
    let workspace_id = auth.principal().workspace_id;
    let users = state
        .store()
        .users()
        .list_by_workspace(workspace_id)
        .await?;
    let roles: std::collections::HashMap<_, _> = state
        .store()
        .memberships()
        .list_by_workspace(workspace_id)
        .await?
        .into_iter()
        .map(|membership| (membership.user_id, membership.role))
        .collect();
    Ok(Json(
        users
            .into_iter()
            .filter_map(|user| {
                roles.get(&user.id).map(|role| ManagedUser {
                    id: user.id,
                    email: user.email,
                    display_name: user.display_name,
                    role: catalerum_iam::role_str(*role).to_string(),
                })
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    email: String,
    display_name: String,
    password: String,
    #[serde(default = "member_role")]
    role: String,
}

fn member_role() -> String {
    "member".to_string()
}

async fn create_user(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateUserRequest>,
) -> ApiResult<(StatusCode, Json<ManagedUser>)> {
    auth.require_workspace_admin()?;
    if !state.config().auth.password_login {
        return Err(ApiError::NotFound);
    }
    validate_password(&body.password)?;
    let role = catalerum_iam::role_from_str(body.role.trim())
        .map_err(|_| ApiError::bad_request("role must be owner, admin, member or viewer"))?;
    let email = body.email.trim();
    let display_name = body.display_name.trim();
    if email.is_empty() || !email.contains('@') || display_name.is_empty() {
        return Err(ApiError::bad_request("email and display_name are required"));
    }
    let hash = hash_password(body.password).await?;
    let id = state
        .store()
        .password_auth()
        .create_user(
            auth.principal().workspace_id,
            email,
            display_name,
            catalerum_iam::role_str(role),
            &hash,
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(ManagedUser {
            id,
            email: email.to_string(),
            display_name: display_name.to_string(),
            role: catalerum_iam::role_str(role).to_string(),
        }),
    ))
}

#[derive(Debug, Deserialize)]
struct ResetPasswordRequest {
    password: String,
}

async fn reset_password(
    State(state): State<AppState>,
    auth: Auth,
    Path(user_id): Path<catalerum_core::UserId>,
    Json(body): Json<ResetPasswordRequest>,
) -> ApiResult<StatusCode> {
    auth.require_workspace_admin()?;
    if !state.config().auth.password_login {
        return Err(ApiError::NotFound);
    }
    validate_password(&body.password)?;
    // Scope the operation: an admin can reset only a member of this workspace.
    state
        .store()
        .memberships()
        .get(auth.principal().workspace_id, user_id)
        .await?;
    let hash = hash_password(body.password).await?;
    state
        .store()
        .password_auth()
        .set_password(user_id, &hash)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query string for `GET /auth/magic`.
#[derive(Debug, Deserialize)]
pub struct MagicQuery {
    /// The one-time login token from the magic link.
    pub token: String,
    /// Response format. Default (absent / anything but `json`) 302-redirects
    /// into the SPA; `json` returns [`SessionResponse`] for programmatic clients.
    #[serde(default)]
    pub format: Option<String>,
}

/// JSON body returned on a successful magic-link redemption when `?format=json`
/// is requested. `token` is the session bearer the client must send on
/// subsequent requests.
#[derive(Debug, Serialize)]
pub struct SessionResponse {
    /// The session bearer token (send as `Authorization: Bearer <token>`).
    pub token: String,
    pub workspace_id: catalerum_core::WorkspaceId,
    pub user_id: catalerum_core::UserId,
    /// Lowercase role token: `owner` | `admin` | `member` | `viewer`.
    pub role: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

async fn magic(State(state): State<AppState>, Query(q): Query<MagicQuery>) -> ApiResult<Response> {
    if q.token.trim().is_empty() {
        return Err(ApiError::bad_request("empty token"));
    }
    // Consume the one-time magic token; what happens next depends on the client.
    let binding = state.iam().consume_login_token(&q.token).await?;

    // Programmatic opt-out: issue + return the session as JSON (curl, e2e
    // harness) — no redirect, no URL involved.
    if q.format.as_deref() == Some("json") {
        let session = state
            .iam()
            .issue_session(binding.workspace_id, binding.user_id)
            .await?;
        return Ok((StatusCode::OK, Json(session_response(session))).into_response());
    }

    // Browser path: mint a short-lived one-time handoff code and 302 into the
    // SPA with `?code=`; the SPA exchanges it (`POST /auth/exchange`) for the
    // real session bearer, which therefore never appears in a URL.
    let handoff = state
        .iam()
        .issue_login_token_with_ttl(
            binding.workspace_id,
            binding.user_id,
            catalerum_iam::HANDOFF_TOKEN_TTL,
        )
        .await?;
    let web = state.config().server.effective_web_url();
    let location = format!("{web}/?code={}", encode_query_component(&handoff.token));
    Ok(Redirect::to(&location).into_response())
}

/// Body for `POST /auth/exchange` — the one-time handoff code the SPA received
/// as `?code=` after a magic-link / SSO browser login.
#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    /// The one-time handoff code.
    pub code: String,
}

/// `POST /auth/exchange` — redeem a one-time handoff code for the real session
/// bearer, returned in the JSON body ([`SessionResponse`]). This is the second
/// half of the browser login handoff: the session token travels only in an
/// authenticated POST body, never in a URL.
async fn exchange_handoff(
    State(state): State<AppState>,
    Json(body): Json<ExchangeRequest>,
) -> ApiResult<Json<SessionResponse>> {
    if body.code.trim().is_empty() {
        return Err(ApiError::bad_request("empty code"));
    }
    let session = state.iam().redeem_login_token(body.code.trim()).await?;
    Ok(Json(session_response(session)))
}

/// `POST /auth/logout` — revoke the very session this request authenticated
/// with (SOUL §18). Server-side invalidation: the bearer stops verifying
/// immediately, not just when the client forgets it. Idempotent — revoking an
/// already-gone session still answers `204`.
async fn logout(State(state): State<AppState>, auth: Auth) -> ApiResult<StatusCode> {
    state.iam().revoke_session(auth.token()).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// One of the caller's workspace memberships, for the workspace switcher (§12).
#[derive(Debug, Serialize)]
pub struct WorkspaceMembership {
    pub id: WorkspaceId,
    /// The organisation this workspace belongs to (SOUL §18) — lets the switcher
    /// group workspaces under their organisation.
    pub organisation_id: catalerum_core::OrganisationId,
    pub name: String,
    pub slug: String,
    /// Lowercase role token in this workspace (`owner`/`admin`/`member`/`viewer`).
    pub role: String,
    /// Whether this is the workspace the current session is scoped to.
    pub active: bool,
}

/// `GET /workspaces` — the workspaces the authenticated user is a member of, each
/// with their role + whether it's the active (current-session) one. Authenticated
/// (any valid session); a user only ever sees **their own** memberships (SOUL §18).
async fn list_workspaces(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<WorkspaceMembership>>> {
    let p = auth.principal();
    let memberships = state.store().memberships().list_by_user(p.user_id).await?;
    // Resolve every membership's workspace in **one** query (not an N+1 `get` per
    // membership), then look each up by id — preserving membership order.
    let ids: Vec<_> = memberships.iter().map(|m| m.workspace_id).collect();
    let by_id: std::collections::HashMap<_, _> = state
        .store()
        .workspaces()
        .get_many(&ids)
        .await?
        .into_iter()
        .map(|ws| (ws.id, ws))
        .collect();
    let mut out = Vec::with_capacity(memberships.len());
    for m in memberships {
        // Best-effort name/slug; skip a membership whose workspace has vanished.
        if let Some(ws) = by_id.get(&m.workspace_id) {
            // Hide archived workspaces from the switcher (SOUL §18) — they can no
            // longer be switched into; an org admin restores them via the org
            // workspaces panel, which lists archived shells separately.
            if ws.archived_at.is_some() {
                continue;
            }
            out.push(WorkspaceMembership {
                id: ws.id,
                organisation_id: ws.organisation_id,
                name: ws.name.clone(),
                slug: ws.slug.clone(),
                role: catalerum_iam::role_str(m.role).to_string(),
                active: ws.id == p.workspace_id,
            });
        }
    }
    Ok(Json(out))
}

/// Body for `POST /auth/switch`.
#[derive(Debug, Deserialize)]
pub struct SwitchRequest {
    /// The workspace to switch the session to.
    pub workspace_id: WorkspaceId,
}

/// `POST /auth/switch` — mint a **new** session bound to `workspace_id` for the
/// authenticated user, returning its bearer ([`SessionResponse`]). The membership
/// check is enforced by `issue_session`: it fails `Unauthorized` if the caller is
/// not a member of the target workspace, so a user can only switch to a workspace
/// they belong to (SOUL §18/§19). The old session stays valid; the client simply
/// adopts the new bearer (the role is resolved from the target membership).
async fn switch_workspace(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<SwitchRequest>,
) -> ApiResult<Json<SessionResponse>> {
    let p = auth.principal();
    // Fail closed: never mint a session into an **archived** workspace (SOUL §18).
    // `get` returns archived rows (restore/admin need them), so we check the flag
    // explicitly before the membership-gated `issue_session`. A missing workspace
    // falls through to `issue_session`, which rejects a non-member.
    if let Ok(ws) = state.store().workspaces().get(body.workspace_id).await {
        if ws.archived_at.is_some() {
            return Err(ApiError::Forbidden(
                "this workspace is archived and cannot be switched into; an \
                 organisation admin must restore it first"
                    .into(),
            ));
        }
    }
    let session = state
        .iam()
        .issue_session(body.workspace_id, p.user_id)
        .await?;
    Ok(Json(SessionResponse {
        token: session.token,
        workspace_id: session.workspace_id,
        user_id: session.user_id,
        role: catalerum_iam::role_str(session.role).to_string(),
        expires_at: session.expires_at,
    }))
}

/// Percent-encode a value for safe inclusion in a query string. Encodes
/// everything outside the URL-unreserved set; ASCII-only, allocation-light.
fn encode_query_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_nibble(b >> 4));
                out.push(hex_nibble(b & 0x0f));
            }
        }
    }
    out
}

fn hex_nibble(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_query_component() {
        assert_eq!(encode_query_component("abc-123_~."), "abc-123_~.");
        assert_eq!(encode_query_component("a+b c/d"), "a%2Bb%20c%2Fd");
    }

    #[test]
    fn switch_request_decodes_workspace_id() {
        let r: SwitchRequest =
            serde_json::from_str(r#"{"workspace_id":"11111111-1111-1111-1111-111111111111"}"#)
                .unwrap();
        assert_eq!(
            r.workspace_id.to_string(),
            "11111111-1111-1111-1111-111111111111"
        );
        // A non-UUID is rejected.
        assert!(serde_json::from_str::<SwitchRequest>(r#"{"workspace_id":"nope"}"#).is_err());
    }

    #[test]
    fn workspace_membership_serializes() {
        let m = WorkspaceMembership {
            id: WorkspaceId::new(),
            organisation_id: catalerum_core::OrganisationId::new(),
            name: "Home".into(),
            slug: "home".into(),
            role: "owner".into(),
            active: true,
        };
        let j = serde_json::to_value(&m).unwrap();
        assert_eq!(j["name"], "Home");
        assert_eq!(j["role"], "owner");
        assert_eq!(j["active"], true);
    }
}
