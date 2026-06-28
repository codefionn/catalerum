//! Microsoft OAuth routes — connect an Outlook / Microsoft 365 calendar
//! (SOUL §8/§13), the Google flow's twin.
//!
//! Two endpoints implement the Microsoft identity platform v2
//! **authorization-code** flow (offline access + `Calendars.ReadWrite`):
//!
//! - `GET /auth/microsoft/connect[?connection=…][&redirect=…]` — authenticated
//!   (`calendar:write`): 302 to the Entra consent screen after minting the
//!   per-connect CSRF `state` + the caller's workspace/connection into a
//!   short-lived `HttpOnly` `SameSite=Lax` state cookie (the shared
//!   [`GoogleStateSigner`](crate::google_oauth_state::GoogleStateSigner)
//!   machinery under an independent `[microsoft].state_secret` key).
//! - `GET /auth/microsoft/callback` — the browser redirect back from Microsoft
//!   (no session): verify + consume the state cookie, exchange the `code` for
//!   `{access_token, refresh_token, expiry}`, seal them **encrypted** (AES-GCM
//!   secret store, SOUL §13) on a `Calendar` connection with provider
//!   `outlook`, then 302 back into the SPA.
//!
//! When `[microsoft]` is unconfigured both routes `404`. Requires
//! `[secrets].master_key` (the callback 500s with a clear message otherwise —
//! there is no plaintext fallback). Unlike the Google flow there is no email
//! kind: calendar only.

use axum::extract::{Query, State};
use axum::http::header::{COOKIE, LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::id::ConnectionId;
use catalerum_core::model::ConnectionKind;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::google_oauth_state::{GoogleStateClaims, GOOGLE_STATE_TTL_SECS};
use crate::state::AppState;

/// The name of the Microsoft-OAuth state cookie.
const STATE_COOKIE: &str = "catalerum_microsoft_state";
/// Cookie path — set at `/auth/microsoft/connect`, read at the callback.
const COOKIE_PATH: &str = "/auth/microsoft";

/// Mount the Microsoft OAuth routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/microsoft/connect", get(connect))
        .route("/auth/microsoft/callback", get(callback))
}

/// Query for `GET /auth/microsoft/connect`.
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    /// An existing calendar connection id to **re-authorize** in place (rotate
    /// its sealed tokens); absent ⇒ a fresh connection is created on callback.
    #[serde(default)]
    pub connection: Option<String>,
    /// Optional same-origin SPA path to land on after connecting.
    #[serde(default)]
    pub redirect: Option<String>,
}

/// `GET /auth/microsoft/connect` — start the consent dance. Authenticated
/// (`calendar:write`); 302s to Microsoft with a freshly minted state cookie.
/// `404` when `[microsoft]` is not configured.
async fn connect(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ConnectQuery>,
) -> ApiResult<Response> {
    let microsoft = &state.config().microsoft;
    if !microsoft.is_enabled() {
        return Err(ApiError::NotFound);
    }
    auth.require(Action::Write, "calendar")?;
    let workspace_id = auth.principal().workspace_id;

    let connection = q
        .connection
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let csrf_state = uuid::Uuid::new_v4().simple().to_string();
    let redirect_after = sanitize_redirect(q.redirect.as_deref());
    let claims = GoogleStateClaims {
        state: csrf_state.clone(),
        workspace_id,
        kind: "calendar".to_string(),
        connection,
        redirect_after,
        exp: chrono::Utc::now().timestamp() + GOOGLE_STATE_TTL_SECS,
    };
    let cookie = set_cookie(
        &state.microsoft_state_signer().mint(&claims),
        cookie_secure(&state),
    );

    let api_base = state.config().server.effective_base_url();
    let redirect_uri = microsoft.effective_redirect_url(&api_base);
    let auth_url = authorization_url(
        &microsoft.client_id,
        &microsoft.tenant,
        &redirect_uri,
        &csrf_state,
    );

    redirect_with_cookie(&auth_url, &cookie)
}

