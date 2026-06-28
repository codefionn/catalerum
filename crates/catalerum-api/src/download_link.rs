//! Signed, single-purpose download links (SOUL §9/§18).
//!
//! A download link is a **self-contained, unauthenticated** URL the agent (via the
//! `download_link` tool) hands the user so they can fetch one stored file — or a
//! whole directory as a `.tar.gz` — straight from the API, with no login. The link
//! carries an opaque token that **is** its own authorization: an HMAC-SHA256
//! signature over a tiny claim set naming exactly `{workspace, store, key, dir,
//! expiry}`. The public redeem route ([`crate::routes::download`]) re-verifies the
//! signature and expiry before streaming a single byte.
//!
//! This is deliberately narrower than a bearer token (SOUL §19): a leaked link
//! grants read of **one** object/prefix for a **short** window, never the
//! workspace at large. The signing key is a process-wide secret — configured
//! (`[server].download_secret`, so links survive a restart / span pods) or a random
//! per-process key when unset (single-pod dev; links then die with the process).
//!
//! Stateless by design: no DB row per link. The token is verified purely from the
//! signature, so minting is free and redemption needs no lookup.

use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use catalerum_core::WorkspaceId;

type HmacSha256 = Hmac<Sha256>;

/// The claims a download token attests to — kept tiny (short serde keys) so the
/// token stays URL-friendly. `store` is the **resolved** store name (so redemption,
/// which has no acting user, targets the exact same backend the tool did); `dir`
/// marks a directory (prefix) link served as an archive; `exp` is a Unix-seconds
/// expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadClaims {
    /// The workspace the link reads from (SOUL §18) — redemption is scoped to it.
    #[serde(rename = "w")]
    pub workspace_id: WorkspaceId,
    /// The resolved store name, or `None` for the caller's default files store.
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// The object key (a single file) or key prefix (when `dir`).
    #[serde(rename = "k")]
    pub key: String,
    /// Whether `key` is a directory prefix served as a `.tar.gz` archive.
    #[serde(rename = "d", default, skip_serializing_if = "std::ops::Not::not")]
    pub dir: bool,
    /// Absolute expiry, Unix seconds. Redemption past this fails.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Why a token failed to verify. All map to a single `404` at the route so a
/// probe can't distinguish "bad signature" from "expired" from "malformed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    /// The token isn't `<payload>.<sig>` or a part isn't valid base64url/JSON.
    Malformed,
    /// The signature doesn't match this signer's key (forged / wrong key / tampered).
    BadSignature,
    /// The signature is valid but the link's `exp` has passed.
    Expired,
}

/// Mints and verifies [`DownloadClaims`] tokens with a process-wide HMAC key.
/// Cheap to clone (`Arc`-backed key); held by both the `download_link` tool (mint)
/// and [`AppState`](crate::state::AppState) (verify at the redeem route).
#[derive(Clone)]
pub struct DownloadSigner {
    key: Arc<[u8; 32]>,
}

