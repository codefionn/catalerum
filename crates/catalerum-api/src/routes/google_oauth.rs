//! Google OAuth routes — connect a Google Calendar **or** Gmail account
//! (SOUL §16 M7/§8/§13/§28).
//!
//! Two endpoints implement the Google **authorization-code** flow (offline access,
//! a scope chosen per `kind`):
//!
//! - `GET /auth/google/connect?kind=calendar|email[&connection=…][&redirect=…]` —
//!   authenticated (`<kind>:write`): 302 to Google's consent screen after minting
//!   the per-connect CSRF `state` + the caller's workspace/connection/kind into a
//!   short-lived `HttpOnly` `SameSite=Lax` [state cookie](crate::google_oauth_state).
//!   `kind=calendar` requests the Calendar **events read/write** scope (write-back,
//!   SOUL §8); `kind=email` requests the Gmail read-only scope.
//! - `GET /auth/google/callback` — the browser redirect back from Google (carries
//!   no session): verify + consume the state cookie, exchange the `code` for
//!   `{access_token, refresh_token, expiry}`, seal them **encrypted** (AES-GCM
//!   secret store, SOUL §13) on a connection of the matching kind (a `Calendar`
//!   connection with provider `google`, or an `Email` connection with provider
//!   `gmail`), then 302 back into the SPA.
//!
//! When `[google]` is unconfigured both routes `404`. The tokens live encrypted
//! behind the connection's `credential_ref` (never in the plaintext `config` blob
//! — this is exactly what closes the plaintext-Gmail-credentials finding); both the
//! calendar provider ([`GoogleTokenStore`](catalerum_calendar::GoogleTokenStore))
//! and the Gmail provider ([`GmailTokenStore`](catalerum_email::GmailTokenStore))
//! reach the **same** sealed blob through their token seam. Requires
//! `[secrets].master_key` to be set (else the callback 500s with a clear message —
//! there is no plaintext fallback).

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

/// The name of the Google-OAuth state cookie.
const STATE_COOKIE: &str = "catalerum_google_state";
/// Cookie path — set at `/auth/google/connect`, read at `/auth/google/callback`.
const COOKIE_PATH: &str = "/auth/google";

/// Mount the Google OAuth routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/google/connect", get(connect))
        .route("/auth/google/callback", get(callback))
}

/// Query for `GET /auth/google/connect`.
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    /// What to connect: `calendar` (default) or `email` (Gmail).
    #[serde(default)]
    pub kind: Option<String>,
    /// An existing connection id (of the matching kind) to **re-authorize** in
    /// place (rotate its sealed tokens); absent ⇒ a fresh connection is created on
    /// callback.
    #[serde(default)]
    pub connection: Option<String>,
    /// Optional same-origin SPA path to land on after connecting.
    #[serde(default)]
    pub redirect: Option<String>,
}

/// `GET /auth/google/connect` — start the Google consent dance. Authenticated
/// (`calendar:write`); 302s to Google with a freshly minted state cookie. `404`
/// when `[google]` is not configured.
async fn connect(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<ConnectQuery>,
) -> ApiResult<Response> {
    let google = &state.config().google;
    if !google.is_enabled() {
        return Err(ApiError::NotFound);
    }

    // Both calendar and Gmail are wired onto the same encrypted token store; the
    // scope is chosen per kind (see `scope_for_kind`).
    let kind = q
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("calendar");
    let scope = scope_for_kind(kind).ok_or_else(|| {
        ApiError::bad_request(format!(
            "unsupported google connect kind `{kind}` (supported: `calendar`, `email`)"
        ))
    })?;
    // Gate on write of the matching domain (`calendar:write` / `email:write`).
    auth.require(Action::Write, kind)?;
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
        kind: kind.to_string(),
        connection,
        redirect_after,
        exp: chrono::Utc::now().timestamp() + GOOGLE_STATE_TTL_SECS,
    };
    let cookie = set_cookie(
        &state.google_state_signer().mint(&claims),
        cookie_secure(&state),
    );

    let api_base = state.config().server.effective_base_url();
    let redirect_uri = google.effective_redirect_url(&api_base);
    let auth_url = authorization_url(&google.client_id, &redirect_uri, scope, &csrf_state);

    redirect_with_cookie(&auth_url, &cookie)
}

