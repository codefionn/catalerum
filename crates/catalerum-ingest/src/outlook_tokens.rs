//! Secret-store backed Microsoft OAuth token seam for the Outlook calendar
//! provider (SOUL §8/§13) — the [`google_tokens`](crate::google_tokens) twin.
//!
//! `catalerum-calendar` knows only the abstract
//! [`OutlookTokenStore`] seam; this module supplies the concrete implementation
//! backed by the AES-GCM [`SecretStore`], keyed by the connection's
//! `credential_ref`. It is the single place the ingest layer decrypts an
//! Outlook connection's OAuth blob — and re-seals the rotated one after every
//! token refresh, which matters more here than for Google: **Microsoft rotates
//! the refresh token on each grant**, so a lost rotation kills the connection.

use std::sync::Arc;

use async_trait::async_trait;

use catalerum_calendar::{CalendarSubKind, OutlookTokenStore, OutlookTokens};
use catalerum_core::error::{Error, Result};
use catalerum_core::id::WorkspaceId;
use catalerum_core::model::Connection;
use catalerum_store::SecretStore;

/// An [`OutlookTokenStore`] that seals/opens the OAuth blob behind one
/// connection's `credential_ref` in the workspace-scoped encrypted store.
pub struct SecretOutlookTokenStore {
    secrets: Arc<SecretStore>,
    workspace_id: WorkspaceId,
    credential_ref: String,
}

impl SecretOutlookTokenStore {
    /// Build the seam for `(workspace, credential_ref)`.
    #[must_use]
    pub fn new(
        secrets: Arc<SecretStore>,
        workspace_id: WorkspaceId,
        credential_ref: String,
    ) -> Self {
        Self {
            secrets,
            workspace_id,
            credential_ref,
        }
    }
}

#[async_trait]
impl OutlookTokenStore for SecretOutlookTokenStore {
    async fn load(&self) -> Result<OutlookTokens> {
        let bytes = self
            .secrets
            .get(self.workspace_id, &self.credential_ref)
            .await
            .map_err(|e| Error::provider(format!("load Microsoft OAuth credential: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::provider(format!("decode Microsoft OAuth credential: {e}")))
    }

    async fn store(&self, tokens: &OutlookTokens) -> Result<()> {
        let bytes = serde_json::to_vec(tokens)
            .map_err(|e| Error::provider(format!("encode Microsoft OAuth credential: {e}")))?;
        self.secrets
            .replace(self.workspace_id, &self.credential_ref, &bytes)
            .await
            .map_err(|e| {
                Error::provider(format!("persist rotated Microsoft OAuth credential: {e}"))
            })
    }
}

/// Build the Outlook token seam for a calendar connection — `Some` only when
/// the connection resolves to the `outlook` sub-kind, a secret store is
/// configured, and the row carries a `credential_ref`. `None` otherwise (a
/// non-Outlook backend ignores it; an Outlook connection then fails to build
/// with a clear error from the provider factory). The seam the ingest
/// sync/collect paths pass to
/// [`catalerum_calendar::provider_from_connection_with`], alongside
/// [`google_token_store_for`](crate::google_tokens::google_token_store_for).
#[must_use]
pub fn outlook_token_store_for(
    secrets: Option<&Arc<SecretStore>>,
    connection: &Connection,
    config: &serde_json::Value,
) -> Option<Arc<dyn OutlookTokenStore>> {
    let is_outlook = CalendarSubKind::from_config(config)
        .map(|k| k == CalendarSubKind::Outlook)
        .unwrap_or(false);
    if !is_outlook {
        return None;
    }
    let secrets = secrets?;
    let credential_ref = connection.credential_ref.clone()?;
    Some(Arc::new(SecretOutlookTokenStore::new(
        secrets.clone(),
        connection.workspace_id,
        credential_ref,
    )))
}
