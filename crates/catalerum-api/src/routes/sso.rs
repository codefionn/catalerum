//! OIDC single sign-on routes — the **SSO first cut** (SOUL §18/§16 M7/§29).
//!
//! Two endpoints implement the OpenID Connect **Authorization Code flow with PKCE
//! (S256)**:
//!
//! - `GET /auth/sso/login` — 302 to the IdP's authorization endpoint, after
//!   minting the per-login `state`/`nonce`/PKCE-verifier into a short-lived,
//!   `HttpOnly` `SameSite=Lax` [state cookie](crate::sso_state).
//! - `GET /auth/sso/callback` — verify + consume the state cookie, exchange the
//!   `code` for an `id_token`, validate it (signature/iss/aud/exp/nonce/azp),
//!   resolve the identity to a local user (subject-bind → email-link → JIT), then
//!   **issue the session through the exact same `issue_session` path magic-link
//!   uses** — so the archived-workspace guard applies here too (SOUL §18).
//!
//! When `[sso]` is unconfigured both routes return `404` — dev magic-link login is
//! unaffected. The **web login button is out of scope** (a follow-up); the API is
//! usable directly and `GET /status` exposes `sso: bool` so the SPA can add it.

use axum::extract::{Query, State};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use catalerum_iam::{SsoDenyReason, SsoResolution};

use crate::error::{ApiError, ApiResult};
use crate::routes::auth::SessionResponse;
use crate::sso_state::{SsoStateClaims, SSO_STATE_TTL_SECS};
use crate::state::AppState;

/// The name of the SSO state cookie (see [`crate::sso_state`]).
const STATE_COOKIE: &str = "catalerum_sso_state";
/// The cookie path — set at `/auth/sso/login`, read at `/auth/sso/callback`.
const COOKIE_PATH: &str = "/auth/sso";

/// Mount the SSO routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/sso/login", get(login))
        .route("/auth/sso/callback", get(callback))
}

/// Query for `GET /auth/sso/login`.
#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    /// Optional same-origin SPA path to land on after login (allow-listed).
    #[serde(default)]
    pub redirect: Option<String>,
}

/// `GET /auth/sso/login` — start the OIDC dance. 302s to the IdP with a freshly
/// minted state cookie; `404` when SSO is not configured.
async fn login(State(state): State<AppState>, Query(q): Query<LoginQuery>) -> ApiResult<Response> {
    let provider = state.sso().ok_or(ApiError::NotFound)?;

    // Per-login randoms. UUIDs give ≥122 bits each; the PKCE verifier concatenates
    // two (64 hex chars, within the 43–128 unreserved range).
    let csrf_state = uuid::Uuid::new_v4().simple().to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let challenge = pkce_challenge(&verifier);
    let redirect_after = sanitize_redirect(q.redirect.as_deref());

    // Sign the round-trip state into the cookie *before* redirecting.
    let claims = SsoStateClaims {
        state: csrf_state.clone(),
        nonce: nonce.clone(),
        pkce_verifier: verifier,
        redirect_after,
        exp: chrono::Utc::now().timestamp() + SSO_STATE_TTL_SECS,
    };
    let cookie = set_cookie(
        &state.sso_state_signer().mint(&claims),
        state.sso_cookie_secure(),
    );

    let auth_url = provider
        .authorization_url(&csrf_state, &nonce, &challenge)
        .await?;

    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let headers = resp.headers_mut();
    headers.insert(LOCATION, header_value(&auth_url)?);
    headers.insert(SET_COOKIE, header_value(&cookie)?);
    Ok(resp)
}

/// Query for `GET /auth/sso/callback`.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// An IdP-reported error (`access_denied`, …) instead of a `code`.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
    /// `json` returns the session as [`SessionResponse`] (curl / e2e); default
    /// 302-redirects into the SPA with a one-time `?code=` handoff token like
    /// the magic-link flow (the SPA exchanges it via `POST /auth/exchange`).
    #[serde(default)]
    pub format: Option<String>,
}

