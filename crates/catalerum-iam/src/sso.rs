//! OpenID Connect single sign-on — the **SSO first cut** (SOUL §18, §16 M7, §29).
//!
//! SOUL §29 asked *OIDC vs SAML to ship first*. Resolution: **OIDC** (the
//! Authorization Code flow with PKCE). SAML is deferred — it needs XML-DSig +
//! metadata handling that OIDC's JSON/JWKS surface avoids, and every IdP we care
//! about (Keycloak, Google, Azure AD, Auth0, Okta) speaks OIDC.
//!
//! This module is the protocol engine: provider discovery, JWKS fetch + cache,
//! the authorization-URL builder, the token-endpoint code exchange, and — the
//! security-critical part — `id_token` validation ([`validate_claims`]). It maps a
//! validated token to an [`SsoIdentity`] the API resolves to a
//! [`User`](catalerum_core::model::User) (see
//! [`IamService::resolve_sso_identity`](crate::IamService::resolve_sso_identity)).
//!
//! The stateless PKCE/nonce/state round-trip (a short-lived signed cookie) and the
//! HTTP routes live in `catalerum-api` (`sso_state.rs` + `routes/sso.rs`); this
//! crate owns the identity + verification logic only.
//!
//! ## What `id_token` validation checks
//! - **Signature** against the JWKS key named by the token's `kid` (RS256 minimum,
//!   ES256 also accepted). Unknown `kid` triggers a single JWKS refetch.
//! - **`iss`** equals the configured issuer, **`aud`** contains our `client_id`.
//! - **`exp`** (with a small configurable leeway) and a not-in-the-future **`iat`**.
//! - **`nonce`** equals the nonce we minted for this login (replay/CSRF guard).
//! - **`azp`** equals our `client_id` **when present** (multi-audience tokens).
//!
//! Anything missing or mismatched fails closed — we never guess an identity.

use serde::Deserialize;

use catalerum_core::model::Subject;

use crate::{Error, Result};

/// The default OIDC scopes requested when config leaves `scopes` unset.
pub const DEFAULT_SCOPES: &str = "openid email profile";

/// Default clock skew (seconds) tolerated on `exp`/`iat` when unset.
pub const DEFAULT_LEEWAY_SECS: u64 = 60;

/// The verified identity extracted from a validated `id_token` (SOUL §18). The
/// callback resolves this to a [`User`](catalerum_core::model::User): match on the
/// SSO [`Subject`], else first-login email linking, else JIT provisioning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsoIdentity {
    /// Verified issuer/subject (`iss`/`sub`) — the stable, globally-unique key.
    pub subject: Subject,
    /// `email` claim, if present.
    pub email: Option<String>,
    /// Whether the IdP asserted the email is verified (`email_verified` claim).
    pub email_verified: bool,
    /// Display-name claim (`name`, falling back to `preferred_username`), if any.
    pub display_name: Option<String>,
}

/// Static OIDC client configuration (SOUL §13/§18) — the resolved values the API
/// builds from its `[sso]` TOML section and hands to [`OidcProvider::new`].
#[derive(Clone, Debug)]
pub struct OidcSettings {
    /// The IdP **issuer** URL (discovery base). `iss` in every `id_token` must equal
    /// this exactly.
    pub issuer: String,
    /// This relying party's OAuth client id (also the expected `aud`).
    pub client_id: String,
    /// The confidential client secret (empty for a public client — unusual here).
    pub client_secret: String,
    /// The exact redirect URI registered with the IdP (our `/auth/sso/callback`).
    pub redirect_uri: String,
    /// Space-separated scopes to request (default [`DEFAULT_SCOPES`]).
    pub scopes: String,
    /// Trust the IdP's `email` claim even when `email_verified` is false/absent —
    /// enables first-login email→user linking for IdPs that don't set the flag.
    /// **Off by default** (deny-by-default): without it, only a verified email links.
    pub trust_email: bool,
    /// Send the client secret via HTTP Basic (`client_secret_basic`) rather than in
    /// the token request body (`client_secret_post`, the default).
    pub token_auth_basic: bool,
    /// Clock-skew leeway (seconds) on `exp`/`iat` (default [`DEFAULT_LEEWAY_SECS`]).
    pub leeway_secs: u64,
}

