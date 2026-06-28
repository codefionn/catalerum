//! The stateless, signed **SSO state token** (SOUL §18/§29).
//!
//! The OIDC Authorization Code + PKCE flow needs to carry three per-login secrets
//! across the round-trip to the IdP and back: the CSRF `state`, the replay `nonce`,
//! and the PKCE `code_verifier`. Rather than a server-side session table, we mint a
//! tiny HMAC-signed token (this module) and hand it to the browser as a short-lived
//! `HttpOnly`, `SameSite=Lax` cookie. The callback re-verifies + consumes it, so the
//! signing key **is** the trust: a tampered or forged token never decodes.
//!
//! This mirrors [`crate::trigger_link`] / [`crate::download_link`] exactly — the
//! same `<payload>.<sig>` shape, constant-time verify, and [`VerifyError`] →
//! opaque-failure mapping — but keeps an independent secret (`[sso].state_secret`)
//! so it rotates separately, and adds an absolute `exp` (≈10 min) since a login
//! that isn't completed promptly should not be resumable.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::download_link::VerifyError;

type HmacSha256 = Hmac<Sha256>;

/// How long a minted SSO state token stays valid — the window a user has to finish
/// authenticating at the IdP and land back on the callback.
pub const SSO_STATE_TTL_SECS: i64 = 600;

/// The claims an SSO state token attests to. Short serde keys keep the cookie
/// compact. `redirect_after` is the (validated same-origin) SPA path to land on
/// after login; the callback re-validates it before use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SsoStateClaims {
    /// The CSRF `state` echoed by the IdP — the callback compares it to the `?state`
    /// query param.
    #[serde(rename = "s")]
    pub state: String,
    /// The OIDC `nonce` — bound into the `id_token` and re-checked at validation.
    #[serde(rename = "n")]
    pub nonce: String,
    /// The PKCE `code_verifier` — replayed to the token endpoint (S256 challenge).
    #[serde(rename = "v")]
    pub pkce_verifier: String,
    /// Same-origin SPA path to redirect to after a successful login (default `/`).
    #[serde(rename = "r")]
    pub redirect_after: String,
    /// Absolute expiry, Unix seconds.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Mints + verifies [`SsoStateClaims`] tokens under a process-wide HMAC key. Cheap
/// to clone (`Arc`-backed key); held by [`AppState`](crate::state::AppState).
#[derive(Clone)]
pub struct SsoStateSigner {
    key: Arc<[u8; 32]>,
}

impl SsoStateSigner {
    /// A signer whose key is derived (SHA-256) from a configured secret
    /// (`[sso].state_secret`), so the operator may use any string while the HMAC gets
    /// a full-width key. A stable secret lets an in-flight login survive a restart /
    /// span pods.
    #[must_use]
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        }
    }

    /// A signer with a fresh random key (no configured secret) — in-flight logins
    /// stop verifying once the process exits (fine for single-pod dev). Entropy is
    /// two v4 UUIDs folded through SHA-256, so no `rand` dependency is pulled in.
    #[must_use]
    pub fn random() -> Self {
        let mut seed = [0u8; 32];
        seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self {
            key: Arc::new(Sha256::digest(seed).into()),
        }
    }

    /// The signer for a config: the configured secret when set, else a random
    /// per-process key (with a one-line log so the operator knows tokens are
    /// ephemeral).
    #[must_use]
    pub fn from_config(secret: Option<&str>) -> Self {
        match secret.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Self::from_secret(s),
            None => {
                tracing::info!(
                    "no [sso].state_secret set; using a random per-process key — \
                     in-flight SSO logins won't survive a restart or span pods"
                );
                Self::random()
            }
        }
    }

    /// HMAC-SHA256 of `msg` under the key.
    fn tag(&self, msg: &[u8]) -> [u8; 32] {
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_slice()).expect("HMAC accepts a 32-byte key");
        mac.update(msg);
        mac.finalize().into_bytes().into()
    }

    /// Mint a `<payload>.<sig>` token for `claims` (base64url, URL-safe, no padding).
    #[must_use]
    pub fn mint(&self, claims: &SsoStateClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.tag(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Checks the signature in **constant
    /// time** before decoding, then rejects an expired token (`now` is Unix seconds).
    /// Any structural problem is [`VerifyError::Malformed`].
    pub fn verify(&self, token: &str, now: i64) -> Result<SsoStateClaims, VerifyError> {
        let (payload, sig_b64) = token.split_once('.').ok_or(VerifyError::Malformed)?;
        let sig = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| VerifyError::Malformed)?;
        // Constant-time signature check first — never trust an unsigned payload.
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_slice()).expect("HMAC accepts a 32-byte key");
        mac.update(payload.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| VerifyError::BadSignature)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| VerifyError::Malformed)?;
        let claims: SsoStateClaims =
            serde_json::from_slice(&json).map_err(|_| VerifyError::Malformed)?;
        if claims.exp <= now {
            return Err(VerifyError::Expired);
        }
        Ok(claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> SsoStateClaims {
        SsoStateClaims {
            state: "state-abc".into(),
            nonce: "nonce-xyz".into(),
            pkce_verifier: "verifier-0123456789".into(),
            redirect_after: "/settings".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let signer = SsoStateSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        assert_eq!(signer.verify(&token, 999).unwrap(), claims());
    }

    #[test]
    fn verify_rejects_expired() {
        let signer = SsoStateSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        assert_eq!(signer.verify(&token, 1_000), Err(VerifyError::Expired));
        assert_eq!(signer.verify(&token, 5_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = SsoStateSigner::from_secret("key-a");
        let b = SsoStateSigner::from_secret("key-b");
        let token = a.mint(&claims());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        // Forge a payload that swaps in a different nonce/verifier, keep the old sig.
        let signer = SsoStateSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        let (payload, sig) = token.split_once('.').unwrap();
        let mut forged = claims();
        forged.nonce = "attacker-nonce".into();
        forged.pkce_verifier = "attacker-verifier".into();
        let bad_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(bad_payload, payload);
        assert_eq!(
            signer.verify(&format!("{bad_payload}.{sig}"), 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_and_checks_signature_first() {
        let signer = SsoStateSigner::from_secret("s3cret");
        assert_eq!(signer.verify("nodot", 0), Err(VerifyError::Malformed));
        assert_eq!(signer.verify("payload.##", 0), Err(VerifyError::Malformed));
        // A garbage payload with a syntactically valid but wrong signature → BadSig
        // (the payload is never trusted before the HMAC).
        assert_eq!(
            signer.verify("anything.AAAA", 0),
            Err(VerifyError::BadSignature)
        );
        // A correctly signed non-JSON payload → Malformed (not BadSig).
        let payload = URL_SAFE_NO_PAD.encode(b"not json at all");
        let sig = URL_SAFE_NO_PAD.encode(signer.tag(payload.as_bytes()));
        assert_eq!(
            signer.verify(&format!("{payload}.{sig}"), 0),
            Err(VerifyError::Malformed)
        );
    }

    #[test]
    fn random_signers_have_independent_keys() {
        let a = SsoStateSigner::random();
        let b = SsoStateSigner::random();
        let token = a.mint(&claims());
        assert!(a.verify(&token, 0).is_ok());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }
}