/// A coarse, fixed callback-failure code surfaced to the SPA login view via
/// `?sso_error=<code>` (SOUL §18 SSO error-feedback). It is a **closed enum** so a
/// browser only ever learns a bucket name — never provider text, claims, or any
/// other attacker-influenceable content. Everything security-sensitive (state /
/// signature / code-exchange / id_token failures) folds into [`Self::Failed`], so
/// those cases stay indistinguishable to a caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SsoErrorCode {
    /// SSO verified but JIT provisioning is off — an admin must invite the user.
    JitDisabled,
    /// The email matches an account already bound to a different SSO identity.
    EmailLinked,
    /// The provider returned no verified email to link/create an account with.
    NoEmail,
    /// A resolved user who belongs to no (live) workspace yet.
    NoWorkspace,
    /// The generic bucket for every other (incl. security-sensitive) failure.
    Failed,
}

impl SsoErrorCode {
    /// The stable wire token embedded in `?sso_error=`. Matches the web login
    /// view's `sso_error_message` mapping.
    fn as_str(self) -> &'static str {
        match self {
            SsoErrorCode::JitDisabled => "jit_disabled",
            SsoErrorCode::EmailLinked => "email_linked",
            SsoErrorCode::NoEmail => "no_email",
            SsoErrorCode::NoWorkspace => "no_workspace",
            SsoErrorCode::Failed => "failed",
        }
    }
}

/// A callback failure paired with the coarse [`SsoErrorCode`] the SPA should show.
/// The `error` is the JSON body non-browser (`?format=json`) callers still receive
/// verbatim; the `code` is *all* a browser learns.
struct SsoFailure {
    code: SsoErrorCode,
    error: ApiError,
}

impl SsoFailure {
    fn new(code: SsoErrorCode, error: ApiError) -> Self {
        Self { code, error }
    }

    /// The generic bucket ([`SsoErrorCode::Failed`]): everything security-sensitive
    /// (missing/invalid state, signature/exchange/id_token faults, header build
    /// errors) collapses here so it is indistinguishable from a plain failure.
    fn failed(error: ApiError) -> Self {
        Self::new(SsoErrorCode::Failed, error)
    }
}

/// `GET /auth/sso/callback` — finish the OIDC dance and establish a session.
///
/// Non-browser callers (`?format=json`, used by curl / e2e) keep the JSON
/// [`ApiError`] on failure. Browsers instead 302 back into the SPA login view with
/// a coarse `?sso_error=<code>` (SOUL §18) so it can explain what went wrong,
/// without ever reflecting provider text or claims.
async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    // SSO unconfigured → the route effectively does not exist (as at /login). This
    // is not a user-facing sign-in failure, so it stays a plain 404 either way.
    let provider = state.sso().ok_or(ApiError::NotFound)?;
    let want_json = q.format.as_deref() == Some("json");

    match run_callback(&state, provider, &headers, &q).await {
        Ok(resp) => Ok(resp),
        Err(SsoFailure { code, error }) => {
            // Ops visibility: a browser only ever learns the coarse code, so this
            // is the one place the underlying failure surfaces (server-side only).
            tracing::warn!(code = code.as_str(), error = %error, "SSO callback failed");
            if want_json {
                Err(error)
            } else {
                sso_error_redirect(&state, code)
            }
        }
    }
}

