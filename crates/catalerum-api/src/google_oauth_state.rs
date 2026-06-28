//! The stateless, signed **Google-OAuth state token** (SOUL §16 M7/§18).
//!
//! The Google authorization-code flow round-trips through Google's consent screen
//! and back to `/auth/google/callback` as a **top-level browser navigation** — no
//! `Authorization` header rides along — so the callback cannot know which
//! workspace (or which existing connection) the tokens belong to from the request
//! alone. Instead the authenticated `/auth/google/connect` route mints a tiny
//! HMAC-signed token carrying the CSRF `state`, the caller's `workspace_id`, and
//! the target connection/kind, and hands it to the browser as a short-lived
//! `HttpOnly`, `SameSite=Lax` cookie. The callback re-verifies + consumes it, so
//! the signing key **is** the trust: a tampered or forged token never decodes.
//!
//! This mirrors [`crate::sso_state`] exactly — the same `<payload>.<sig>` shape,
//! constant-time verify, [`VerifyError`] mapping, and absolute `exp` — but keeps an
//! **independent** secret (`[google].state_secret`) so it rotates separately from
//! the SSO signer (never reusing the SSO signer's key).

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use catalerum_core::id::WorkspaceId;

use crate::download_link::VerifyError;

type HmacSha256 = Hmac<Sha256>;

/// How long a minted Google-OAuth state token stays valid — the window a user has
/// to finish consenting at Google and land back on the callback.
pub const GOOGLE_STATE_TTL_SECS: i64 = 600;

/// The claims a Google-OAuth state token attests to. Short serde keys keep the
/// cookie compact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleStateClaims {
    /// The CSRF `state` echoed by Google — the callback compares it to `?state`.
    #[serde(rename = "s")]
    pub state: String,
    /// The workspace the connection (and its encrypted tokens) belongs to. Trusted
    /// from this signed cookie because it was minted for an authenticated caller.
    #[serde(rename = "w")]
    pub workspace_id: WorkspaceId,
    /// What is being connected (`"calendar"`), so the callback picks the right
    /// connection kind / config shape.
    #[serde(rename = "k")]
    pub kind: String,
    /// An existing connection id to **re-authorize** in place (rotate its sealed
    /// tokens); absent ⇒ the callback creates a fresh connection.
    #[serde(rename = "c", default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// Same-origin SPA path to redirect to after a successful connect.
    #[serde(rename = "r")]
    pub redirect_after: String,
    /// Absolute expiry, Unix seconds.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Mints + verifies [`GoogleStateClaims`] tokens under a process-wide HMAC key.
/// Cheap to clone (`Arc`-backed key); held by [`AppState`](crate::state::AppState).
#[derive(Clone)]
pub struct GoogleStateSigner {
    key: Arc<[u8; 32]>,
}

impl GoogleStateSigner {
    /// A signer whose key is derived (SHA-256) from a configured secret
    /// (`[google].state_secret`) so a stable secret lets an in-flight connect
    /// survive a restart / span pods.
    #[must_use]
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        }
    }

    /// A signer with a fresh random key (no configured secret). Entropy is two v4
    /// UUIDs folded through SHA-256 (no `rand` dependency).
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
                    "no [google].state_secret set; using a random per-process key — \
                     in-flight Google connects won't survive a restart or span pods"
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
    pub fn mint(&self, claims: &GoogleStateClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.tag(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Checks the signature in **constant
    /// time** before decoding, then rejects an expired token (`now` is Unix
    /// seconds). Any structural problem is [`VerifyError::Malformed`].
    pub fn verify(&self, token: &str, now: i64) -> Result<GoogleStateClaims, VerifyError> {
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
        let claims: GoogleStateClaims =
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

    fn claims() -> GoogleStateClaims {
        GoogleStateClaims {
            state: "state-abc".into(),
            workspace_id: WorkspaceId::new(),
            kind: "calendar".into(),
            connection: Some("conn-1".into()),
            redirect_after: "/settings".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let signer = GoogleStateSigner::from_secret("s3cret");
        let c = claims();
        let token = signer.mint(&c);
        assert_eq!(signer.verify(&token, 999).unwrap(), c);
    }

    #[test]
    fn verify_rejects_expired() {
        let signer = GoogleStateSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        assert_eq!(signer.verify(&token, 1_000), Err(VerifyError::Expired));
        assert_eq!(signer.verify(&token, 5_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = GoogleStateSigner::from_secret("key-a");
        let b = GoogleStateSigner::from_secret("key-b");
        let token = a.mint(&claims());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        // Forge a payload that swaps the workspace, keep the old sig — must fail.
        let signer = GoogleStateSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        let (payload, sig) = token.split_once('.').unwrap();
        let mut forged = claims();
        forged.workspace_id = WorkspaceId::new();
        let bad_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(bad_payload, payload);
        assert_eq!(
            signer.verify(&format!("{bad_payload}.{sig}"), 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_and_checks_signature_first() {
        let signer = GoogleStateSigner::from_secret("s3cret");
        assert_eq!(signer.verify("nodot", 0), Err(VerifyError::Malformed));
        assert_eq!(signer.verify("payload.##", 0), Err(VerifyError::Malformed));
        assert_eq!(
            signer.verify("anything.AAAA", 0),
            Err(VerifyError::BadSignature)
        );
        let payload = URL_SAFE_NO_PAD.encode(b"not json at all");
        let sig = URL_SAFE_NO_PAD.encode(signer.tag(payload.as_bytes()));
        assert_eq!(
            signer.verify(&format!("{payload}.{sig}"), 0),
            Err(VerifyError::Malformed)
        );
    }

    #[test]
    fn random_signers_have_independent_keys() {
        let a = GoogleStateSigner::random();
        let b = GoogleStateSigner::random();
        let token = a.mint(&claims());
        assert!(a.verify(&token, 0).is_ok());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }
}
