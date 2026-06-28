//! Dev auth-token plumbing (SOUL §12, §18 dev magic-link login).
//!
//! Browser logins (dev magic-link, SSO) land here with a one-time `?code=`
//! handoff token, which the app exchanges for the real session bearer over
//! `POST /auth/exchange` and caches in `localStorage` — the session token
//! itself never appears in a URL. (A `?token=` bearer in the boot URL is still
//! adopted for the e2e harness, which obtains a session out-of-band.)
//!
//! The token is attached to API calls as a bearer credential. For the chat
//! WebSocket (browsers can't set request headers on the WS handshake) it is
//! passed as a `?token=` query parameter, which the API also accepts.

use wasm_bindgen::JsValue;

use crate::api::TOKEN_STORAGE_KEY;

/// Adopt an inbound bearer carried in the boot URL's `?token=` — the shape used
/// by **both** the dev magic-link and the SSO callback's SPA redirect — then
/// scrub it out of the address bar so the credential never lingers there.
///
/// This is the boot-time counterpart to [`resolve_token`]: it caches the token
/// exactly the same way (`localStorage` via [`store_token`], the one path a
/// workspace switch also uses), but instead of reloading it rewrites history in
/// place with [`History::replace_state_with_url`], preserving the path, any
/// other query params, and the hash. A no-op when the URL carries no token.
///
/// Runs once at [`crate::App`] mount, before any panel calls [`resolve_token`],
/// so subsequent reads resolve from storage against a clean URL.
pub fn adopt_url_token() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let Ok(search) = location.search() else {
        return;
    };
    let Some(tok) = parse_query_token(&search) else {
        return;
    };
    let tok = tok.trim().to_string();
    if tok.is_empty() {
        return;
    }
    store_token(&tok);
    // Rebuild the URL without the `token` param and swap it in without a reload.
    let cleaned = strip_token_param(&search);
    let pathname = location.pathname().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    let new_url = format!("{pathname}{cleaned}{hash}");
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
    }
}

/// Read and **scrub** a one-time login handoff code (`?code=…`) from the boot
/// URL — the shape both the dev magic-link and the SSO callback now redirect
/// with (SOUL §18). The code is short-lived and single-use; the caller exchanges
/// it for the real session bearer over `POST /auth/exchange`
/// ([`crate::rest::exchange_handoff_code`]), so the session token never appears
/// in the URL. The param is removed from the address bar immediately (same
/// `replace_state` scrub as [`adopt_url_token`]) so it can't linger in history
/// or be replayed by a reload. `None` when the URL carries no code.
pub fn take_handoff_code() -> Option<String> {
    let window = web_sys::window()?;
    let location = window.location();
    let search = location.search().ok()?;
    let code = parse_query_param(&search, "code")?;
    let code = code.trim().to_string();
    if code.is_empty() {
        return None;
    }
    // Scrub `?code=` from the URL without a reload (mirrors adopt_url_token).
    let cleaned = strip_query_param(&search, "code");
    let pathname = location.pathname().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    let new_url = format!("{pathname}{cleaned}{hash}");
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
    }
    Some(code)
}

/// The app-relative path (+ hash) the browser is currently on, for use as the
/// SSO `?redirect=` so the user lands back where they started after the IdP
/// round-trip. Empty when there is no `window`.
#[must_use]
pub fn current_relative_path() -> String {
    let Some(window) = web_sys::window() else {
        return String::new();
    };
    let location = window.location();
    let path = location.pathname().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    format!("{path}{hash}")
}

/// Resolve the dev session token, preferring a fresh `?token=` in the URL and
/// otherwise the value cached in `localStorage`.
///
/// When a token is found in the URL it is written back to `localStorage` so
/// subsequent reloads (which drop the query) remain authenticated. Returns
/// `None` if no token is available anywhere.
#[must_use]
pub fn resolve_token() -> Option<String> {
    if let Some(tok) = token_from_url() {
        let tok = tok.trim().to_string();
        if !tok.is_empty() {
            store_token(&tok);
            return Some(tok);
        }
    }
    token_from_storage().filter(|t| !t.trim().is_empty())
}

/// Read `?token=` from `window.location.search`, if present.
#[must_use]
pub fn token_from_url() -> Option<String> {
    let window = web_sys::window()?;
    let search = window.location().search().ok()?;
    parse_query_token(&search)
}