/// The callback body, factored out so [`callback`] can turn any failure into either
/// a JSON error or an `?sso_error=` redirect. Every fallible step attaches the code
/// the SPA should surface; security-sensitive steps use the generic bucket.
async fn run_callback(
    state: &AppState,
    provider: &catalerum_iam::OidcProvider,
    headers: &HeaderMap,
    q: &CallbackQuery,
) -> Result<Response, SsoFailure> {
    // The IdP declined (user cancelled, consent denied, …). The reported `error` is
    // attacker-influenceable, so a browser only ever learns the generic bucket.
    if let Some(err) = q.error.as_deref().filter(|e| !e.trim().is_empty()) {
        let detail = q.error_description.as_deref().unwrap_or(err);
        return Err(SsoFailure::failed(ApiError::Unauthorized(format!(
            "SSO provider returned an error: {detail}"
        ))));
    }

    let code = q
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SsoFailure::failed(ApiError::bad_request("missing authorization code")))?;
    let returned_state = q
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SsoFailure::failed(ApiError::bad_request("missing state parameter")))?;

    // Recover the signed round-trip state from the cookie and re-verify it.
    let cookie_token = read_cookie(headers, STATE_COOKIE).ok_or_else(|| {
        SsoFailure::failed(ApiError::bad_request(
            "missing SSO state cookie (login not started here?)",
        ))
    })?;
    let now = chrono::Utc::now().timestamp();
    let claims = state
        .sso_state_signer()
        .verify(&cookie_token, now)
        .map_err(|_| {
            SsoFailure::failed(ApiError::bad_request("invalid or expired SSO login state"))
        })?;

    // CSRF guard: the IdP's `state` must equal the one we minted into the cookie.
    if !constant_time_eq(claims.state.as_bytes(), returned_state.as_bytes()) {
        return Err(SsoFailure::failed(ApiError::bad_request(
            "SSO state mismatch",
        )));
    }

    // Exchange the code (PKCE verifier from the cookie) and validate the id_token
    // (signature + iss/aud/exp/iat/nonce/azp) — fail closed on any problem.
    let identity = provider
        .authenticate(code, &claims.pkce_verifier, &claims.nonce)
        .await
        .map_err(|e| SsoFailure::failed(ApiError::from(e)))?;

    let cfg = state.config().sso.clone();
    let resolution = state
        .iam()
        .resolve_sso_identity(&identity, cfg.jit_enabled(), cfg.trust_email)
        .await
        .map_err(|e| SsoFailure::failed(ApiError::from(e)))?;

    let user = match resolution {
        SsoResolution::Existing(user) => user,
        SsoResolution::Provisioned(user) => {
            // JIT users get **organisation** membership per config — and, unless
            // `jit_workspace` opts in below, no workspace (fail-closed): an admin
            // invites them into a workspace afterward.
            provision_org_membership(state, &cfg, user.id)
                .await
                .map_err(SsoFailure::failed)?;
            user
        }
        SsoResolution::Denied(reason) => return Err(deny_failure(reason)),
    };

    // Land in the user's first non-archived workspace, issuing the session through
    // the shared `issue_session` chokepoint (so the archived guard applies, §18).
    // A user with no live workspace is auto-joined to the configured JIT landing
    // workspace first, when that knob is on.
    let mut landing = first_landing_workspace(state, user.id)
        .await
        .map_err(SsoFailure::failed)?;
    if landing.is_none() {
        landing = provision_workspace_membership(state, &cfg, user.id)
            .await
            .map_err(SsoFailure::failed)?;
    }
    let Some(workspace_id) = landing else {
        // Resolved a user but they belong to no (live) workspace. Common for a
        // freshly JIT-provisioned user: they're in the org but not yet a workspace.
        return Err(SsoFailure::new(
            SsoErrorCode::NoWorkspace,
            ApiError::Forbidden(
                "your account is not a member of any workspace yet; ask an organisation \
                 admin to add you to one"
                    .into(),
            ),
        ));
    };
    // Deliver the session. Clear the (now-spent) state cookie on the way out.
    let clear = clear_cookie(state.sso_cookie_secure());
    if q.format.as_deref() == Some("json") {
        // Programmatic client: issue + return the session as JSON — no redirect,
        // no URL involved.
        let session = state
            .iam()
            .issue_session(workspace_id, user.id)
            .await
            .map_err(|e| SsoFailure::failed(ApiError::from(e)))?;
        let mut resp = Json(SessionResponse {
            token: session.token,
            workspace_id: session.workspace_id,
            user_id: session.user_id,
            role: catalerum_iam::role_str(session.role).to_string(),
            expires_at: session.expires_at,
        })
        .into_response();
        resp.headers_mut().insert(
            SET_COOKIE,
            header_value(&clear).map_err(SsoFailure::failed)?,
        );
        return Ok(resp);
    }

    // Browser path: mint a short-lived one-time handoff code and 302 into the
    // SPA (its validated same-origin landing path) with `?code=`, exactly like
    // the magic-link redeem. The SPA exchanges the code (`POST /auth/exchange`)
    // for the real session bearer — the session token never appears in a URL
    // (browser history, referers, access logs).
    let handoff = state
        .iam()
        .issue_login_token_with_ttl(workspace_id, user.id, catalerum_iam::HANDOFF_TOKEN_TTL)
        .await
        .map_err(|e| SsoFailure::failed(ApiError::from(e)))?;
    let web = state.config().server.effective_web_url();
    let location = format!(
        "{web}{}?code={}",
        claims.redirect_after,
        encode_query_component(&handoff.token)
    );
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(
        LOCATION,
        header_value(&location).map_err(SsoFailure::failed)?,
    );
    h.insert(
        SET_COOKIE,
        header_value(&clear).map_err(SsoFailure::failed)?,
    );
    Ok(resp)
}