/// Query for `GET /auth/microsoft/callback`.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// An error Microsoft reports instead of a `code` (`access_denied`, …).
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /auth/microsoft/callback` — finish the dance: verify the state cookie,
/// exchange the code, seal the tokens on an `outlook` calendar connection,
/// redirect to the SPA.
async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let microsoft = state.config().microsoft.clone();
    if !microsoft.is_enabled() {
        return Err(ApiError::NotFound);
    }

    if let Some(err) = q.error.as_deref().filter(|e| !e.trim().is_empty()) {
        return Err(ApiError::Unauthorized(format!(
            "Microsoft declined the authorization: {err}"
        )));
    }
    let code = q
        .code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing authorization code"))?;
    let returned_state = q
        .state
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("missing state parameter"))?;

    let cookie_token = read_cookie(&headers, STATE_COOKIE).ok_or_else(|| {
        ApiError::bad_request("missing Microsoft state cookie (connect not started here?)")
    })?;
    let now = chrono::Utc::now().timestamp();
    let claims = state
        .microsoft_state_signer()
        .verify(&cookie_token, now)
        .map_err(|_| ApiError::bad_request("invalid or expired Microsoft connect state"))?;
    if !constant_time_eq(claims.state.as_bytes(), returned_state.as_bytes()) {
        return Err(ApiError::bad_request("Microsoft state mismatch"));
    }

    // The secret store is required — tokens are only ever stored encrypted.
    let secrets = state.secret_store().cloned().ok_or_else(|| {
        ApiError::Internal(
            "cannot store Microsoft credentials: set [secrets].master_key to enable encryption"
                .into(),
        )
    })?;
    let workspace_id = claims.workspace_id;

    // Exchange the code for the token set (offline_access ⇒ a refresh token).
    let api_base = state.config().server.effective_base_url();
    let redirect_uri = microsoft.effective_redirect_url(&api_base);
    let tokens = catalerum_ingest::outlook_exchange_code(
        &microsoft.client_id,
        microsoft.client_secret.expose(),
        &microsoft.tenant,
        code,
        &redirect_uri,
    )
    .await?;
    let sealed = serde_json::to_vec(&tokens)
        .map_err(|e| ApiError::internal(format!("encode Microsoft credential: {e}")))?;

    // Re-authorize an existing connection in place when one was named; else
    // create a fresh `outlook` calendar connection (the Google callback's exact
    // three branches).
    let reuse = match &claims.connection {
        Some(id) => Some(resolve_reusable_connection(&state, workspace_id, id).await?),
        None => None,
    };
    match reuse {
        Some((_, Some(credential_ref))) => {
            secrets
                .replace(workspace_id, &credential_ref, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Microsoft credential: {e}")))?;
        }
        Some((connection_id, None)) => {
            let credential_ref = secrets
                .put(workspace_id, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Microsoft credential: {e}")))?;
            state
                .store()
                .connections()
                .set_credential_ref(workspace_id, connection_id, Some(&credential_ref))
                .await?;
        }
        None => {
            let credential_ref = secrets
                .put(workspace_id, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Microsoft credential: {e}")))?;
            state
                .store()
                .connections()
                .create(
                    workspace_id,
                    ConnectionKind::Calendar,
                    "Outlook Calendar",
                    Some(&credential_ref),
                    Some(serde_json::json!({ "provider": "outlook" })),
                )
                .await?;
        }
    }

    let clear = clear_cookie(cookie_secure(&state));
    let web = state.config().server.effective_web_url();
    let location = format!("{web}{}", claims.redirect_after);
    redirect_with_cookie(&location, &clear)
}

/// Resolve a named calendar connection for in-place re-authorization (the
/// Google route's helper, calendar-kind only).
async fn resolve_reusable_connection(
    state: &AppState,
    workspace_id: catalerum_core::WorkspaceId,
    id: &str,
) -> ApiResult<(ConnectionId, Option<String>)> {
    let uuid = uuid::Uuid::parse_str(id)
        .map_err(|_| ApiError::bad_request("connection is not a valid id"))?;
    let connection = state
        .store()
        .connections()
        .get(workspace_id, ConnectionId::from_uuid(uuid))
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != ConnectionKind::Calendar {
        return Err(ApiError::bad_request(
            "connection is not a Calendar connection".to_string(),
        ));
    }
    Ok((connection.id, connection.credential_ref))
}

/// Build the Entra consent-screen URL (authorization-code; the offline +
/// read/write calendar scopes; `prompt=consent` so the refresh token is
/// guaranteed on re-auth too).
fn authorization_url(
    client_id: &str,
    tenant: &str,
    redirect_uri: &str,
    csrf_state: &str,
) -> String {
    let params = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("response_mode", "query"),
        ("scope", catalerum_ingest::OUTLOOK_CALENDAR_SCOPES),
        ("prompt", "consent"),
        ("state", csrf_state),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_query_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", catalerum_ingest::outlook_auth_url(tenant))
}

// --- shared helpers (mirrors of `routes::google_oauth`'s private ones) -------

fn redirect_with_cookie(location: &str, cookie: &str) -> ApiResult<Response> {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(LOCATION, header_value(location)?);
    h.insert(SET_COOKIE, header_value(cookie)?);
    Ok(resp)
}

fn cookie_secure(state: &AppState) -> bool {
    state
        .config()
        .server
        .effective_base_url()
        .starts_with("https")
}

fn set_cookie(token: &str, secure: bool) -> String {
    let mut v = format!(
        "{STATE_COOKIE}={token}; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age={GOOGLE_STATE_TTL_SECS}"
    );
    if secure {
        v.push_str("; Secure");
    }
    v
}

fn clear_cookie(secure: bool) -> String {
    let mut v = format!("{STATE_COOKIE}=; Path={COOKIE_PATH}; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        v.push_str("; Secure");
    }
    v
}

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

/// Same-origin relative-path guard (open-redirect defence, SOUL §18).
fn sanitize_redirect(raw: Option<&str>) -> String {
    let Some(s) = raw.map(str::trim) else {
        return "/".to_string();
    };
    let path = s.split(['?', '#']).next().unwrap_or("");
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains(':')
        && !path.contains('\\')
        && !path.chars().any(|c| c.is_control() || c == ' ');
    if ok {
        path.to_string()
    } else {
        "/".to_string()
    }
}

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

fn header_value(s: &str) -> ApiResult<HeaderValue> {
    HeaderValue::from_str(s).map_err(|e| ApiError::internal(format!("bad header value: {e}")))
}

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
    fn authorization_url_carries_scopes_state_and_tenant() {
        let url = authorization_url("app-id", "contoso.com", "https://app/cb", "st8");
        assert!(
            url.starts_with("https://login.microsoftonline.com/contoso.com/oauth2/v2.0/authorize?")
        );
        assert!(url.contains("response_type=code"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("state=st8"));
        assert!(url.contains("client_id=app-id"));
        // offline_access + Calendars.ReadWrite, percent-encoded.
        assert!(url.contains(
            "scope=offline_access%20https%3A%2F%2Fgraph.microsoft.com%2FCalendars.ReadWrite"
        ));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));
        // A blank tenant falls back to the multi-tenant `common` endpoint.
        let url = authorization_url("app-id", "", "https://app/cb", "s");
        assert!(url.starts_with("https://login.microsoftonline.com/common/"));
    }

    #[test]
    fn set_and_clear_cookie_flags() {
        let set = set_cookie("tok", true);
        assert!(set.contains("catalerum_microsoft_state=tok"));
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
        assert!(set.contains("Secure") && set.contains("Path=/auth/microsoft"));
        assert!(!set_cookie("tok", false).contains("Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn sanitize_redirect_allows_only_same_origin_paths() {
        assert_eq!(sanitize_redirect(Some("/settings")), "/settings");
        assert_eq!(sanitize_redirect(Some("//evil.com")), "/");
        assert_eq!(sanitize_redirect(Some("https://evil.com")), "/");
        assert_eq!(sanitize_redirect(None), "/");
    }
}