impl OidcSettings {
    /// Whether the configured scope string, trimmed, is non-empty; otherwise the
    /// caller substitutes [`DEFAULT_SCOPES`].
    #[must_use]
    fn effective_scopes(&self) -> &str {
        let s = self.scopes.trim();
        if s.is_empty() {
            DEFAULT_SCOPES
        } else {
            s
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (provider discovery + JWKS + id_token claims)
// ---------------------------------------------------------------------------

/// The subset of the OIDC discovery document we use.
#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

/// A JSON Web Key Set (the IdP's signing keys).
#[derive(Clone, Debug, Deserialize)]
struct Jwks {
    #[serde(default)]
    keys: Vec<Jwk>,
}

impl Jwks {
    /// The key matching `kid` (or, when the token carries no `kid` and the set has
    /// exactly one signing key, that sole key).
    fn find(&self, kid: Option<&str>) -> Option<&Jwk> {
        match kid {
            Some(kid) => self.keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
            None => {
                if self.keys.len() == 1 {
                    self.keys.first()
                } else {
                    None
                }
            }
        }
    }

    /// Whether a key named `kid` is present (drives the refetch-on-unknown-kid).
    fn contains(&self, kid: Option<&str>) -> bool {
        self.find(kid).is_some()
    }
}

/// One JSON Web Key. Only the fields the RS256/ES256 verifiers need are decoded.
#[derive(Clone, Debug, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    // RSA
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    // EC (ES256)
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

/// The token-endpoint response (we only need `id_token`).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: Option<String>,
}

/// Decoded `id_token` claims. Reserved claims (`iss`/`aud`/`exp`) are validated by
/// `jsonwebtoken` itself; the rest are read here for identity + `nonce`/`azp`.
#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    iat: Option<i64>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    azp: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default, deserialize_with = "de_boolish")]
    email_verified: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
}

/// Deserialize a claim that some IdPs send as a JSON bool and others as the string
/// `"true"`/`"false"` (Azure AD, older Keycloak) into a plain `bool`.
fn de_boolish<'de, D>(d: D) -> std::result::Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Boolish {
        Bool(bool),
        Str(String),
    }
    Ok(match Option::<Boolish>::deserialize(d)? {
        Some(Boolish::Bool(b)) => b,
        Some(Boolish::Str(s)) => s.eq_ignore_ascii_case("true") || s == "1",
        None => false,
    })
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// A configured OIDC relying party (SOUL §18). Holds the client config, an HTTP
/// client, and lazily-populated discovery + JWKS caches. Cheap to wrap in an
/// `Arc`; every method is `&self`.
pub struct OidcProvider {
    settings: OidcSettings,
    http: reqwest::Client,
    discovery: tokio::sync::RwLock<Option<Discovery>>,
    jwks: tokio::sync::RwLock<Option<Jwks>>,
}

impl OidcProvider {
    /// Build a provider from resolved [`OidcSettings`]. The HTTP client carries a
    /// short timeout so a hung IdP can never stall a login callback.
    ///
    /// # Errors
    /// [`Error::Invalid`] if `issuer`/`client_id`/`redirect_uri` are blank, or the
    /// HTTP client cannot be built.
    pub fn new(settings: OidcSettings) -> Result<Self> {
        if settings.issuer.trim().is_empty() {
            return Err(Error::invalid("sso: issuer is empty"));
        }
        if settings.client_id.trim().is_empty() {
            return Err(Error::invalid("sso: client_id is empty"));
        }
        if settings.redirect_uri.trim().is_empty() {
            return Err(Error::invalid("sso: redirect_uri is empty"));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::invalid(format!("sso: building HTTP client: {e}")))?;
        Ok(Self {
            settings,
            http,
            discovery: tokio::sync::RwLock::new(None),
            jwks: tokio::sync::RwLock::new(None),
        })
    }

    /// The resolved settings (the routes read `scopes`/`client_id` from here).
    #[must_use]
    pub fn settings(&self) -> &OidcSettings {
        &self.settings
    }