/// Bounce a failed **browser** callback back into the SPA login view at the web
/// root with a coarse, fixed `?sso_error=<code>` (never provider text / claims),
/// clearing the (spent/aborted) state cookie on the way. The login view reads and
/// scrubs the param on mount.
fn sso_error_redirect(state: &AppState, code: SsoErrorCode) -> ApiResult<Response> {
    let web = state.config().server.effective_web_url();
    // `web` carries no trailing slash (see `effective_web_url`); land on the root.
    let location = format!("{web}/?sso_error={}", code.as_str());
    let clear = clear_cookie(state.sso_cookie_secure());
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(LOCATION, header_value(&location)?);
    h.insert(SET_COOKIE, header_value(&clear)?);
    Ok(resp)
}

/// Give a JIT-provisioned user their configured organisation membership (SOUL §18).
/// Resolves the target org by slug, falling back to the well-known default org, and
/// upserts the configured org role (defaulting to `member` on an unknown token).
async fn provision_org_membership(
    state: &AppState,
    cfg: &crate::config::SsoConfig,
    user_id: catalerum_core::UserId,
) -> ApiResult<()> {
    let org = match state
        .store()
        .organisations()
        .get_by_slug(cfg.jit_org_slug())
        .await
    {
        Ok(org) => org.id,
        // The configured org doesn't exist → land JIT users in the default org
        // rather than failing the login outright.
        Err(_) => catalerum_iam::DEFAULT_ORGANISATION_ID,
    };
    let role = catalerum_iam::org_role_from_str(cfg.jit_org_role_token())
        .unwrap_or(catalerum_core::model::OrgRole::Member);
    state
        .store()
        .org_memberships()
        .upsert(org, user_id, role)
        .await?;
    Ok(())
}

/// Auto-join an SSO user who belongs to no live workspace into the configured JIT
/// landing workspace (SOUL §18) — the "SSO logins just work" opt-in. Requires JIT
/// provisioning enabled AND a `jit_workspace` slug; the slug is resolved with a
/// fallback to the well-known default workspace (mirroring the org fallback
/// above), and when **neither exists** the configured workspace is **created** —
/// on a fresh `multi_user` deployment (`dev_login` off) no workspace exists and
/// no admin can ever log in to make one, so a join-only knob would deadlock the
/// instance. The operator opted in by naming the slug; creation lands in the
/// default organisation like every seed workspace. An **archived** target keeps
/// the fail-closed no-workspace denial rather than landing the user somewhere
/// surprising. Note this also re-admits a user an admin removed from every
/// workspace — turn the knob off (or deactivate the account at the IdP) to lock
/// someone out.
async fn provision_workspace_membership(
    state: &AppState,
    cfg: &crate::config::SsoConfig,
    user_id: catalerum_core::UserId,
) -> ApiResult<Option<catalerum_core::WorkspaceId>> {
    if !cfg.jit_enabled() {
        return Ok(None);
    }
    let Some(slug) = cfg.jit_workspace_slug() else {
        return Ok(None);
    };
    let role = catalerum_iam::role_from_str(cfg.jit_workspace_role_token())
        .unwrap_or(catalerum_core::model::Role::Member);
    join_jit_workspace(state.store(), slug, user_id, role).await
}