/// Read the cached token from `localStorage`.
#[must_use]
pub fn token_from_storage() -> Option<String> {
    let window = web_sys::window()?;
    let storage = window.local_storage().ok().flatten()?;
    storage.get_item(TOKEN_STORAGE_KEY).ok().flatten()
}

/// Persist a token to `localStorage` (best-effort; ignores failures, e.g.
/// storage disabled in private mode).
pub fn store_token(token: &str) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item(TOKEN_STORAGE_KEY, token);
        }
    }
}

/// Adopt a new session `token` (e.g. from a workspace switch) and reload the
/// page so every panel re-fetches under the new session: cache the token, drop
/// any stale `?token=` from the URL (so it doesn't override the new one on
/// reload, since [`resolve_token`] prefers the URL), then reload.
pub fn adopt_token_and_reload(token: &str) {
    store_token(token);
    if let Some(window) = web_sys::window() {
        let location = window.location();
        // Clearing the search both drops a stale `?token=` and triggers a reload;
        // `reload()` is a belt-and-braces fallback when the search was empty.
        let _ = location.set_search("");
        let _ = location.reload();
    }
}

/// Clear any cached token (dev "logout").
pub fn clear_token() {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.remove_item(TOKEN_STORAGE_KEY);
        }
    }
}

/// Whether a failed REST call means the session itself is dead: a `401
/// Unauthorized` on a request that actually **carried** a bearer. The server
/// answers 401 when the token no longer names a session ("unknown session
/// token", "session expired") — the cached credential is useless and every
/// panel would keep failing the same way. Token-less calls (the anonymous
/// login probe) can 401 without implying anything about a session, so they
/// never count.
///
/// Standalone (no web-sys) so it is unit-testable on any target.
#[must_use]
pub fn is_session_expired(status: u16, token: Option<&str>) -> bool {
    status == 401 && token.is_some_and(|t| !t.trim().is_empty())
}

/// Drop the dead session and land the user on the login surface: clear the
/// cached token, scrub any `?token=` from the address bar (a stale URL token
/// would be re-adopted at mount and loop the redirect), then reload.
/// [`crate::App`] resolves no token on the reload and mounts the
/// [`crate::components::LoginView`] on the **same path**, so the SSO
/// round-trip returns the user where they were.
pub fn redirect_to_login() {
    clear_token();
    let Some(window) = web_sys::window() else {
        return;
    };
    let location = window.location();
    let search = location.search().unwrap_or_default();
    let cleaned = strip_token_param(&search);
    let pathname = location.pathname().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    let new_url = format!("{pathname}{cleaned}{hash}");
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
    }
    let _ = location.reload();
}

/// Extract a named parameter's (percent-decoded) value from a raw `?a=b&key=…`
/// query string. `None` when the key is absent.
///
/// Standalone (no web-sys) so it is unit-testable on any target.
#[must_use]
pub fn parse_query_param(search: &str, key: &str) -> Option<String> {
    let q = search.strip_prefix('?').unwrap_or(search);
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if k == key {
            let raw = it.next().unwrap_or("");
            return Some(percent_decode(raw));
        }
    }
    None
}

/// Extract the `token` parameter from a raw `?a=b&token=…` query string. Thin
/// wrapper over [`parse_query_param`].
#[must_use]
pub fn parse_query_token(search: &str) -> Option<String> {
    parse_query_param(search, "token")
}

