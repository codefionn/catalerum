//! Signed, single-purpose **Google push-channel tokens** (SOUL §8/§11/§16 M7).
//!
//! When the ingest scan registers a Google `events.watch` channel it hands Google
//! a per-channel secret token; Google echoes that token back in the
//! `X-Goog-Channel-Token` header on every notification it POSTs to the public
//! webhook ([`crate::routes::google_calendar_push`]). The token **is** its own
//! authorization: an HMAC-SHA256 signature over a tiny claim set naming exactly
//! `{workspace, connection, expiry}`. The webhook re-verifies the signature +
//! expiry (constant-time) and reads the workspace/connection straight from the
//! claims — so it needs **no** database lookup by channel id, and a forged/expired
//! token collapses to an opaque `404` (SOUL §18) revealing nothing.
//!
//! Mirrors [`crate::trigger_link`] / [`crate::download_link`] (same stateless HMAC
//! token shape, same [`VerifyError`] → flat-`404` mapping) and keeps an independent
//! secret (`[google].push_secret`) so it rotates separately.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use catalerum_core::id::ConnectionId;
use catalerum_core::WorkspaceId;

use crate::download_link::VerifyError;

type HmacSha256 = Hmac<Sha256>;

/// The claims a push-channel token attests to — kept tiny (short serde keys) so the
/// token stays header-friendly. `connection` is the Google calendar connection the
/// channel watches; on a verified notification the webhook enqueues a collect for
/// every enabled collect automation on it. `exp` is a Unix-seconds expiry (set to
/// outlive the Google channel; the scan re-mints on renewal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelClaims {
    /// The workspace the connection lives in (SOUL §18) — notifications are scoped to it.
    #[serde(rename = "w")]
    pub workspace_id: WorkspaceId,
    /// The Google calendar connection this channel watches.
    #[serde(rename = "c")]
    pub connection_id: ConnectionId,
    /// Absolute expiry, Unix seconds. A notification past this fails to verify.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Mints and verifies [`ChannelClaims`] tokens with a process-wide HMAC key. Cheap
/// to clone (`Arc`-backed key); held by both the ingest watch scan (mint) and
/// [`AppState`](crate::state::AppState) (verify at the public webhook).
#[derive(Clone)]
pub struct GoogleChannelSigner {
    key: Arc<[u8; 32]>,
}

impl GoogleChannelSigner {
    /// A signer whose key is derived (via SHA-256) from a configured secret
    /// (`[google].push_secret`), so the operator may use any string while the HMAC
    /// always gets a full-width 32-byte key. A stable secret makes channel tokens
    /// survive restarts and verify across pods.
    #[must_use]
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        }
    }

    /// A signer with a fresh random key (no configured secret). Tokens it mints stop
    /// verifying once the process exits (single-pod dev). Entropy comes from two v4
    /// UUIDs folded through SHA-256, so no `rand` dependency is pulled in.
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
    /// per-process key (with a one-line log so the operator knows channel tokens are
    /// ephemeral).
    #[must_use]
    pub fn from_config(secret: Option<&str>) -> Self {
        match secret.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => Self::from_secret(s),
            None => {
                tracing::info!(
                    "no [google].push_secret set; using a random per-process key — Google \
                     push-channel tokens won't survive a restart or span pods (notifications \
                     for pre-restart channels 404 and fall back to the poll until renewed)"
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
    /// `sig` is base64url(HMAC(payload)). URL-safe, no padding — a valid HTTP header
    /// value.
    #[must_use]
    pub fn mint(&self, claims: &ChannelClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.tag(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Checks the signature in **constant
    /// time** (via `verify_slice`) before decoding, then rejects an expired token
    /// (`now` is Unix seconds). Any structural problem is [`VerifyError::Malformed`].
    pub fn verify(&self, token: &str, now: i64) -> Result<ChannelClaims, VerifyError> {
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
        let claims: ChannelClaims =
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

    fn claims() -> ChannelClaims {
        ChannelClaims {
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(1)),
            connection_id: ConnectionId::from_uuid(uuid::Uuid::from_u128(7)),
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let signer = GoogleChannelSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        assert_eq!(signer.verify(&token, 999).unwrap(), claims());
    }

    #[test]
    fn verify_rejects_expired() {
        let signer = GoogleChannelSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        // exp is 1000; at exactly 1000 (and after) it's dead.
        assert_eq!(signer.verify(&token, 1_000), Err(VerifyError::Expired));
        assert_eq!(signer.verify(&token, 5_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = GoogleChannelSigner::from_secret("key-a");
        let b = GoogleChannelSigner::from_secret("key-b");
        let token = a.mint(&claims());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let signer = GoogleChannelSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        let (payload, sig) = token.split_once('.').unwrap();
        // Re-encode a payload escalating to another connection, keep the old sig.
        let mut forged = claims();
        forged.connection_id = ConnectionId::from_uuid(uuid::Uuid::from_u128(999));
        let bad_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(bad_payload, payload);
        assert_eq!(
            signer.verify(&format!("{bad_payload}.{sig}"), 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_and_checks_signature_first() {
        let signer = GoogleChannelSigner::from_secret("s3cret");
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