/// Store-only core of [`provision_workspace_membership`] (factored so the
/// DB-gated test can drive it without an `AppState`): resolve the landing
/// workspace — configured slug, else the well-known default, else **create** the
/// configured one (see the caller's doc for why creation is safe here) — and
/// upsert the membership. `None` for an archived target (fail-closed).
async fn join_jit_workspace(
    store: &catalerum_store::Store,
    slug: &str,
    user_id: catalerum_core::UserId,
    role: catalerum_core::model::Role,
) -> ApiResult<Option<catalerum_core::WorkspaceId>> {
    let workspaces = store.workspaces();
    let ws = match workspaces.get_by_slug(slug).await {
        Ok(ws) => ws,
        Err(_) => match workspaces
            .get_by_slug(catalerum_iam::DEFAULT_WORKSPACE_SLUG)
            .await
        {
            Ok(ws) => ws,
            Err(_) => {
                // Bootstrap: neither the configured nor the default workspace
                // exists. Create the configured one; on a concurrent-login race
                // the second create hits the slug conflict, so re-resolve.
                match workspaces.create(jit_workspace_name(slug), slug).await {
                    Ok(ws) => {
                        tracing::info!(workspace_id = %ws.id, slug,
                            "SSO JIT bootstrap: created the configured landing workspace");
                        ws
                    }
                    Err(_) => workspaces.get_by_slug(slug).await?,
                }
            }
        },
    };
    if ws.archived_at.is_some() {
        return Ok(None);
    }
    store.memberships().upsert(ws.id, user_id, role).await?;
    Ok(Some(ws.id))
}

/// Display name for a JIT-created landing workspace: the canonical default name
/// for the well-known `default` slug, else the slug itself (the operator can
/// rename it in the UI afterwards).
fn jit_workspace_name(slug: &str) -> &str {
    if slug == catalerum_iam::DEFAULT_WORKSPACE_SLUG {
        catalerum_iam::DEFAULT_WORKSPACE_NAME
    } else {
        slug
    }
}

/// The first **non-archived** workspace `user_id` is a member of (membership order),
/// or `None`. Mirrors the switcher's archived-hiding (SOUL §18); `issue_session`
/// still re-checks the archive flag as the real chokepoint.
async fn first_landing_workspace(
    state: &AppState,
    user_id: catalerum_core::UserId,
) -> ApiResult<Option<catalerum_core::WorkspaceId>> {
    let memberships = state.store().memberships().list_by_user(user_id).await?;
    if memberships.is_empty() {
        return Ok(None);
    }
    let ids: Vec<_> = memberships.iter().map(|m| m.workspace_id).collect();
    let by_id: std::collections::HashMap<_, _> = state
        .store()
        .workspaces()
        .get_many(&ids)
        .await?
        .into_iter()
        .map(|ws| (ws.id, ws))
        .collect();
    for m in &memberships {
        if let Some(ws) = by_id.get(&m.workspace_id) {
            if ws.archived_at.is_none() {
                return Ok(Some(ws.id));
            }
        }
    }
    Ok(None)
}

/// Map a resolution denial onto its coarse SPA [`SsoErrorCode`] plus the friendly,
/// non-leaking JSON [`ApiError`] the `?format=json` path keeps (SOUL §18).
fn deny_failure(reason: SsoDenyReason) -> SsoFailure {
    match reason {
        SsoDenyReason::ProvisioningDisabled => SsoFailure::new(
            SsoErrorCode::JitDisabled,
            ApiError::Forbidden(
                "single sign-on succeeded but automatic account provisioning is disabled; \
                 ask an administrator to invite you first"
                    .into(),
            ),
        ),
        SsoDenyReason::NoVerifiedEmail => SsoFailure::new(
            SsoErrorCode::NoEmail,
            ApiError::BadRequest(
                "single sign-on succeeded but the provider returned no verified email, so no \
                 account can be created or matched"
                    .into(),
            ),
        ),
        SsoDenyReason::EmailAlreadyLinked => SsoFailure::new(
            SsoErrorCode::EmailLinked,
            ApiError::Forbidden(
                "an account with this email is already linked to a different single sign-on \
                 identity"
                    .into(),
            ),
        ),
    }
}