impl DownloadSigner {
    /// A signer whose key is derived from a configured secret
    /// (`[server].download_secret`). Deriving via SHA-256 lets the operator use any
    /// human-chosen string while the HMAC always gets a full-width 32-byte key. A
    /// stable secret makes links survive restarts and be verifiable across pods.
    #[must_use]
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(Sha256::digest(secret.as_bytes()).into()),
        }
    }

    /// A signer with a fresh random key (no configured secret). Links it mints stop
    /// verifying once the process exits — fine for single-pod dev, where the
    /// convenience of zero-config beats cross-restart durability. Entropy comes from
    /// two v4 UUIDs (≈244 bits) folded through SHA-256, so no `rand` dependency is
    /// pulled in just for a key.
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
                    "no [server].download_secret set; using a random per-process key — \
                     download links won't survive a restart or span pods"
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

    /// HMAC tag over `msg` for a sibling signer that reuses this process key
    /// (e.g. [`EndpointSigner`](crate::mcp_endpoint_link::EndpointSigner)), so one
    /// operator secret covers download links *and* endpoint tokens.
    #[must_use]
    pub(crate) fn tag_for(&self, msg: &[u8]) -> [u8; 32] {
        self.tag(msg)
    }

    /// Constant-time verify of `sig` over `msg` under this key — the sibling-signer
    /// counterpart of [`tag_for`](Self::tag_for). `Err(())` on any mismatch.
    pub(crate) fn verify_tag(&self, msg: &[u8], sig: &[u8]) -> Result<(), ()> {
        let mut mac =
            HmacSha256::new_from_slice(self.key.as_slice()).expect("HMAC accepts a 32-byte key");
        mac.update(msg);
        mac.verify_slice(sig).map_err(|_| ())
    }

    /// Mint a `<payload>.<sig>` token for `claims`. `payload` is base64url(JSON);
    /// `sig` is base64url(HMAC(payload)). URL-safe, no padding — drops straight into
    /// a path segment.
    #[must_use]
    pub fn mint(&self, claims: &DownloadClaims) -> String {
        // `DownloadClaims` is always serializable (uuid + strings + ints), so this
        // never fails in practice.
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.tag(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Checks the signature in **constant
    /// time** (via `verify_slice`) before decoding, then rejects an expired link
    /// (`now` is Unix seconds). Any structural problem is [`VerifyError::Malformed`].
    pub fn verify(&self, token: &str, now: i64) -> Result<DownloadClaims, VerifyError> {
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
        let claims: DownloadClaims =
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

    fn claims() -> DownloadClaims {
        DownloadClaims {
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(1)),
            store: Some("archive".into()),
            key: "reports/q3.pdf".into(),
            dir: false,
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let signer = DownloadSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        // Valid before expiry.
        assert_eq!(signer.verify(&token, 999).unwrap(), claims());
    }

    #[test]
    fn verify_rejects_expired() {
        let signer = DownloadSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        // exp is 1000; at exactly 1000 (and after) it's dead.
        assert_eq!(signer.verify(&token, 1_000), Err(VerifyError::Expired));
        assert_eq!(signer.verify(&token, 5_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = DownloadSigner::from_secret("key-a");
        let b = DownloadSigner::from_secret("key-b");
        let token = a.mint(&claims());
        // A different signing key can't verify a's token (forgery guard).
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_payload() {
        let signer = DownloadSigner::from_secret("s3cret");
        let token = signer.mint(&claims());
        let (payload, sig) = token.split_once('.').unwrap();
        // Re-encode a payload that escalates to another workspace, keep the old sig.
        let mut forged = claims();
        forged.workspace_id = WorkspaceId::from_uuid(uuid::Uuid::from_u128(999));
        let bad_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_ne!(bad_payload, payload);
        let forged_token = format!("{bad_payload}.{sig}");
        assert_eq!(
            signer.verify(&forged_token, 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed() {
        let signer = DownloadSigner::from_secret("s3cret");
        // No `.` separator at all.
        assert_eq!(signer.verify("nodot", 0), Err(VerifyError::Malformed));
        // A signature that isn't valid base64url (decoded before the HMAC check).
        assert_eq!(signer.verify("payload.##", 0), Err(VerifyError::Malformed));
        // A *correctly signed* payload that isn't valid JSON: the signature verifies,
        // then decoding the claims fails → Malformed (not BadSignature).
        let payload = URL_SAFE_NO_PAD.encode(b"not json at all");
        let sig = URL_SAFE_NO_PAD.encode(signer.tag(payload.as_bytes()));
        assert_eq!(
            signer.verify(&format!("{payload}.{sig}"), 0),
            Err(VerifyError::Malformed)
        );
    }

    #[test]
    fn verify_checks_signature_before_payload() {
        // A garbage payload with a syntactically valid (but wrong) signature is
        // rejected as BadSignature — the payload is never trusted before the HMAC.
        let signer = DownloadSigner::from_secret("s3cret");
        assert_eq!(
            signer.verify("anything.AAAA", 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn random_signers_have_independent_keys() {
        let a = DownloadSigner::random();
        let b = DownloadSigner::random();
        let token = a.mint(&claims());
        // Two random signers can't verify each other's tokens.
        assert!(a.verify(&token, 0).is_ok());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn dir_flag_survives_roundtrip() {
        let signer = DownloadSigner::from_secret("s3cret");
        let mut c = claims();
        c.dir = true;
        c.key = "reports".into();
        c.store = None;
        let token = signer.mint(&c);
        assert_eq!(signer.verify(&token, 0).unwrap(), c);
    }
}