/// Drop `key` from a raw `?a=b&key=…&c=d` query string, returning a query string
/// ready to re-attach (a leading `?` when anything survives, else `""`). Order and
/// every other pair are preserved verbatim.
///
/// Standalone (no web-sys) so it is unit-testable on any target.
#[must_use]
pub fn strip_query_param(search: &str, key: &str) -> String {
    let q = search.strip_prefix('?').unwrap_or(search);
    if q.is_empty() {
        return String::new();
    }
    let kept: Vec<&str> = q
        .split('&')
        .filter(|pair| {
            let k = pair.split('=').next().unwrap_or("");
            k != key && !pair.is_empty()
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

/// Drop the `token` key from a raw query string. Thin wrapper over
/// [`strip_query_param`].
#[must_use]
pub fn strip_token_param(search: &str) -> String {
    strip_query_param(search, "token")
}

/// Map a coarse SSO callback error code (from `?sso_error=`, set by the API's
/// `GET /auth/sso/callback` on a failed browser login) to a friendly banner
/// message. Known codes get a specific explanation; the `failed` bucket and **any
/// unknown code** fold to a generic retry message — the raw param is never echoed,
/// so an attacker can't reflect arbitrary text into the login view. `None` for a
/// blank/absent code (nothing to show).
///
/// Standalone (no web-sys) so it is unit-testable on any target. Keep the codes in
/// sync with the API's `SsoErrorCode` wire tokens.
#[must_use]
pub fn sso_error_message(code: &str) -> Option<&'static str> {
    let code = code.trim();
    if code.is_empty() {
        return None;
    }
    Some(match code {
        "jit_disabled" => {
            "Single sign-in worked, but this instance doesn't create accounts \
             automatically. Ask an administrator to invite you first."
        }
        "email_linked" => {
            "An account with this email is already linked to a different sign-in \
             identity. Ask an administrator for help."
        }
        "no_email" => {
            "Your identity provider didn't share a verified email, so no account \
             could be created or matched."
        }
        "no_workspace" => {
            "You're signed in, but not a member of any workspace yet. Ask an \
             administrator to add you to one."
        }
        // "failed" and every unrecognised code: stay generic, never reflect input.
        _ => {
            "Single sign-in didn't complete. Please try again, or contact your \
             administrator if it keeps failing."
        }
    })
}

/// Consume a `?sso_error=<code>` the SSO callback bounced us back with: read the
/// (fixed-enum) code, scrub the param from the address bar exactly like
/// [`adopt_url_token`] scrubs `?token=`, and map it to a friendly banner message.
/// Returns `None` when absent/blank. Unknown codes fold to the generic message via
/// [`sso_error_message`] — the raw param is never surfaced.
///
/// Runs once at [`crate::components::LoginView`] mount.
#[must_use]
pub fn take_sso_error_message() -> Option<&'static str> {
    let window = web_sys::window()?;
    let location = window.location();
    let search = location.search().ok()?;
    let code = parse_query_param(&search, "sso_error")?;
    // Scrub `?sso_error=` from the URL without a reload (mirrors adopt_url_token).
    let cleaned = strip_query_param(&search, "sso_error");
    let pathname = location.pathname().unwrap_or_default();
    let hash = location.hash().unwrap_or_default();
    let new_url = format!("{pathname}{cleaned}{hash}");
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&new_url));
    }
    sso_error_message(&code)
}

/// Build the `GET /auth/sso/login` href **on the API origin** (the SPA and API
/// live on different origins — a page-relative href would only hit the SPA's
/// static server and never start the OIDC dance). `advertised` is the full
/// login URL the server pinned in config via `GET /status/login`
/// (`sso_login_url`, for APIs not reachable at the derived origin, e.g. behind
/// a Kubernetes ingress on another domain) and wins when non-blank; otherwise
/// the href is built on `api_base` (`api.<host>` in production,
/// `localhost:8787` in dev). Carries `redirect` as a (percent-encoded)
/// SPA-relative path when non-empty so the IdP round-trip lands the user back
/// where they started.
///
/// Standalone (no web-sys) so it is unit-testable on any target; the caller
/// passes [`crate::api::api_base`].
#[must_use]
pub fn sso_login_href(api_base: &str, advertised: Option<&str>, redirect: &str) -> String {
    let base = match advertised.map(str::trim).filter(|s| !s.is_empty()) {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => format!("{}/auth/sso/login", api_base.trim_end_matches('/')),
    };
    let r = redirect.trim();
    if r.is_empty() {
        base
    } else {
        // An advertised URL may already carry a query of its own.
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}redirect={}", encode_redirect(r))
    }
}