    /// Discover the provider's endpoints (cached after the first fetch). Validates
    /// that the returned `issuer` matches the configured one — a hostile discovery
    /// document must not be able to redirect us to a different token/JWKS host.
    async fn discovery(&self) -> Result<Discovery> {
        if let Some(d) = self.discovery.read().await.clone() {
            return Ok(d);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.settings.issuer.trim_end_matches('/')
        );
        let doc: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(net_err)?
            .error_for_status()
            .map_err(net_err)?
            .json()
            .await
            .map_err(net_err)?;
        // The issuer in the discovery doc MUST equal the one we asked (OIDC Discovery
        // §4.3): otherwise a poisoned document could point us at an attacker's keys.
        if doc.issuer.trim_end_matches('/') != self.settings.issuer.trim_end_matches('/') {
            return Err(Error::unauthorized(
                "sso: discovery issuer does not match configured issuer",
            ));
        }
        *self.discovery.write().await = Some(doc.clone());
        Ok(doc)
    }

    /// Fetch (and cache) the JWKS. `force` bypasses the cache — used once when a
    /// token names a `kid` we haven't seen (routine key rotation).
    async fn fetch_jwks(&self, force: bool) -> Result<Jwks> {
        if !force {
            if let Some(j) = self.jwks.read().await.clone() {
                return Ok(j);
            }
        }
        let jwks_uri = self.discovery().await?.jwks_uri;
        let jwks: Jwks = self
            .http
            .get(&jwks_uri)
            .send()
            .await
            .map_err(net_err)?
            .error_for_status()
            .map_err(net_err)?
            .json()
            .await
            .map_err(net_err)?;
        *self.jwks.write().await = Some(jwks.clone());
        Ok(jwks)
    }

