//! Signed, shareable scoped tokens for Boa-scripted MCP endpoints (SOUL §26).
//!
//! A scoped token **is** its own authorization: an HMAC-SHA256 signature over a
//! tiny claim set naming exactly `{workspace, endpoint, expiry}`. The public serve
//! route (`POST /mcp/s/{token}`) re-verifies the signature + expiry, then serves
//! *only* that one endpoint's script-declared tools — nothing else in the
//! workspace. This is the "hand an external agent a read-only wiki endpoint"
//! path: a leaked token grants use of one endpoint for a short window, never the
//! workspace at large (the narrower sibling of a workspace bearer token, §19).
//!
//! Stateless by design (mirrors [`crate::download_link`]): no DB row per token; the
//! token verifies purely from the signature. The signing key is a process-wide
//! secret — configured (`[server].download_secret`, reused so links + endpoint
//! tokens share one operator secret and survive restarts / span pods) or a random
//! per-process key when unset.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use catalerum_core::WorkspaceId;
use serde::{Deserialize, Serialize};

use crate::download_link::DownloadSigner;
pub use crate::download_link::VerifyError;

/// The claims a scoped endpoint token attests to — kept tiny (short serde keys)
/// so the token stays URL-friendly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointClaims {
    /// The workspace the endpoint lives in (SOUL §18) — serving is scoped to it.
    #[serde(rename = "w")]
    pub workspace_id: WorkspaceId,
    /// The endpoint's (workspace-unique) name — the only endpoint this token serves.
    #[serde(rename = "n")]
    pub endpoint: String,
    /// Absolute expiry, Unix seconds. Serving past this fails.
    #[serde(rename = "e")]
    pub exp: i64,
}

/// Mints and verifies [`EndpointClaims`] tokens. Reuses the process-wide HMAC key
/// of the [`DownloadSigner`] (same operator secret, same forgery/tamper/expiry
/// guarantees), so an endpoint token and a download link are indistinguishable
/// crypto-wise — only their claim shape differs.
#[derive(Clone)]
pub struct EndpointSigner {
    inner: DownloadSigner,
}

impl EndpointSigner {
    /// Build over an existing [`DownloadSigner`] (shares its key).
    #[must_use]
    pub fn new(inner: DownloadSigner) -> Self {
        Self { inner }
    }

    /// Mint a `<payload>.<sig>` token for `claims` — URL-safe, drops into a path
    /// segment.
    #[must_use]
    pub fn mint(&self, claims: &EndpointClaims) -> String {
        let json = serde_json::to_vec(claims).unwrap_or_default();
        let payload = URL_SAFE_NO_PAD.encode(&json);
        let sig = URL_SAFE_NO_PAD.encode(self.inner.tag_for(payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token and return its claims. Constant-time signature check before
    /// decoding, then an expiry check (`now` is Unix seconds). Every structural
    /// problem is [`VerifyError::Malformed`]; a forged/tampered token is
    /// [`VerifyError::BadSignature`]; a stale one is [`VerifyError::Expired`].
    pub fn verify(&self, token: &str, now: i64) -> Result<EndpointClaims, VerifyError> {
        let (payload, sig_b64) = token.split_once('.').ok_or(VerifyError::Malformed)?;
        let sig = URL_SAFE_NO_PAD
            .decode(sig_b64)
            .map_err(|_| VerifyError::Malformed)?;
        self.inner
            .verify_tag(payload.as_bytes(), &sig)
            .map_err(|_| VerifyError::BadSignature)?;
        let json = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| VerifyError::Malformed)?;
        let claims: EndpointClaims =
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

    fn signer() -> EndpointSigner {
        EndpointSigner::new(DownloadSigner::from_secret("s3cret"))
    }

    fn claims() -> EndpointClaims {
        EndpointClaims {
            workspace_id: WorkspaceId::from_uuid(uuid::Uuid::from_u128(7)),
            endpoint: "acme-wiki".into(),
            exp: 1_000,
        }
    }

    #[test]
    fn mint_then_verify_roundtrips() {
        let s = signer();
        let token = s.mint(&claims());
        assert_eq!(s.verify(&token, 999).unwrap(), claims());
    }

    #[test]
    fn verify_rejects_expired() {
        let s = signer();
        let token = s.mint(&claims());
        assert_eq!(s.verify(&token, 1_000), Err(VerifyError::Expired));
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let a = EndpointSigner::new(DownloadSigner::from_secret("a"));
        let b = EndpointSigner::new(DownloadSigner::from_secret("b"));
        let token = a.mint(&claims());
        assert_eq!(b.verify(&token, 0), Err(VerifyError::BadSignature));
    }

    #[test]
    fn verify_rejects_tampered_endpoint() {
        // Re-encode a payload naming a different endpoint, keep the old sig.
        let s = signer();
        let token = s.mint(&claims());
        let (_, sig) = token.split_once('.').unwrap();
        let mut forged = claims();
        forged.endpoint = "secrets".into();
        let bad = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        assert_eq!(
            s.verify(&format!("{bad}.{sig}"), 0),
            Err(VerifyError::BadSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed() {
        let s = signer();
        assert_eq!(s.verify("nodot", 0), Err(VerifyError::Malformed));
    }
}