/// PKCE S256 challenge: `base64url(sha256(verifier))`, no padding.
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Validate an optional `?redirect=` into a **same-origin relative path** (open-
/// redirect guard, SOUL §18): must be a single-slash-rooted path with no scheme,
/// authority, control chars, whitespace, or query/fragment. Anything else → `/`.
fn sanitize_redirect(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim) else {
        return "/".to_string();
    };
    // Keep only the path segment (drop any query/fragment the caller tacked on; the
    // callback appends its own `?code=`).
    let path = s.split(['?', '#']).next().unwrap_or("");
    let ok = path.starts_with('/')            // rooted…
        && !path.starts_with("//")            // …but not protocol-relative (`//host`)
        && !path.contains(':')                // no `scheme:` / `\` tricks
        && !path.contains('\\')
        && !path.chars().any(|c| c.is_control() || c == ' ');
    if ok {
        path.to_string()
    } else {
        "/".to_string()
    }
}

/// Build the `Set-Cookie` value for the state cookie: `HttpOnly`, `SameSite=Lax`
/// (so it survives the top-level GET navigation back from the IdP), path-scoped to
/// the SSO routes, short `Max-Age`, and `Secure` on an https deployment.
fn set_cookie(token: &str, secure: bool) -> String {
    let mut v = format!(
        "{STATE_COOKIE}={token}; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age={SSO_STATE_TTL_SECS}"
    );
    if secure {
        v.push_str("; Secure");
    }
    v
}

/// The `Set-Cookie` value that immediately clears the state cookie (spent/aborted).
fn clear_cookie(secure: bool) -> String {
    let mut v = format!("{STATE_COOKIE}=; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        v.push_str("; Secure");
    }
    v
}