    /// Build the authorization-endpoint redirect URL for the browser (SOUL §18):
    /// Authorization Code flow with PKCE (S256). `code_challenge` is
    /// `base64url(sha256(verifier))`; `state`/`nonce` are the caller's per-login
    /// randoms carried in the signed state cookie.
    ///
    /// # Errors
    /// Network/parse failure during discovery, or a malformed authorization endpoint.
    pub async fn authorization_url(
        &self,
        state: &str,
        nonce: &str,
        code_challenge: &str,
    ) -> Result<String> {
        let endpoint = self.discovery().await?.authorization_endpoint;
        let mut url = reqwest::Url::parse(&endpoint)
            .map_err(|e| Error::invalid(format!("sso: bad authorization_endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.settings.client_id)
            .append_pair("redirect_uri", &self.settings.redirect_uri)
            .append_pair("scope", self.settings.effective_scopes())
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Exchange an authorization `code` for an `id_token` at the token endpoint
    /// (Authorization Code + PKCE). The client secret is sent per
    /// [`OidcSettings::token_auth_basic`] (`client_secret_basic` vs the default
    /// `client_secret_post`).
    async fn exchange_code(&self, code: &str, code_verifier: &str) -> Result<String> {
        let token_endpoint = self.discovery().await?.token_endpoint;
        let mut form: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.settings.redirect_uri),
            ("client_id", &self.settings.client_id),
            ("code_verifier", code_verifier),
        ];
        let mut req = self.http.post(&token_endpoint);
        let secret = self.settings.client_secret.trim();
        if !secret.is_empty() {
            if self.settings.token_auth_basic {
                req = req.basic_auth(&self.settings.client_id, Some(secret));
            } else {
                form.push(("client_secret", secret));
            }
        }
        let resp: TokenResponse = req
            .form(&form)
            .send()
            .await
            .map_err(net_err)?
            .error_for_status()
            .map_err(|e| Error::unauthorized(format!("sso: token exchange rejected: {e}")))?
            .json()
            .await
            .map_err(net_err)?;
        resp.id_token
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| Error::unauthorized("sso: token response carried no id_token"))
    }

    /// Validate an `id_token` against the (cached, refetched-once-on-unknown-kid)
    /// JWKS and the configured issuer/audience, checking `nonce`/`azp` too.
    async fn validate_id_token(&self, id_token: &str, expected_nonce: &str) -> Result<SsoIdentity> {
        let header = jsonwebtoken::decode_header(id_token)
            .map_err(|e| Error::unauthorized(format!("sso: malformed id_token header: {e}")))?;
        let kid = header.kid.clone();
        let mut jwks = self.fetch_jwks(false).await?;
        // Routine key rotation: the token names a key we haven't cached → refetch once.
        if !jwks.contains(kid.as_deref()) {
            jwks = self.fetch_jwks(true).await?;
        }
        validate_claims(id_token, &jwks, &self.settings, expected_nonce)
    }

    /// The full callback path: exchange the `code`, then validate the returned
    /// `id_token` against `expected_nonce`, yielding the verified [`SsoIdentity`].
    ///
    /// # Errors
    /// [`Error::Unauthorized`] on any validation/exchange failure (fail closed);
    /// network faults surface as a core error (→ 5xx at the API).
    pub async fn authenticate(
        &self,
        code: &str,
        code_verifier: &str,
        expected_nonce: &str,
    ) -> Result<SsoIdentity> {
        let id_token = self.exchange_code(code, code_verifier).await?;
        self.validate_id_token(&id_token, expected_nonce).await
    }
}

/// Map a transport-level failure onto a core error (→ 5xx). The message never
/// includes a token or secret — only the network fault.
fn net_err(e: reqwest::Error) -> Error {
    Error::Core(catalerum_core::Error::other(format!(
        "sso: provider request failed: {e}"
    )))
}

/// Build a `jsonwebtoken` [`DecodingKey`](jsonwebtoken::DecodingKey) from a JWK.
/// Only RSA (RS256) and EC P-256 (ES256) are supported.
fn decoding_key(jwk: &Jwk) -> Result<(jsonwebtoken::DecodingKey, jsonwebtoken::Algorithm)> {
    use jsonwebtoken::{Algorithm, DecodingKey};
    match jwk.kty.as_str() {
        "RSA" => {
            let (n, e) = (jwk.n.as_deref(), jwk.e.as_deref());
            let (Some(n), Some(e)) = (n, e) else {
                return Err(Error::unauthorized("sso: RSA JWK missing n/e"));
            };
            let key = DecodingKey::from_rsa_components(n, e)
                .map_err(|e| Error::unauthorized(format!("sso: bad RSA JWK: {e}")))?;
            Ok((key, Algorithm::RS256))
        }
        "EC" => {
            if jwk.crv.as_deref() != Some("P-256") {
                return Err(Error::unauthorized(
                    "sso: only EC P-256 (ES256) is supported",
                ));
            }
            let (Some(x), Some(y)) = (jwk.x.as_deref(), jwk.y.as_deref()) else {
                return Err(Error::unauthorized("sso: EC JWK missing x/y"));
            };
            let key = DecodingKey::from_ec_components(x, y)
                .map_err(|e| Error::unauthorized(format!("sso: bad EC JWK: {e}")))?;
            Ok((key, Algorithm::ES256))
        }
        other => Err(Error::unauthorized(format!(
            "sso: unsupported JWK key type: {other}"
        ))),
    }
}

/// The pure, network-free heart of validation (SOUL §18): verify `token`'s
/// signature against the JWKS key it names, enforce `iss`/`aud`/`exp`/`iat`, then
/// the `nonce`/`azp` binding, and extract the [`SsoIdentity`]. Unit-tested against
/// a fixture keypair + JWKS.
fn validate_claims(
    token: &str,
    jwks: &Jwks,
    settings: &OidcSettings,
    expected_nonce: &str,
) -> Result<SsoIdentity> {
    use jsonwebtoken::{decode, Validation};

    let header = jsonwebtoken::decode_header(token)
        .map_err(|e| Error::unauthorized(format!("sso: malformed id_token header: {e}")))?;
    let jwk = jwks
        .find(header.kid.as_deref())
        .ok_or_else(|| Error::unauthorized("sso: no JWKS key matches the id_token kid"))?;
    let (key, jwk_alg) = decoding_key(jwk)?;
    // Pin the algorithm to the JWK's own type, so a token can't downgrade RS256→HS256
    // (the classic JWKS confusion attack); and require it match the header alg.
    if header.alg != jwk_alg {
        return Err(Error::unauthorized(
            "sso: id_token alg does not match the signing key",
        ));
    }

    let mut validation = Validation::new(jwk_alg);
    validation.algorithms = vec![jwk_alg];
    // `iss` must equal the configured issuer, `aud` must contain our client id, and
    // `exp` (+ `nbf` when present) is enforced with a small clock-skew leeway.
    validation.set_issuer(&[settings.issuer.trim_end_matches('/')]);
    validation.set_audience(&[settings.client_id.as_str()]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.leeway = settings.leeway_secs;

    let data = decode::<IdTokenClaims>(token, &key, &validation)
        .map_err(|e| Error::unauthorized(format!("sso: id_token invalid: {e}")))?;
    let claims = data.claims;

    // Nonce binding: the token MUST echo the nonce we minted for this login.
    match claims.nonce.as_deref() {
        Some(n) if n == expected_nonce => {}
        _ => return Err(Error::unauthorized("sso: id_token nonce mismatch")),
    }

    // `azp` (authorized party), when present, must be our client (multi-aud tokens).
    if let Some(azp) = claims.azp.as_deref() {
        if azp != settings.client_id {
            return Err(Error::unauthorized("sso: id_token azp is not this client"));
        }
    }

    // `iat` must not be in the future beyond the leeway (a token minted "later" than
    // now is a clock/forgery smell). `exp` is already enforced by `jsonwebtoken`.
    if let Some(iat) = claims.iat {
        let now = chrono::Utc::now().timestamp();
        if iat > now + settings.leeway_secs as i64 {
            return Err(Error::unauthorized("sso: id_token iat is in the future"));
        }
    }

    if claims.sub.trim().is_empty() {
        return Err(Error::unauthorized("sso: id_token has an empty sub"));
    }

    let display_name = claims
        .name
        .filter(|s| !s.trim().is_empty())
        .or(claims.preferred_username.filter(|s| !s.trim().is_empty()));

    Ok(SsoIdentity {
        subject: Subject {
            issuer: settings.issuer.trim_end_matches('/').to_string(),
            subject: claims.sub,
        },
        email: claims.email.filter(|s| !s.trim().is_empty()),
        email_verified: claims.email_verified,
        display_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed RSA-2048 test keypair (PKCS#8 PEM) + its JWKS `n`/`e` (base64url).
    // Generated once with openssl; used only to sign/verify fixtures in these tests.
    const TEST_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDN7d4EqfzHvnz9\n\
t/naJPLgclGbZ/rHwL4c/+Hr65bEwWkF7MIDYqcUtmqGdyi8VrMt42IKwnJyoxx4\n\
fLGOtb/jbW+YmXLQoPLrDSXvRAsWpxpkmYPum5skR/pG4kHySFOJ3K4FW1At8rvc\n\
PYCFkRd+e6QXpinM8eccTSE3MKKKDd4XUsm/1v0+5ryzxrKKY/pIExaGXxev8oO5\n\
nL/2cVx8hngf1ee2QFn3idrAnJKH9gtYqinJVomEYPOJrx9RPckct/mh182ALU5C\n\
Yu0aX1yTm1bRA4bzI8EufNBzW5u643X/npArzdKfOoUs9A0j6dm0/ZYYO30EqFDA\n\
kMqV6ODjAgMBAAECggEAUEMndyroDtREdEFqPSeMkIWOICOxX3zUvInRPPo4a9y0\n\
ee4zGk2vsId+0oUMGAg00yxecLIkGGFRvfZf4C8fqN1lExWv5fftZkbcI7siFUSx\n\
KUeaX/w/Ri9VsZ4LNQsSoFembgkOobILnYZNGwIXpaE8LkmB3lLkkKfRS+kFWQfm\n\
L6cLlyym5QHpsM9pV31SwcNDUOhQy+SRRpcS9WlrgykUNvKazOd5e0Jo34ohDemu\n\
NYhEAJhNxhWv9z1e7ROd0JI+BL2LH/gDul9Lw/xxNiSN+hNBE8SbvVjAl9FiWRma\n\
paI6O0FP2Sq+7dmNUex+sdIGm5f5mzFYbiU5iNpGFQKBgQD3Uk4frDJIVJJv3kpJ\n\
susmvTTW8Naxx41NwKStL5qn4x66AB6isrwOd1LTsVN3ygFLWLlEteGkApAXZ9IY\n\
KzcNarem71tbCKjIsP54+iIJU/ugeeq5AiTe+Y+An5JQ8flIhkIPoYTHx6e2bpf8\n\
Wo4Jo4UIgMiEDcfxkVjrWtYhpQKBgQDVJ7vYod1msOyt8OaEZ0w5tks2jCaZpmBC\n\
GkyRwIk3F2hM1q53ub8H2/VhoU7NFIefeImTkpBolmbUk4p5XHN0R+Q5oc8GlrHw\n\
pTiOK6w2g6a/INmQOE5hBacaO7nWG4M1OUqLOAsayFBYTmXQvY3v7Ocq/ierWKw7\n\
Anujx11h5wKBgEvfIvpScCaCU14gOnf7fGoo9zHNNn/ZcP7eT2aVyQMiCMYUzVEq\n\
NcjWUEGDD9Ea1mTP9h4fEfanlp6nietCLqReDbMXkNYPhP/0VEy2p4RnEDV90UUq\n\
ZDdHJf/WdCOC5++YyGFVMo+7LzcnHFcdTJ+mW2RtZZYlSCZSaY3iEvjFAoGAXuPU\n\
XQkZ3dhPVNPUWwb9SQfdDchweqA1Y9f/VDdJHmxeMy6y9nuLDj2eTDsaMHO+OIDZ\n\
hgeOH/Esj9+qmoJMp2xFrl5ZIk69oip7Ndc9T/tlpNpD4E8gnVJ95FDIVwdibrQ1\n\
eiqVzvNzyQwFiVqJMFDfTCVelYnhClf9oJhk+usCgYBqp8/9k4s56h7DbgfhTR9d\n\
mCyNrQmx8ufkjEoRRjNSYG+8gp1dnVejsyqy7GgJ7eFTYjgjbuGeBbv/8RalG4ww\n\
dMoAauIkrjsO9b0f2/5IY5L/U1AgjgyOQiUo3Ag11TzEW75hMG5zrc95G7Bp/3ht\n\
f955g1MAG5QuxwVe/kUZ1Q==\n\
-----END PRIVATE KEY-----\n";

    const TEST_KEY_N: &str = "ze3eBKn8x758_bf52iTy4HJRm2f6x8C-HP_h6-uWxMFpBezCA2KnFLZqhncovFazLeNiCsJycqMceHyxjrW_421vmJly0KDy6w0l70QLFqcaZJmD7pubJEf6RuJB8khTidyuBVtQLfK73D2AhZEXfnukF6YpzPHnHE0hNzCiig3eF1LJv9b9Pua8s8ayimP6SBMWhl8Xr_KDuZy_9nFcfIZ4H9XntkBZ94nawJySh_YLWKopyVaJhGDzia8fUT3JHLf5odfNgC1OQmLtGl9ck5tW0QOG8yPBLnzQc1ubuuN1_56QK83SnzqFLPQNI-nZtP2WGDt9BKhQwJDKlejg4w";
    const TEST_KEY_E: &str = "AQAB";
    const TEST_KID: &str = "test-key-1";

    fn settings() -> OidcSettings {
        OidcSettings {
            issuer: "https://idp.example.com".into(),
            client_id: "catalerum-client".into(),
            client_secret: "s3cret".into(),
            redirect_uri: "https://app.example.com/auth/sso/callback".into(),
            scopes: DEFAULT_SCOPES.into(),
            trust_email: false,
            token_auth_basic: false,
            leeway_secs: DEFAULT_LEEWAY_SECS,
        }
    }

    fn test_jwks() -> Jwks {
        Jwks {
            keys: vec![Jwk {
                kty: "RSA".into(),
                kid: Some(TEST_KID.into()),
                n: Some(TEST_KEY_N.into()),
                e: Some(TEST_KEY_E.into()),
                crv: None,
                x: None,
                y: None,
            }],
        }
    }

    /// Sign an `id_token` with the test key. `claims` is any serde value.
    fn sign(claims: serde_json::Value, kid: Option<&str>) -> String {
        use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
        let mut header = Header::new(Algorithm::RS256);
        header.kid = kid.map(str::to_string);
        let key = EncodingKey::from_rsa_pem(TEST_KEY_PEM.as_bytes()).expect("valid test PEM");
        encode(&header, &claims, &key).expect("sign")
    }

    fn base_claims() -> serde_json::Value {
        let now = chrono::Utc::now().timestamp();
        serde_json::json!({
            "iss": "https://idp.example.com",
            "sub": "user-abc-123",
            "aud": "catalerum-client",
            "exp": now + 3600,
            "iat": now,
            "nonce": "nonce-xyz",
            "email": "alice@example.com",
            "email_verified": true,
            "name": "Alice Example",
        })
    }

    #[test]
    fn valid_token_yields_identity() {
        let token = sign(base_claims(), Some(TEST_KID));
        let id = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap();
        assert_eq!(id.subject.issuer, "https://idp.example.com");
        assert_eq!(id.subject.subject, "user-abc-123");
        assert_eq!(id.email.as_deref(), Some("alice@example.com"));
        assert!(id.email_verified);
        assert_eq!(id.display_name.as_deref(), Some("Alice Example"));
    }

    #[test]
    fn wrong_nonce_is_rejected() {
        let token = sign(base_claims(), Some(TEST_KID));
        let err =
            validate_claims(&token, &test_jwks(), &settings(), "different-nonce").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let mut c = base_claims();
        c["aud"] = serde_json::json!("some-other-client");
        let token = sign(c, Some(TEST_KID));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let mut c = base_claims();
        c["iss"] = serde_json::json!("https://evil.example.com");
        let token = sign(c, Some(TEST_KID));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn expired_token_is_rejected() {
        let now = chrono::Utc::now().timestamp();
        let mut c = base_claims();
        c["exp"] = serde_json::json!(now - 3600);
        c["iat"] = serde_json::json!(now - 7200);
        let token = sign(c, Some(TEST_KID));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn future_iat_is_rejected() {
        let now = chrono::Utc::now().timestamp();
        let mut c = base_claims();
        c["iat"] = serde_json::json!(now + 3600);
        let token = sign(c, Some(TEST_KID));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn azp_mismatch_is_rejected() {
        let mut c = base_claims();
        c["azp"] = serde_json::json!("another-client");
        let token = sign(c, Some(TEST_KID));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn unknown_kid_finds_no_key() {
        // A token whose `kid` isn't in the JWKS is rejected (the async wrapper would
        // refetch once; the pure validator simply fails to match).
        let token = sign(base_claims(), Some("rotated-away"));
        let err = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = sign(base_claims(), Some(TEST_KID));
        // Flip the last char of the signature segment.
        let mut parts: Vec<&str> = token.split('.').collect();
        let sig = parts[2].to_string();
        let mutated: String = {
            let mut s = sig.clone();
            let last = s.pop().unwrap();
            let repl = if last == 'A' { 'B' } else { 'A' };
            s.push(repl);
            s
        };
        parts[2] = &mutated;
        let forged = parts.join(".");
        let err = validate_claims(&forged, &test_jwks(), &settings(), "nonce-xyz").unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)));
    }

    #[test]
    fn email_verified_accepts_string_form() {
        let mut c = base_claims();
        c["email_verified"] = serde_json::json!("true"); // Azure-style string bool
        let token = sign(c, Some(TEST_KID));
        let id = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap();
        assert!(id.email_verified);
    }

    #[test]
    fn array_audience_containing_client_is_accepted() {
        let mut c = base_claims();
        c["aud"] = serde_json::json!(["catalerum-client", "other-service"]);
        c["azp"] = serde_json::json!("catalerum-client");
        let token = sign(c, Some(TEST_KID));
        let id = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap();
        assert_eq!(id.subject.subject, "user-abc-123");
    }

    #[test]
    fn missing_display_name_falls_back_to_preferred_username() {
        let mut c = base_claims();
        c.as_object_mut().unwrap().remove("name");
        c["preferred_username"] = serde_json::json!("alice");
        let token = sign(c, Some(TEST_KID));
        let id = validate_claims(&token, &test_jwks(), &settings(), "nonce-xyz").unwrap();
        assert_eq!(id.display_name.as_deref(), Some("alice"));
    }
}