/// Percent-encode a relative path for use as a query-param value: unreserved
/// characters (RFC 3986) and `/` ride through, everything else becomes `%XX`.
/// Hand-rolled to avoid a urlencoding dependency in the wasm bundle.
fn encode_redirect(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Sanitize an SPA path for use as the SSO `?redirect=`: paths under `/auth/`
/// fold to `/`. Those are API routes, not SPA views — a browser only ends up
/// *parked* there when a broken login link got served the SPA shell instead
/// (nginx `try_files → index.html`, e.g. the old page-relative SSO href), and
/// carrying that path forward would bounce the user back onto it after login
/// (`?redirect=/auth/sso/login`) instead of into the app.
///
/// Standalone (no web-sys) so it is unit-testable on any target.
#[must_use]
pub fn sanitize_spa_redirect(path: &str) -> &str {
    let p = path.trim();
    if p == "/auth" || p.starts_with("/auth/") {
        "/"
    } else {
        path
    }
}

/// Whether the login surface should offer the "Sign in with SSO" button, given
/// what the anonymous `GET /status/login` probe learned about the `sso` flag
/// (`None` ⇒ the probe is still in flight). Shown only once SSO is *positively*
/// known to be on: rendering it while the answer is pending made the button
/// flash up and vanish on every dev login view (the probe answers `false` just
/// after first paint). A probe *failure* must still offer the button — hiding
/// it would strand the only real login path on SSO deployments — which the
/// caller handles by resolving the failed probe to `Some(true)`.
#[must_use]
pub fn show_sso_button(sso_known: Option<bool>) -> bool {
    sso_known == Some(true)
}

/// Minimal `application/x-www-form-urlencoded` decode: `+` → space and
/// `%XX` → byte. Sufficient for opaque dev tokens; avoids pulling a urlencoding
/// crate into the wasm bundle.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_token() {
        assert_eq!(
            parse_query_token("?token=abc123"),
            Some("abc123".to_string())
        );
        assert_eq!(
            parse_query_token("?foo=1&token=xyz&bar=2"),
            Some("xyz".to_string())
        );
    }

    #[test]
    fn no_token() {
        assert_eq!(parse_query_token("?foo=1"), None);
        assert_eq!(parse_query_token(""), None);
    }

    #[test]
    fn percent_and_plus() {
        assert_eq!(
            parse_query_token("?token=a%2Bb+c"),
            Some("a+b c".to_string())
        );
    }

    #[test]
    fn strip_token_keeps_the_rest() {
        // Sole param → empty (nothing to re-attach).
        assert_eq!(strip_token_param("?token=abc"), "");
        // Leading token drops, the rest survive in order.
        assert_eq!(strip_token_param("?token=abc&foo=1&bar=2"), "?foo=1&bar=2");
        // Trailing / middle token drops without disturbing neighbours.
        assert_eq!(strip_token_param("?foo=1&token=abc&bar=2"), "?foo=1&bar=2");
        assert_eq!(strip_token_param("?foo=1&token=abc"), "?foo=1");
        // No token / no query → empty, unchanged shape.
        assert_eq!(strip_token_param("?foo=1"), "?foo=1");
        assert_eq!(strip_token_param(""), "");
        // A non-token key that merely starts with "token" is preserved.
        assert_eq!(strip_token_param("?token_hint=x"), "?token_hint=x");
    }

    #[test]
    fn parse_query_param_generalises_beyond_token() {
        assert_eq!(
            parse_query_param("?sso_error=failed", "sso_error"),
            Some("failed".to_string())
        );
        assert_eq!(
            parse_query_param("?foo=1&sso_error=jit_disabled&bar=2", "sso_error"),
            Some("jit_disabled".to_string())
        );
        assert_eq!(parse_query_param("?foo=1", "sso_error"), None);
        // Percent/plus decoding rides through the generalised path.
        assert_eq!(
            parse_query_param("?sso_error=a%20b", "sso_error"),
            Some("a b".to_string())
        );
    }

    #[test]
    fn strip_query_param_drops_the_named_key() {
        // Sole param → empty.
        assert_eq!(strip_query_param("?sso_error=failed", "sso_error"), "");
        // Neighbours survive in order; a same-prefix key is untouched.
        assert_eq!(
            strip_query_param("?foo=1&sso_error=failed&bar=2", "sso_error"),
            "?foo=1&bar=2"
        );
        assert_eq!(
            strip_query_param("?sso_error_hint=x", "sso_error"),
            "?sso_error_hint=x"
        );
        // Absent key / empty query → unchanged shape.
        assert_eq!(strip_query_param("?foo=1", "sso_error"), "?foo=1");
        assert_eq!(strip_query_param("", "sso_error"), "");
    }

    #[test]
    fn sso_error_message_maps_known_codes_and_stays_generic_otherwise() {
        // Known codes get their own message…
        assert_ne!(
            sso_error_message("jit_disabled"),
            sso_error_message("failed")
        );
        assert_ne!(
            sso_error_message("email_linked"),
            sso_error_message("failed")
        );
        assert_ne!(sso_error_message("no_email"), sso_error_message("failed"));
        assert_ne!(
            sso_error_message("no_workspace"),
            sso_error_message("failed")
        );
        // Each known code returns *something*.
        assert!(sso_error_message("no_workspace").is_some());
        // The generic bucket names the retry / contact-admin path.
        let generic = sso_error_message("failed").unwrap();
        assert!(generic.contains("try again") && generic.contains("administrator"));
        // Unknown codes fold to the SAME generic message (never echoed).
        assert_eq!(sso_error_message("../../etc/passwd"), Some(generic));
        assert_eq!(sso_error_message("<script>"), Some(generic));
        assert_eq!(sso_error_message("totally_made_up"), Some(generic));
        // Blank / absent → nothing to show.
        assert_eq!(sso_error_message(""), None);
        assert_eq!(sso_error_message("   "), None);
    }

    #[test]
    fn sso_href_lands_on_the_api_origin_and_encodes_redirect() {
        const API: &str = "http://localhost:8787";
        assert_eq!(
            sso_login_href(API, None, ""),
            "http://localhost:8787/auth/sso/login"
        );
        assert_eq!(
            sso_login_href(API, None, "   "),
            "http://localhost:8787/auth/sso/login"
        );
        // A trailing slash on the base doesn't double up.
        assert_eq!(
            sso_login_href("https://api.example.com/", None, ""),
            "https://api.example.com/auth/sso/login"
        );
        // A plain app path rides through unencoded (slashes kept).
        assert_eq!(
            sso_login_href(API, None, "/app/calendar"),
            "http://localhost:8787/auth/sso/login?redirect=/app/calendar"
        );
        // Reserved chars are percent-encoded so the redirect survives the query.
        assert_eq!(
            sso_login_href(API, None, "/app/x?y=1&z=2"),
            "http://localhost:8787/auth/sso/login?redirect=/app/x%3Fy%3D1%26z%3D2"
        );
    }

    #[test]
    fn sso_href_prefers_the_server_advertised_login_url() {
        const API: &str = "http://localhost:8787";
        // A pinned URL (different domain, e.g. a k8s ingress) wins over api_base.
        assert_eq!(
            sso_login_href(API, Some("https://sso.example.com/auth/sso/login"), ""),
            "https://sso.example.com/auth/sso/login"
        );
        assert_eq!(
            sso_login_href(
                API,
                Some("https://sso.example.com/auth/sso/login/"),
                "/app/notes"
            ),
            "https://sso.example.com/auth/sso/login?redirect=/app/notes"
        );
        // An advertised URL already carrying a query gets `&`, not a second `?`.
        assert_eq!(
            sso_login_href(API, Some("https://sso.example.com/login?tenant=a"), "/x"),
            "https://sso.example.com/login?tenant=a&redirect=/x"
        );
        // Blank advertised values fall back to the api_base-derived href.
        assert_eq!(
            sso_login_href(API, Some("   "), ""),
            "http://localhost:8787/auth/sso/login"
        );
    }

    #[test]
    fn spa_redirect_folds_api_auth_paths_to_root() {
        // A browser parked on an API route (broken-link fallback) must not carry
        // it forward as the post-login landing path.
        assert_eq!(sanitize_spa_redirect("/auth/sso/login"), "/");
        assert_eq!(sanitize_spa_redirect("/auth/sso/login#x"), "/");
        assert_eq!(sanitize_spa_redirect("/auth"), "/");
        // Real SPA views ride through untouched.
        assert_eq!(sanitize_spa_redirect("/app/notes"), "/app/notes");
        assert_eq!(sanitize_spa_redirect("/"), "/");
        // A same-prefix SPA path is not an API route.
        assert_eq!(sanitize_spa_redirect("/authors"), "/authors");
    }

    #[test]
    fn session_expired_only_on_bearer_carrying_401() {
        // A 401 on a request that carried a bearer = dead session → bounce.
        assert!(is_session_expired(401, Some("tok")));
        // Token-less / blank-token calls (the anonymous login probe) never bounce.
        assert!(!is_session_expired(401, None));
        assert!(!is_session_expired(401, Some("")));
        assert!(!is_session_expired(401, Some("   ")));
        // Other statuses are not a session verdict (403 = permission, not auth).
        assert!(!is_session_expired(403, Some("tok")));
        assert!(!is_session_expired(500, Some("tok")));
        assert!(!is_session_expired(200, Some("tok")));
    }

    #[test]
    fn sso_button_only_shows_once_positively_known_on() {
        assert!(show_sso_button(Some(true)));
        // Probe still in flight → hidden (no flash-of-SSO-button on dev).
        assert!(!show_sso_button(None));
        assert!(!show_sso_button(Some(false)));
    }
}