/// Query for `GET /auth/google/callback`.
#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    /// An error Google reports instead of a `code` (`access_denied`, …).
    #[serde(default)]
    pub error: Option<String>,
}

/// `GET /auth/google/callback` — finish the dance: verify the state cookie,
/// exchange the code, seal the tokens on a calendar connection, redirect to the SPA.
async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> ApiResult<Response> {
    let google = state.config().google.clone();
    if !google.is_enabled() {
        return Err(ApiError::NotFound);
    }

    if let Some(err) = q.error.as_deref().filter(|e| !e.trim().is_empty()) {
        return Err(ApiError::Unauthorized(format!(
            "Google declined the authorization: {err}"
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

    // Recover + re-verify the signed round-trip state from the cookie.
    let cookie_token = read_cookie(&headers, STATE_COOKIE).ok_or_else(|| {
        ApiError::bad_request("missing Google state cookie (connect not started here?)")
    })?;
    let now = chrono::Utc::now().timestamp();
    let claims = state
        .google_state_signer()
        .verify(&cookie_token, now)
        .map_err(|_| ApiError::bad_request("invalid or expired Google connect state"))?;
    // CSRF guard: Google's echoed `state` must equal the minted one.
    if !constant_time_eq(claims.state.as_bytes(), returned_state.as_bytes()) {
        return Err(ApiError::bad_request("Google state mismatch"));
    }

    // The secret store is required — tokens are only ever stored encrypted.
    let secrets = state.secret_store().cloned().ok_or_else(|| {
        ApiError::Internal(
            "cannot store Google credentials: set [secrets].master_key to enable encryption".into(),
        )
    })?;
    let workspace_id = claims.workspace_id;

    // Exchange the code for the token set (offline access ⇒ a refresh token).
    let api_base = state.config().server.effective_base_url();
    let redirect_uri = google.effective_redirect_url(&api_base);
    let tokens = catalerum_ingest::google_exchange_code(
        &google.client_id,
        google.client_secret.expose(),
        code,
        &redirect_uri,
    )
    .await?;
    let sealed = serde_json::to_vec(&tokens)
        .map_err(|e| ApiError::internal(format!("encode Google credential: {e}")))?;

    // The kind chosen at connect time decides the connection created/rotated here
    // (a `Calendar`+`google` or an `Email`+`gmail` connection). An unknown kind in a
    // (necessarily self-minted, signature-verified) claim is a 400 rather than a
    // silent default.
    let (conn_kind, conn_name, conn_config) =
        connection_spec_for_kind(&claims.kind).ok_or_else(|| {
            ApiError::bad_request(format!(
                "unsupported google connect kind `{}` in state",
                claims.kind
            ))
        })?;

    // Re-authorize an existing connection **in place** when one was named; otherwise
    // create a fresh connection of the kind. A named connection is resolved to its id
    // + optional credential slot:
    let reuse = match &claims.connection {
        Some(id) => Some(resolve_reusable_connection(&state, workspace_id, id, conn_kind).await?),
        None => None,
    };
    match reuse {
        // Named connection that already has a sealed credential → rotate it in place.
        Some((_, Some(credential_ref))) => {
            secrets
                .replace(workspace_id, &credential_ref, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Google credential: {e}")))?;
        }
        // Named connection lacking a credential slot → seal a fresh secret and point
        // the existing row at it (the earlier deferral: a `credential_ref`-less
        // connection can now be re-authed in place via the set-credential repo method,
        // instead of creating a duplicate — SOUL §8/§13/§28).
        Some((connection_id, None)) => {
            let credential_ref = secrets
                .put(workspace_id, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Google credential: {e}")))?;
            state
                .store()
                .connections()
                .set_credential_ref(workspace_id, connection_id, Some(&credential_ref))
                .await?;
        }
        // No connection named → create a fresh one of the kind.
        None => {
            let credential_ref = secrets
                .put(workspace_id, &sealed)
                .await
                .map_err(|e| ApiError::internal(format!("seal Google credential: {e}")))?;
            state
                .store()
                .connections()
                .create(
                    workspace_id,
                    conn_kind,
                    conn_name,
                    Some(&credential_ref),
                    Some(conn_config),
                )
                .await?;
        }
    }

    // Clear the (spent) state cookie and 302 back into the SPA.
    let clear = clear_cookie(cookie_secure(&state));
    let web = state.config().server.effective_web_url();
    let location = format!("{web}{}", claims.redirect_after);
    redirect_with_cookie(&location, &clear)
}

/// Resolve a named connection for **in-place re-authorization**: returns its
/// `(id, credential_ref)` when it exists and is of the `expected_kind`. A
/// `Some(ref)` means rotate the existing sealed blob; a `None` ref means the row has
/// no credential slot yet, so the caller seals a fresh secret and points the row at
/// it (`set_credential_ref`) rather than creating a duplicate. Errors on a
/// foreign/unknown id or a kind mismatch.
async fn resolve_reusable_connection(
    state: &AppState,
    workspace_id: catalerum_core::WorkspaceId,
    id: &str,
    expected_kind: ConnectionKind,
) -> ApiResult<(ConnectionId, Option<String>)> {
    let uuid = uuid::Uuid::parse_str(id)
        .map_err(|_| ApiError::bad_request("connection is not a valid id"))?;
    let connection = state
        .store()
        .connections()
        .get(workspace_id, ConnectionId::from_uuid(uuid))
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != expected_kind {
        return Err(ApiError::bad_request(format!(
            "connection is not a {expected_kind:?} connection"
        )));
    }
    Ok((connection.id, connection.credential_ref))
}

/// Map a connect `kind` to the OAuth scope it requests: calendar gets the
/// events read/**write** scope (write-back, SOUL §8 — a pre-write-back
/// connection still holding the read-only scope keeps syncing and re-connects
/// through this same route to upgrade); email stays Gmail read-only. `None`
/// for an unsupported kind (the connect route 400s).
fn scope_for_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "calendar" => Some(catalerum_ingest::GOOGLE_CALENDAR_EVENTS_SCOPE),
        "email" => Some(catalerum_ingest::GMAIL_READONLY_SCOPE),
        _ => None,
    }
}

/// The connection [`ConnectionKind`] + display name + stamped `config` a callback
/// creates for `kind` (`google`/`gmail` provider discriminators the ingest factory
/// reads). `None` for an unsupported kind. Pure — the callback's kind-branch target.
fn connection_spec_for_kind(
    kind: &str,
) -> Option<(ConnectionKind, &'static str, serde_json::Value)> {
    match kind {
        "calendar" => Some((
            ConnectionKind::Calendar,
            "Google Calendar",
            serde_json::json!({ "provider": "google", "calendar": "primary" }),
        )),
        "email" => Some((
            ConnectionKind::Email,
            "Gmail",
            serde_json::json!({ "provider": "gmail", "label": "INBOX" }),
        )),
        _ => None,
    }
}

/// Build Google's consent-screen URL (authorization-code, offline access, the given
/// read-only `scope`, forced consent so a refresh token is returned).
fn authorization_url(client_id: &str, redirect_uri: &str, scope: &str, csrf_state: &str) -> String {
    let params = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", scope),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("include_granted_scopes", "true"),
        ("state", csrf_state),
    ];
    let query = params
        .iter()
        .map(|(k, v)| format!("{k}={}", encode_query_component(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{query}", catalerum_ingest::GOOGLE_AUTH_URL)
}

// --- shared helpers (mirrors of the private ones in `routes::sso`) -----------

/// A 302 response to `location` that also sets `cookie`.
fn redirect_with_cookie(location: &str, cookie: &str) -> ApiResult<Response> {
    let mut resp = Response::new(axum::body::Body::empty());
    *resp.status_mut() = StatusCode::FOUND;
    let h = resp.headers_mut();
    h.insert(LOCATION, header_value(location)?);
    h.insert(SET_COOKIE, header_value(cookie)?);
    Ok(resp)
}

/// Whether the state cookie should carry `Secure` — true on an https deployment.
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

/// Same-origin relative-path guard (open-redirect defence, SOUL §18). Anything
/// that isn't a single-slash-rooted path with no scheme/authority/whitespace → `/`.
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

/// Percent-encode a value for a query string (URL-unreserved set only).
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
        assert_eq!(sanitize_redirect(Some("/x?y=1#z")), "/x");
        assert_eq!(sanitize_redirect(None), "/");
        assert_eq!(sanitize_redirect(Some("//evil.com")), "/");
        assert_eq!(sanitize_redirect(Some("https://evil.com")), "/");
        assert_eq!(sanitize_redirect(Some("javascript:alert(1)")), "/");
        assert_eq!(sanitize_redirect(Some("relative/path")), "/");
        assert_eq!(sanitize_redirect(Some("/with space")), "/");
    }

    #[test]
    fn read_cookie_extracts_named_value() {
        let mut h = HeaderMap::new();
        h.insert(
            COOKIE,
            HeaderValue::from_static("foo=1; catalerum_google_state=abc.def; bar=2"),
        );
        assert_eq!(read_cookie(&h, STATE_COOKIE).as_deref(), Some("abc.def"));
        assert_eq!(read_cookie(&h, "missing"), None);
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn set_and_clear_cookie_flags() {
        let set = set_cookie("tok", true);
        assert!(set.contains("catalerum_google_state=tok"));
        assert!(set.contains("HttpOnly") && set.contains("SameSite=Lax"));
        assert!(set.contains("Secure") && set.contains("Path=/auth/google"));
        assert!(!set_cookie("tok", false).contains("Secure"));
        assert!(clear_cookie(false).contains("Max-Age=0"));
    }

    #[test]
    fn authorization_url_carries_offline_events_scope_and_state() {
        let url = authorization_url(
            "cid.apps",
            "https://app/cb",
            catalerum_ingest::GOOGLE_CALENDAR_EVENTS_SCOPE,
            "st8",
        );
        assert!(url.starts_with(catalerum_ingest::GOOGLE_AUTH_URL));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("state=st8"));
        // The scope + redirect are percent-encoded.
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcalendar.events"));
        assert!(url.contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));
        assert!(url.contains("client_id=cid.apps"));
    }

    #[test]
    fn authorization_url_requests_gmail_scope_for_email_kind() {
        // The email kind threads the Gmail read-only scope into the same URL shape.
        let scope = scope_for_kind("email").unwrap();
        let url = authorization_url("cid.apps", "https://app/cb", scope, "st9");
        assert!(url.contains("scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fgmail.readonly"));
        assert!(url.contains("access_type=offline") && url.contains("prompt=consent"));
    }

    #[test]
    fn scope_for_kind_maps_calendar_and_email_only() {
        assert_eq!(
            scope_for_kind("calendar"),
            Some(catalerum_ingest::GOOGLE_CALENDAR_EVENTS_SCOPE),
            "calendar connects request the read/write events scope (write-back)"
        );
        assert_eq!(
            scope_for_kind("email"),
            Some(catalerum_ingest::GMAIL_READONLY_SCOPE)
        );
        assert_eq!(scope_for_kind("drive"), None);
        assert_eq!(scope_for_kind(""), None);
    }

    #[test]
    fn connection_spec_for_kind_creates_the_right_connection() {
        // Calendar → a `google` calendar connection.
        let (kind, name, config) = connection_spec_for_kind("calendar").unwrap();
        assert_eq!(kind, ConnectionKind::Calendar);
        assert_eq!(name, "Google Calendar");
        assert_eq!(config["provider"], "google");
        assert_eq!(config["calendar"], "primary");

        // Email → a `gmail` email connection (the sealed-credential Gmail path the
        // ingest factory routes to the encrypted token store).
        let (kind, name, config) = connection_spec_for_kind("email").unwrap();
        assert_eq!(kind, ConnectionKind::Email);
        assert_eq!(name, "Gmail");
        assert_eq!(config["provider"], "gmail");
        assert_eq!(config["label"], "INBOX");

        assert!(connection_spec_for_kind("drive").is_none());
    }
}
