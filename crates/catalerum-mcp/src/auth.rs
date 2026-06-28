//! Authentication for the HTTP MCP client (SOUL §26/§19).
//!
//! A remote MCP server reached over HTTP is usually behind a credential. An
//! [`AuthProvider`] produces the request headers each call needs, refreshing them
//! when they expire — so the transport stays oblivious to *how* a server is
//! authenticated. Four modes cover the field:
//!
//! - [`none`] — no credential (a trusted/internal server).
//! - [`bearer`] — a static `Authorization: Bearer <token>` (a PAT / API key /
//!   long-lived JWT).
//! - [`header`] — an arbitrary header (e.g. `X-Api-Key: …`).
//! - [`oauth2`] — OAuth 2.0 **machine-to-machine SSO**: `client_credentials`
//!   (a service account at an IdP / OIDC token endpoint) or `refresh_token` (an
//!   admin completed the interactive SSO once; we refresh access tokens
//!   headlessly). The access token is fetched on first use and cached until just
//!   before it expires, then transparently refreshed.
//!
//! Interactive authorization-code + PKCE with a browser redirect is out of scope
//! for a headless outbound client; the OAuth2 service-account / refresh-token
//! paths are the standard enterprise-SSO story for a server-to-server caller.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;

use catalerum_core::error::{Error, Result};

/// Refresh an OAuth2 token this many seconds before its stated expiry, so a token
/// never goes stale mid-flight.
const EXPIRY_SKEW_SECS: u64 = 30;
/// Fallback lifetime when a token response omits `expires_in`.
const DEFAULT_TOKEN_TTL_SECS: u64 = 3600;

/// Produces the HTTP headers an MCP request must carry to authenticate. `&self`
/// with interior mutability so one provider is shared across every call and can
/// cache/refresh a token.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    /// The headers to attach to each request (name, value). May perform a network
    /// round-trip (an OAuth token fetch/refresh) and fail.
    async fn headers(&self) -> Result<Vec<(String, String)>>;
}

/// No credential.
#[must_use]
pub fn none() -> Arc<dyn AuthProvider> {
    Arc::new(NoAuth)
}

/// A static `Authorization: Bearer <token>`.
#[must_use]
pub fn bearer(token: impl Into<String>) -> Arc<dyn AuthProvider> {
    Arc::new(BearerAuth(token.into()))
}

/// An arbitrary static header (e.g. `X-Api-Key`).
#[must_use]
pub fn header(name: impl Into<String>, value: impl Into<String>) -> Arc<dyn AuthProvider> {
    Arc::new(HeaderAuth {
        name: name.into(),
        value: value.into(),
    })
}

/// OAuth 2.0 machine-to-machine SSO. See [`OAuth2Params`].
#[must_use]
pub fn oauth2(params: OAuth2Params) -> Arc<dyn AuthProvider> {
    Arc::new(OAuth2Auth {
        params,
        http: reqwest::Client::new(),
        cached: Mutex::new(None),
    })
}

struct NoAuth;

#[async_trait]
impl AuthProvider for NoAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        Ok(Vec::new())
    }
}

struct BearerAuth(String);

#[async_trait]
impl AuthProvider for BearerAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![("Authorization".into(), format!("Bearer {}", self.0))])
    }
}

struct HeaderAuth {
    name: String,
    value: String,
}

#[async_trait]
impl AuthProvider for HeaderAuth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        Ok(vec![(self.name.clone(), self.value.clone())])
    }
}

/// Configuration for the OAuth2 ([`oauth2`]) provider. `grant_type` selects the
/// flow: `client_credentials` (a service account) or `refresh_token` (refresh a
/// token an admin obtained via interactive SSO).
#[derive(Clone, Debug)]
pub struct OAuth2Params {
    /// The IdP/OIDC token endpoint (`POST` target).
    pub token_url: String,
    /// `client_credentials` or `refresh_token`.
    pub grant_type: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret (empty for a public client).
    pub client_secret: String,
    /// The refresh token (only for the `refresh_token` grant).
    pub refresh_token: String,
    /// Requested scopes, space-separated (empty → omit).
    pub scope: String,
}

