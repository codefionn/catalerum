//! Secret-store backed Google OAuth token seam — shared by the Google **Calendar**
//! provider ([`GoogleTokenStore`]) and the **Gmail** email provider
//! ([`catalerum_email::GmailTokenStore`]) (SOUL §8/§13/§16 M7/§28).
//!
//! The provider crates know only their abstract token seam
//! (`catalerum_calendar` ⇒ [`GoogleTokenStore`]; `catalerum_email` ⇒
//! [`GmailTokenStore`](catalerum_email::GmailTokenStore)); this module supplies the
//! concrete implementation backed by the AES-GCM [`SecretStore`], keyed by the
//! connection's `credential_ref`. It is the single place the ingest layer decrypts
//! a Google connection's OAuth blob (and re-seals a rotated one after a token
//! refresh) — one encrypted entry serves both kinds because the two seams seal the
//! **identical** JSON blob shape (`{client_id, client_secret, access_token,
//! refresh_token, expiry}`), so `SecretGoogleTokenStore` implements both traits over
//! the same bytes.

use std::sync::Arc;

use async_trait::async_trait;

use catalerum_calendar::{CalendarSubKind, GoogleTokenStore, GoogleTokens};
use catalerum_core::error::{Error, Result};
use catalerum_core::id::{ConnectionId, WorkspaceId};
use catalerum_core::model::Connection;
use catalerum_email::{
    reseal_gmail_plaintext, EmailSubKind, GmailProvider, GmailResealer, GmailTokenStore,
    GmailTokens, GMAIL_PLAINTEXT_KEYS,
};
use catalerum_store::{ConnectionRepo, SecretStore};

/// A [`GoogleTokenStore`] that seals/opens the OAuth blob behind one connection's
/// `credential_ref` in the workspace-scoped encrypted secret store.
pub struct SecretGoogleTokenStore {
    secrets: Arc<SecretStore>,
    workspace_id: WorkspaceId,
    credential_ref: String,
}

