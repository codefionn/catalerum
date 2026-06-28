//! Signed, single-purpose **trigger-fire links** (SOUL §11/§12/§18).
//!
//! A trigger link is a **self-contained, unauthenticated** URL an external service
//! (a CI job, a device, a third-party webhook) can `POST` to fire one named
//! automation signal — the public counterpart to the authenticated `fire_trigger`
//! tool and `POST /triggers/{name}` route. The link carries an opaque token that
//! **is** its own authorization: an HMAC-SHA256 signature over a tiny claim set
//! naming exactly `{workspace, name, expiry}`. The public redeem route
//! ([`crate::routes::triggers`]) re-verifies the signature + expiry, then dispatches
//! that one named signal in that one workspace (§18) — nothing else.
//!
//! This is deliberately narrower than a bearer token (SOUL §19): a leaked link can
//! only fire **one** named trigger for a **short** window, never reach the workspace
//! at large. Each automation the signal fans out to still runs under its own §19
//! authority, so the link's power is bounded by what those automations already do.
//!
//! Mirrors [`crate::download_link`] (same stateless HMAC token shape, same
//! [`VerifyError`] → flat-`404` mapping); the two keep independent secrets
//! (`[server].trigger_secret` vs `[server].download_secret`) so an operator can
//! rotate one without invalidating the other.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use catalerum_core::WorkspaceId;

use crate::download_link::VerifyError;

type HmacSha256 = Hmac<Sha256>;

/// The claims a trigger-fire token attests to — kept tiny (short serde keys) so the
/// token stays URL-friendly. `name` is the signal fired (matched against a
/// `{ "kind": "trigger", "name": … }` trigger); `exp` is a Unix-seconds expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerClaims {
    /// The workspace the signal fires in (SOUL §18) — redemption is scoped to it.
    #[serde(rename = "w")]
    pub workspace_id: WorkspaceId,
    /// The signal name to fire (exact match against `trigger` triggers).
    #[serde(rename = "n")]
    pub name: String,
    /// Absolute expiry, Unix seconds. Redemption past this fails.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Mints and verifies [`TriggerClaims`] tokens with a process-wide HMAC key. Cheap
/// to clone (`Arc`-backed key); held by both the `trigger_link` tool / mint route
/// and [`AppState`](crate::state::AppState) (verify at the public redeem route).
#[derive(Clone)]
pub struct TriggerSigner {
    key: Arc<[u8; 32]>,
}

impl TriggerSigner {
    /// A signer whose key is derived (via SHA-256) from a configured secret
    /// (`[server].trigger_secret`), so the operator may use any human-chosen string
    /// while the HMAC always gets a full-width 32-byte key. A stable secret makes
    /// links survive restarts and verify across pods.
    #[must_use]
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        }
    }

    /// A signer with a fresh random key (no configured secret). Links it mints stop
    /// verifying once the process exits — fine for single-pod dev. Entropy comes from
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
    /// per-process key (with a one-line log so the operator knows links are
    /// ephemeral).
    #[must_use]
    pub fn from_config(secret: Option<&str>) -> Self {
        match secret.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Self::from_secret(s),
            None => {
                tracing::info!(
                    "no [server].trigger_secret set; using a random per-process key — \
                     trigger-fire links won't survive a restart or span pods"
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

    /// Mint a `<payload>.<sig>` token for `claims`. `payload` is base64url(JSON);
    /// `sig` is base64url(HMAC(payload)). URL-safe, no padding — drops straight into
    /// a path segment.
    #[must_use]
    pub fn mint(&self, claims: &TriggerClaims) -> String {
        // `TriggerClaims` is always serializable (uuid + string + int), so this never
        // fails in practice.
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.tag(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Checks the signature in **constant
    /// time** (via `verify_slice`) before decoding, then rejects an expired link
    /// (`now` is Unix seconds). Any structural problem is [`VerifyError::Malformed`].
    pub fn verify(&self, token: &str, now: i64) -> Result<TriggerClaims, VerifyError> {
        let (payload, sig_b64) = token.split_once('.').ok_or(VerifyError::Malformed)?;
        let sig = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| VerifyError::Malformed)?;
        // Constant-time signature check first — never touch the payload of a token we
        // didn't sign.
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_slice()).expect("HMAC accepts a 32-byte key");
        mac.update(payload.as_bytes());
        mac.verify_slice(&sig)
            .map_err(|_| VerifyError::BadSignature)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| VerifyError::Malformed)?;
        let claims: TriggerClaims =
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

    fn claims() -> TriggerClaims {
        TriggerClaims {
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(1)),
            name: "rebuild-report".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let signer = TriggerSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        assert_eq!(signer.verify(&token, 999).unwrap(), claims());
    }

    #[test]
    fn verify_rejects_expired() {
        let signer = TriggerSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        // exp is 1000; at exactly 1000 (and after) it's dead.
        assert_eq!(signer.verify(&token, 1_000), Err(VerifyError::Expired));
        assert_eq!(signer.verify(&token, 5_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = TriggerSigner::from_secret("key-a");
        let b = TriggerSigner::from_secret("key-b");
        let token = a.mint(&claims());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let signer = TriggerSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        let (payload, sig) = token.split_once('.').unwrap();
        // Re-encode a payload that escalates to another workspace, keep the old sig.
        let mut forged = claims();
        forged.workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(999));
        let bad_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(bad_payload, payload);
        assert_eq!(
            signer.verify(&format!("{bad_payload}.{sig}"), 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_and_checks_signature_first() {
        let signer = TriggerSigner::from_secret("s3cret");
        // No `.` separator; a non-base64url signature.
        assert_eq!(signer.verify("nodot", 0), Err(VerifyError::Malformed));
        assert_eq!(signer.verify("payload.##", 0), Err(VerifyError::Malformed));
        // A garbage payload with a syntactically valid but wrong signature is rejected
        // as BadSignature — the payload is never trusted before the HMAC.
        assert_eq!(
            signer.verify("anything.AAAA", 0),
            Err(VerifyError::BadSignature)
        );
        // A correctly signed payload that isn't valid JSON → Malformed (not BadSig).
        let payload = URL_SAFE_NO_PAD.encode(b"not json at all");
        let sig = URL_SAFE_NO_PAD.encode(signer.tag(payload.as_bytes()));
        assert_eq!(
            signer.verify(&format!("{payload}.{sig}"), 0),
            Err(VerifyError::Malformed)
        );
    }
}