struct OAuth2Auth {
    params: OAuth2Params,
    http: reqwest::Client,
    cached: Mutex<Option<CachedToken>>,
}

/// A fetched bearer token and the instant it should be considered expired.
struct CachedToken {
    value: String,
    expires_at: Instant,
}

#[async_trait]
impl AuthProvider for OAuth2Auth {
    async fn headers(&self) -> Result<Vec<(String, String)>> {
        // Hold the lock across the fetch so concurrent callers don't stampede the
        // token endpoint — the first refreshes, the rest reuse the fresh token.
        let mut guard = self.cached.lock().await;
        if let Some(tok) = guard.as_ref() {
            if tok.expires_at > Instant::now() {
                return Ok(vec![(
                    "Authorization".into(),
                    format!("Bearer {}", tok.value),
                )]);
            }
        }
        let fresh = self.fetch_token().await?;
        let header = vec![("Authorization".into(), format!("Bearer {}", fresh.value))];
        *guard = Some(fresh);
        Ok(header)
    }
}

impl OAuth2Auth {
    /// POST the configured grant to the token endpoint and parse the access token.
    async fn fetch_token(&self) -> Result<CachedToken> {
        let p = &self.params;
        let mut form: Vec<(&str, &str)> = vec![("grant_type", &p.grant_type)];
        if !p.client_id.is_empty() {
            form.push(("client_id", &p.client_id));
        }
        if !p.client_secret.is_empty() {
            form.push(("client_secret", &p.client_secret));
        }
        if !p.scope.is_empty() {
            form.push(("scope", &p.scope));
        }
        if p.grant_type == "refresh_token" {
            if p.refresh_token.is_empty() {
                return Err(Error::invalid(
                    "mcp oauth2: grant_type=refresh_token requires a refresh_token",
                ));
            }
            form.push(("refresh_token", &p.refresh_token));
        }

        let body = serde_urlencoded::to_string(&form).map_err(|e| {
            Error::provider(format!("mcp oauth2: failed to encode token form: {e}"))
        })?;
        let resp = self
            .http
            .post(&p.token_url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("mcp oauth2: token request failed: {e}")))?;
        let status = resp.status();
        let body: Value = resp.json().await.map_err(|e| {
            Error::provider(format!("mcp oauth2: token response was not JSON: {e}"))
        })?;
        if !status.is_success() {
            let detail = body
                .get("error_description")
                .or_else(|| body.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("no detail");
            return Err(Error::provider(format!(
                "mcp oauth2: token endpoint returned {status}: {detail}"
            )));
        }
        let value = body
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::provider("mcp oauth2: token response had no access_token"))?
            .to_string();
        let ttl = body
            .get("expires_in")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        let life = ttl.saturating_sub(EXPIRY_SKEW_SECS).max(1);
        Ok(CachedToken {
            value,
            expires_at: Instant::now() + Duration::from_secs(life),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_providers_emit_expected_headers() {
        assert!(none().headers().await.unwrap().is_empty());
        assert_eq!(
            bearer("abc").headers().await.unwrap(),
            vec![("Authorization".to_string(), "Bearer abc".to_string())]
        );
        assert_eq!(
            header("X-Api-Key", "k").headers().await.unwrap(),
            vec![("X-Api-Key".to_string(), "k".to_string())]
        );
    }

    #[tokio::test]
    async fn oauth2_refresh_grant_requires_a_refresh_token() {
        let auth = oauth2(OAuth2Params {
            token_url: "http://127.0.0.1:1/token".into(),
            grant_type: "refresh_token".into(),
            client_id: "id".into(),
            client_secret: String::new(),
            refresh_token: String::new(),
            scope: String::new(),
        });
        let err = auth.headers().await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got {err:?}");
    }
}