impl SecretGoogleTokenStore {
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
impl GoogleTokenStore for SecretGoogleTokenStore {
    async fn load(&self) -> Result<GoogleTokens> {
        let bytes = self
            .secrets
            .get(self.workspace_id, &self.credential_ref)
            .await
            .map_err(|e| Error::provider(format!("load Google OAuth credential: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::provider(format!("decode Google OAuth credential: {e}")))
    }

    async fn store(&self, tokens: &GoogleTokens) -> Result<()> {
        let bytes = serde_json::to_vec(tokens)
            .map_err(|e| Error::provider(format!("encode Google OAuth credential: {e}")))?;
        self.secrets
            .replace(self.workspace_id, &self.credential_ref, &bytes)
            .await
            .map_err(|e| Error::provider(format!("persist rotated Google OAuth credential: {e}")))
    }
}

/// The **same** encrypted entry, opened as a Gmail token blob. The calendar
/// [`GoogleTokens`] and email [`GmailTokens`] structs are byte-identical JSON, so a
/// connection sealed by either `/auth/google/connect` kind is readable here — the
/// unified Google token store (SOUL §13/§28).
#[async_trait]
impl GmailTokenStore for SecretGoogleTokenStore {
    async fn load(&self) -> Result<GmailTokens> {
        let bytes = self
            .secrets
            .get(self.workspace_id, &self.credential_ref)
            .await
            .map_err(|e| Error::provider(format!("load Gmail OAuth credential: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::provider(format!("decode Gmail OAuth credential: {e}")))
    }

    async fn store(&self, tokens: &GmailTokens) -> Result<()> {
        let bytes = serde_json::to_vec(tokens)
            .map_err(|e| Error::provider(format!("encode Gmail OAuth credential: {e}")))?;
        self.secrets
            .replace(self.workspace_id, &self.credential_ref, &bytes)
            .await
            .map_err(|e| Error::provider(format!("persist rotated Gmail OAuth credential: {e}")))
    }
}

/// The ingest-side [`GmailResealer`]: seals into the AES-GCM secret store and drives
/// the `credential_ref` / `config` mutations on the connections repo, scoped to one
/// connection (SOUL §13/§28). Store failures are mapped to the provider error the
/// seam speaks (mirroring [`SecretGoogleTokenStore`]'s error mapping).
struct IngestGmailResealer {
    secrets: Arc<SecretStore>,
    connections: ConnectionRepo,
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
}

#[async_trait]
impl GmailResealer for IngestGmailResealer {
    async fn seal(&self, tokens: &GmailTokens) -> Result<String> {
        let bytes = serde_json::to_vec(tokens).map_err(|e| {
            Error::provider(format!("encode Gmail OAuth credential for reseal: {e}"))
        })?;
        self.secrets
            .put(self.workspace_id, &bytes)
            .await
            .map_err(|e| Error::provider(format!("seal Gmail OAuth credential: {e}")))
    }

    async fn set_credential_ref(&self, credential_ref: &str) -> Result<()> {
        self.connections
            .set_credential_ref(self.workspace_id, self.connection_id, Some(credential_ref))
            .await
            .map(|_| ())
            .map_err(|e| Error::provider(format!("point connection at sealed credential: {e}")))
    }

    async fn scrub_config(&self, keys: &[&str]) -> Result<()> {
        self.connections
            .scrub_config_keys(self.workspace_id, self.connection_id, keys)
            .await
            .map(|_| ())
            .map_err(|e| Error::provider(format!("scrub plaintext OAuth keys from config: {e}")))
    }
}

/// Opportunistically reseal a legacy **plaintext** Gmail connection onto the encrypted
/// store, or heal a half-resealed one (SOUL §13/§28). Called by the email collect path
/// for every Gmail connection; **best-effort** — a failure logs and leaves the
/// connection intact (retried next sync), never failing the collect.
///
/// A no-op unless `[secrets].master_key` is configured (`secrets` present): with no
/// key, today's warn-only plaintext path is unchanged. Two cases when it is present:
/// - **`credential_ref` absent (plaintext):** prove the plaintext creds via a refresh
///   grant, then seal → `set_credential_ref` → scrub (see [`reseal_gmail_plaintext`]).
///   Logs one info-level `resealed connection …`. Idempotent: once sealed the
///   connection carries a `credential_ref`, so this branch can't re-trigger.
/// - **`credential_ref` present but plaintext keys survive in `config`:** the
///   residual-scrub healing a crash between `set_credential_ref` and scrub — strip the
///   stray plaintext keys (the sealed path ignores them anyway). Logs one info-level
///   `scrubbed residual plaintext …`.
pub async fn reseal_plaintext_gmail_if_applicable(
    connections: &ConnectionRepo,
    secrets: Option<&Arc<SecretStore>>,
    connection: &Connection,
    config: &serde_json::Value,
) {
    let Some(secrets) = secrets else {
        return; // no master key ⇒ warn-only plaintext path (unchanged)
    };
    let is_gmail = EmailSubKind::from_config(config)
        .map(|k| k == EmailSubKind::Gmail)
        .unwrap_or(false);
    if !is_gmail {
        return;
    }
    let resealer = IngestGmailResealer {
        secrets: secrets.clone(),
        connections: connections.clone(),
        workspace_id: connection.workspace_id,
        connection_id: connection.id,
    };

    if connection.credential_ref.is_none() {
        // Plaintext path: prove the creds work, then seal → ref → scrub.
        let Ok(provider) =
            GmailProvider::from_config(connection.workspace_id, connection.id, config)
        else {
            // A Gmail connection lacking the triplet is (mis)configured — nothing to
            // reseal; the sync itself surfaces the config error.
            return;
        };
        match provider.plaintext_reseal_blob().await {
            Ok(Some(blob)) => match reseal_gmail_plaintext(&blob, &resealer).await {
                Ok(_) => tracing::info!(
                    workspace = %connection.workspace_id,
                    connection = %connection.id,
                    "resealed connection {} onto the encrypted Google token store",
                    connection.name,
                ),
                Err(e) => tracing::warn!(
                    workspace = %connection.workspace_id, connection = %connection.id, error = %e,
                    "opportunistic Gmail reseal failed; leaving the plaintext connection intact (retried next sync)",
                ),
            },
            Ok(None) => {} // already sealed — nothing to do (idempotent)
            Err(e) => tracing::warn!(
                workspace = %connection.workspace_id, connection = %connection.id, error = %e,
                "could not prove plaintext Gmail credentials for reseal; leaving the connection intact",
            ),
        }
    } else if GMAIL_PLAINTEXT_KEYS
        .iter()
        .any(|k| config.get(*k).is_some())
    {
        // Sealed already, but a crash between set_credential_ref and scrub left stray
        // plaintext keys behind. The sealed path ignores them; strip them so no
        // plaintext credential lingers at rest.
        match resealer.scrub_config(GMAIL_PLAINTEXT_KEYS).await {
            Ok(()) => tracing::info!(
                workspace = %connection.workspace_id, connection = %connection.id,
                "scrubbed residual plaintext OAuth keys from sealed connection {}",
                connection.name,
            ),
            Err(e) => tracing::warn!(
                workspace = %connection.workspace_id, connection = %connection.id, error = %e,
                "failed to scrub residual plaintext OAuth keys (retried next sync)",
            ),
        }
    }
}

/// Build the Google token seam for a calendar connection — `Some` only when the
/// connection resolves to the `google` sub-kind, a secret store is configured, and
/// the row carries a `credential_ref`. Returns `None` otherwise (a non-Google
/// backend ignores it; a Google connection then fails to build with a clear error
/// from the provider factory). This is the seam the ingest sync/collect paths pass
/// to [`catalerum_calendar::provider_from_connection_with`].
#[must_use]
pub fn google_token_store_for(
    secrets: Option<&Arc<SecretStore>>,
    connection: &Connection,
    config: &serde_json::Value,
) -> Option<Arc<dyn GoogleTokenStore>> {
    let is_google = CalendarSubKind::from_config(config)
        .map(|k| k == CalendarSubKind::Google)
        .unwrap_or(false);
    if !is_google {
        return None;
    }
    let secrets = secrets?;
    let credential_ref = connection.credential_ref.clone()?;
    Some(Arc::new(SecretGoogleTokenStore::new(
        secrets.clone(),
        connection.workspace_id,
        credential_ref,
    )))
}

/// Build the Gmail token seam for an email connection — `Some` only when the
/// connection resolves to the `gmail` sub-kind, a secret store is configured, and
/// the row carries a `credential_ref`. Returns `None` otherwise (a non-Gmail
/// backend ignores it; a Gmail connection with a `credential_ref` but `None` seam
/// then fails to build with a clear error from the provider factory, and a
/// `credential_ref`-less Gmail connection falls back to the legacy plaintext path).
/// This is the Gmail counterpart of [`google_token_store_for`] — the seam the ingest
/// collect path passes to [`catalerum_email::provider_from_connection_with`].
#[must_use]
pub fn gmail_token_store_for(
    secrets: Option<&Arc<SecretStore>>,
    connection: &Connection,
    config: &serde_json::Value,
) -> Option<Arc<dyn GmailTokenStore>> {
    let is_gmail = EmailSubKind::from_config(config)
        .map(|k| k == EmailSubKind::Gmail)
        .unwrap_or(false);
    if !is_gmail {
        return None;
    }
    let secrets = secrets?;
    let credential_ref = connection.credential_ref.clone()?;
    Some(Arc::new(SecretGoogleTokenStore::new(
        secrets.clone(),
        connection.workspace_id,
        credential_ref,
    )))
}