/// Read a single cookie value out of the request `Cookie` header.
fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some((k, val)) = pair.split_once('=') {
            if k.trim() == name {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

/// Constant-time byte-slice equality — used to compare the CSRF `state` without a
/// length/early-exit timing signal.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Build a `HeaderValue` from a string we constructed (URL / cookie), mapping the
/// (practically impossible) invalid-header case to a 500 rather than panicking.
fn header_value(s: &str) -> ApiResult<HeaderValue> {
    HeaderValue::from_str(s).map_err(|e| ApiError::internal(format!("bad header value: {e}")))
}

/// Percent-encode a value for a query string (URL-unreserved set only). Mirrors the
/// helper in [`crate::routes::auth`] so the session token lands intact.
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
    fn sanitize_redirect_allows_only_same_origin_paths() {
        assert_eq!(sanitize_redirect(Some("/settings")), "/settings");
        assert_eq!(sanitize_redirect(Some("/a/b/c")), "/a/b/c");
        // Query/fragment are stripped (the callback appends its own ?code=).
        assert_eq!(sanitize_redirect(Some("/x?y=1#z")), "/x");
        assert_eq!(sanitize_redirect(None), "/");
        // Open-redirect vectors all collapse to "/".
        assert_eq!(sanitize_redirect(Some("//evil.com")), "/");
        assert_eq!(sanitize_redirect(Some("https://evil.com")), "/");
        assert_eq!(sanitize_redirect(Some("http://evil.com/x")), "/");
        assert_eq!(sanitize_redirect(Some("javascript:alert(1)")), "/");
        assert_eq!(sanitize_redirect(Some("relative/path")), "/");
        assert_eq!(sanitize_redirect(Some("/back\\slash")), "/");
        assert_eq!(sanitize_redirect(Some("/with space")), "/");
        assert_eq!(sanitize_redirect(Some("")), "/");
    }

    #[test]
    fn pkce_challenge_is_stable_base64url_sha256() {
        // Known vector from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn read_cookie_extracts_named_value() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("foo=1; catalerum_sso_state=abc.def; bar=2"),
        );
        assert_eq!(read_cookie(&h, STATE_COOKIE).as_deref(), Some("abc.def"));
        assert_eq!(read_cookie(&h, "missing"), None);
        assert_eq!(read_cookie(&HeaderMap::new(), STATE_COOKIE), None);
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// SOUL §18 JIT landing-workspace bootstrap: on an instance with **no**
    /// workspaces (fresh `multi_user` deploy, `dev_login` off) the configured
    /// workspace is created and joined — the deadlock the k3s deployment hit:
    /// join-only provisioning can never seed the first workspace. Later logins
    /// join the same workspace; an archived target stays fail-closed.
    #[tokio::test]
    async fn jit_workspace_bootstrap_creates_then_joins_then_respects_archive() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping jit-workspace bootstrap test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        use catalerum_core::model::Role;
        let store = crate::test_db::isolated_store(&url).await;
        let first = store
            .users()
            .create("first@sso.test", "First", None)
            .await
            .expect("user");

        // Zero workspaces → the configured slug is created and joined as admin.
        let ws_id = join_jit_workspace(&store, "default", first.id, Role::Admin)
            .await
            .expect("bootstrap")
            .expect("created + joined");
        let ws = store.workspaces().get(ws_id).await.expect("ws");
        assert_eq!(ws.slug, catalerum_iam::DEFAULT_WORKSPACE_SLUG);
        assert_eq!(ws.name, catalerum_iam::DEFAULT_WORKSPACE_NAME);
        let m = store
            .memberships()
            .get(ws_id, first.id)
            .await
            .expect("membership");
        assert_eq!(m.role, Role::Admin);

        // A second login joins the SAME workspace (no duplicate creation).
        let second = store
            .users()
            .create("second@sso.test", "Second", None)
            .await
            .expect("user");
        let again = join_jit_workspace(&store, "default", second.id, Role::Member)
            .await
            .expect("join")
            .expect("joined existing");
        assert_eq!(again, ws_id);

        // Archived target → fail-closed None, and no membership is written.
        store.workspaces().archive(ws_id).await.expect("archive");
        let third = store
            .users()
            .create("third@sso.test", "Third", None)
            .await
            .expect("user");
        assert!(
            join_jit_workspace(&store, "default", third.id, Role::Member)
                .await
                .expect("no-op")
                .is_none()
        );
        assert!(store.memberships().get(ws_id, third.id).await.is_err());
    }

    #[test]
    fn sso_error_code_wire_tokens_are_the_fixed_enum() {
        // The web login view keys its `sso_error_message` mapping off these exact
        // tokens; they must stay a small closed set (no attacker-controllable text).
        assert_eq!(SsoErrorCode::JitDisabled.as_str(), "jit_disabled");
        assert_eq!(SsoErrorCode::EmailLinked.as_str(), "email_linked");
        assert_eq!(SsoErrorCode::NoEmail.as_str(), "no_email");
        assert_eq!(SsoErrorCode::NoWorkspace.as_str(), "no_workspace");
        assert_eq!(SsoErrorCode::Failed.as_str(), "failed");
    }

    #[test]
    fn deny_failure_maps_each_reason_to_its_code() {
        // Each user-facing denial gets a specific, friendly code + status; nothing
        // here is the generic bucket.
        let jit = deny_failure(SsoDenyReason::ProvisioningDisabled);
        assert_eq!(jit.code, SsoErrorCode::JitDisabled);
        assert_eq!(jit.error.status(), StatusCode::FORBIDDEN);

        let no_email = deny_failure(SsoDenyReason::NoVerifiedEmail);
        assert_eq!(no_email.code, SsoErrorCode::NoEmail);
        assert_eq!(no_email.error.status(), StatusCode::BAD_REQUEST);

        let linked = deny_failure(SsoDenyReason::EmailAlreadyLinked);
        assert_eq!(linked.code, SsoErrorCode::EmailLinked);
        assert_eq!(linked.error.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn failed_bucket_never_carries_a_specific_code() {
        // The generic constructor is the only path for security-sensitive faults
        // (state/signature/exchange), so they can't be told apart.
        let f = SsoFailure::failed(ApiError::bad_request("SSO state mismatch"));
        assert_eq!(f.code, SsoErrorCode::Failed);
        assert_eq!(f.code.as_str(), "failed");
    }

    #[test]
    fn set_and_clear_cookie_carry_the_right_flags() {
        let set = set_cookie("tok", true);
        assert!(set.contains("catalerum_sso_state=tok"));
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
        assert!(set.contains("Secure") && set.contains("Path=/auth/sso"));
        let insecure = set_cookie("tok", false);
        assert!(!insecure.contains("Secure"));
        let clear = clear_cookie(false);
        assert!(clear.contains("Max-Age=0"));
    }
}
