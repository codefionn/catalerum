//! Gmail API email provider over HTTP (SOUL §28).
//!
//! A read-only ingest backend ([`EmailProvider`]) for a Gmail account. catalerum
//! **reads** mail; it never sends or mutates state (§14), so sync only ever
//! issues `users.messages.list/get`, `users.history.list`, and
//! `users.getProfile`.
//!
//! ## Auth — two paths (sealed OAuth blob, or legacy plaintext grant)
//! The provider mints a short-lived access token from Google's token endpoint via
//! the refresh-token grant. *Where* the OAuth material comes from is the security
//! seam:
//!
//! - **Sealed** (the unified path, SOUL §13): the OAuth blob
//!   `{client_id, client_secret, access_token, refresh_token, expiry}` lives
//!   **AES-GCM-encrypted** behind the connection's `credential_ref`, reached through
//!   the [`GmailTokenStore`] seam (the ingest layer supplies a secret-store-backed
//!   impl, so this crate needs no secret-store / DB dependency). The token is
//!   cached and transparently **refreshed** under a single-flight lock when it
//!   expires, and the rotated blob is persisted back through the seam — byte-for-
//!   byte the Google **Calendar** provider's discipline, over the same encrypted
//!   store the `/auth/google/connect?kind=email` web flow seals.
//! - **Legacy plaintext** (back-compat, deprecated): a connection whose `config`
//!   still carries a `client_id`/`client_secret`/`refresh_token` triplet keeps
//!   syncing — each sync exchanges them for an access token — so existing
//!   deployments never break. The factory logs a one-per-sync `warn` pointing at
//!   `/auth/google/connect?kind=email` to re-seal them. Prefer the sealed path.
//!
//! Which path a connection takes is decided by
//! [`provider_from_connection_with`](crate::provider_from_connection_with):
//! `credential_ref` present ⇒ sealed; absent ⇒ legacy plaintext.
//!
//! ## Incrementality (SOUL §3.4)
//! The connection syncs one Gmail **label** (default `INBOX`) as a mailbox. The
//! [`Cursor`] is the account `historyId`:
//! - First sync (no cursor): `messages.list` the label, `messages.get?format=RAW`
//!   each → full snapshot upsert; cursor = the latest `historyId`.
//! - Incremental: `history.list?startHistoryId=…` → messages added / deleted /
//!   re-labelled. Membership of and flag changes within the synced label become
//!   upserts; messages deleted or moved out of the label become `deletions`.
//! - If Gmail reports the `historyId` as **expired** (`404`), sync falls back to a
//!   full snapshot.
//!
//! Being a true delta ([`is_incremental`](EmailProvider::is_incremental) is
//! `true`), the ingest worker treats this provider as authoritative for deletions
//! and never diff-reconciles.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Cursor, Email, Mailbox};
use catalerum_core::provider::{EmailProvider, SyncBatch};
use catalerum_core::{ConnectionId, WorkspaceId};

use crate::{parse_email, stable_mailbox_id};

/// Google's OAuth2 token endpoint (refresh-token grant).
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Base for the Gmail REST API, scoped to the authenticated user.
const API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// The read-only Gmail scope requested by the `/auth/google/connect?kind=email`
/// OAuth flow (the sealed-credential path). Exposed so the API's connect route and
/// this provider agree on one constant (mirrors the calendar crate's
/// `CALENDAR_READONLY_SCOPE`).
pub const GMAIL_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";

/// `messages.list` page size + a safety cap on a full snapshot.
const PAGE: usize = 500;
const SNAPSHOT_CAP: usize = 10_000;

/// Clock-skew leeway (seconds): refresh the sealed access token this long *before*
/// its nominal expiry so an in-flight request never races the expiry (mirrors the
/// calendar provider).
const EXPIRY_SKEW_SECS: i64 = 60;

/// The OAuth material for one Gmail connection — sealed (AES-GCM) behind the
/// connection's `credential_ref` and reached through [`GmailTokenStore`].
///
/// Byte-for-byte the same JSON shape the Google **Calendar** provider seals
/// (`catalerum_calendar::GoogleTokens`), so a single `/auth/google/connect` flow
/// and one encrypted secret entry serve either kind — this is the *unified* Google
/// token store. Kept a local mirror (rather than a dependency on the calendar
/// crate) so `catalerum-email` stays free of a calendar dependency.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GmailTokens {
    /// OAuth client id of the catalerum Google app (from `[google]`).
    pub client_id: String,
    /// OAuth client secret of the catalerum Google app (from `[google]`).
    pub client_secret: String,
    /// The current short-lived access token, if one has been minted. Absent forces
    /// a refresh on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// The long-lived refresh token (offline access). Required.
    pub refresh_token: String,
    /// When [`access_token`](Self::access_token) expires (absent ⇒ treat as already
    /// expired, forcing a refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<DateTime<Utc>>,
}

/// The persistence seam for a Gmail connection's [`GmailTokens`] (SOUL §13).
///
/// Kept a trait so `catalerum-email` needs no secret-store / DB dependency: the
/// ingest layer supplies an implementation backed by the AES-GCM secret store
/// (keyed by the connection's `credential_ref` — the same one the calendar
/// `GoogleTokenStore` opens). The provider calls [`load`](Self::load) once per sync
/// run and [`store`](Self::store) only when a refresh rotates the blob.
#[async_trait]
pub trait GmailTokenStore: Send + Sync {
    /// Decrypt and return the connection's current OAuth material.
    async fn load(&self) -> Result<GmailTokens>;
    /// Re-seal the (rotated) OAuth material in place.
    async fn store(&self, tokens: &GmailTokens) -> Result<()>;
}

/// The plaintext OAuth `config` keys opportunistic resealing strips once a legacy
/// Gmail connection is sealed onto the encrypted store — exactly the triplet
/// [`GmailProvider::from_config`] reads. Exposed so the ingest layer scrubs the same
/// keys the provider trusts (a mismatch would either leave a plaintext secret behind
/// or delete a key the plaintext path still needs). `access_token`/`expiry` are never
/// in `config` (they're minted at runtime), so they aren't scrubbed.
pub const GMAIL_PLAINTEXT_KEYS: &[&str] = &["client_id", "client_secret", "refresh_token"];

/// The persistence seam for opportunistically **resealing** a legacy plaintext Gmail
/// connection onto the encrypted store (SOUL §13/§28). The ingest layer implements it
/// over the AES-GCM secret store + connections repo; kept a trait so `catalerum-email`
/// needs no secret-store / DB dependency (mirroring [`GmailTokenStore`]).
///
/// [`reseal_gmail_plaintext`] drives the three methods in the fixed, crash-safe order
/// **`seal` → `set_credential_ref` → `scrub_config`** — see that function for the
/// crash-window analysis.
#[async_trait]
pub trait GmailResealer: Send + Sync {
    /// Seal the proven OAuth blob into the secret store, returning its opaque
    /// `credential_ref`.
    async fn seal(&self, tokens: &GmailTokens) -> Result<String>;
    /// Point the connection at the sealed credential (set its `credential_ref`).
    async fn set_credential_ref(&self, credential_ref: &str) -> Result<()>;
    /// Remove the named plaintext keys from the connection's `config`.
    async fn scrub_config(&self, keys: &[&str]) -> Result<()>;
}

/// How a [`GmailProvider`] obtains OAuth material — the security seam.
enum GmailAuth {
    /// **Legacy plaintext** (back-compat): the refresh-grant triplet read straight
    /// from the connection `config`. Each sync exchanges it for an access token; no
    /// caching (the grant is cheap and this path is deprecated).
    Plaintext {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
    /// **Sealed**: the OAuth blob lives encrypted behind `credential_ref`, reached
    /// through the token seam. The decrypted blob is cached for the run and
    /// transparently refreshed under the lock (single-flight), with the rotated
    /// blob persisted back before the lock releases.
    Sealed {
        tokens: Arc<dyn GmailTokenStore>,
        cache: tokio::sync::Mutex<Option<GmailTokens>>,
    },
}

/// A read-only Gmail [`EmailProvider`] for one label (SOUL §28).
pub struct GmailProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    /// The Gmail label id this connection ingests (also the mailbox external id).
    label: String,
    http: reqwest::Client,
    auth: GmailAuth,
}

impl std::fmt::Debug for GmailProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailProvider")
            .field("workspace_id", &self.workspace_id)
            .field("connection_id", &self.connection_id)
            .field("label", &self.label)
            .field(
                "auth",
                &match self.auth {
                    GmailAuth::Plaintext { .. } => "plaintext",
                    GmailAuth::Sealed { .. } => "sealed",
                },
            )
            .finish_non_exhaustive()
    }
}

impl GmailProvider {
    /// Build a Gmail HTTP client (shared by both auth paths).
    fn http_client() -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .user_agent("catalerum-email")
            // Fail fast on an unreachable Gmail endpoint instead of hanging the
            // sync worker. No overall timeout: a message fetch can be large and
            // shouldn't be aborted mid-transfer (the API paginates).
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::provider(format!("build http client: {e}")))
    }

    /// The Gmail label id from `config` (`label`, alias `mailbox`; default `INBOX`).
    fn label_from_config(config: &Value) -> String {
        opt_str(config, "label")
            .or_else(|| opt_str(config, "mailbox"))
            .unwrap_or_else(|| "INBOX".to_string())
    }

    /// Build from a connection's **plaintext** `config` JSON (the legacy path).
    /// Required: `client_id`, `client_secret`, `refresh_token`. Optional: `label`
    /// (default `INBOX`). Prefer [`from_sealed`](Self::from_sealed).
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
    ) -> Result<Self> {
        let client_id = req_str(config, "client_id")?;
        let client_secret = req_str(config, "client_secret")?;
        let refresh_token = req_str(config, "refresh_token")?;
        Ok(Self {
            workspace_id,
            connection_id,
            label: Self::label_from_config(config),
            http: Self::http_client()?,
            auth: GmailAuth::Plaintext {
                client_id,
                client_secret,
                refresh_token,
            },
        })
    }

    /// Build the **sealed** provider: the OAuth material comes from the encrypted
    /// [`GmailTokenStore`] (behind the connection's `credential_ref`), never from
    /// `config`. The only config key read is the optional `label` (default `INBOX`).
    pub fn from_sealed(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
        tokens: Arc<dyn GmailTokenStore>,
    ) -> Result<Self> {
        Ok(Self {
            workspace_id,
            connection_id,
            label: Self::label_from_config(config),
            http: Self::http_client()?,
            auth: GmailAuth::Sealed {
                tokens,
                cache: tokio::sync::Mutex::new(None),
            },
        })
    }

    fn mailbox(&self) -> Mailbox {
        Mailbox {
            id: stable_mailbox_id(self.connection_id, &self.label),
            workspace_id: self.workspace_id,
            connection_id: self.connection_id,
            external_id: self.label.clone(),
            name: self.label.clone(),
            read_only: true,
        }
    }

    /// A valid access token for the current run. Dispatches on the auth path:
    /// - **Plaintext**: exchange the config triplet for a fresh access token every
    ///   call (the legacy grant; cheap and stateless).
    /// - **Sealed**: return the cached token while valid, else mint a fresh one via
    ///   the refresh-token grant — single-flight (the cache lock is held across the
    ///   refresh so two concurrent syncs never both refresh) — and persist the
    ///   rotated blob before releasing the lock.
    async fn access_token(&self) -> Result<String> {
        match &self.auth {
            GmailAuth::Plaintext {
                client_id,
                client_secret,
                refresh_token,
            } => Ok(
                refresh_grant(&self.http, client_id, client_secret, refresh_token)
                    .await?
                    .access_token,
            ),
            GmailAuth::Sealed { tokens, cache } => {
                let mut guard = cache.lock().await;
                if guard.is_none() {
                    *guard = Some(tokens.load().await?);
                }
                let blob = guard.as_mut().expect("cache populated above");
                let now = Utc::now();
                if !needs_refresh(blob, now, EXPIRY_SKEW_SECS) {
                    return Ok(blob
                        .access_token
                        .clone()
                        .expect("needs_refresh false implies a token"));
                }
                let resp = refresh_grant(
                    &self.http,
                    &blob.client_id,
                    &blob.client_secret,
                    &blob.refresh_token,
                )
                .await?;
                apply_refresh(blob, &resp, now);
                // Persist the rotated blob (new access token/expiry, possibly a
                // rotated refresh token) so the next run starts from it.
                tokens.store(blob).await?;
                Ok(blob
                    .access_token
                    .clone()
                    .expect("apply_refresh sets the access token"))
            }
        }
    }

    /// Prove a **legacy plaintext** connection's credentials by exchanging them for a
    /// fresh access token, and fold the result into a sealable [`GmailTokens`] blob
    /// (`{client_id, client_secret, access_token, refresh_token, expiry}`) — the input
    /// [`reseal_gmail_plaintext`] seals (SOUL §13/§28).
    ///
    /// Returns `Ok(None)` when this provider is already on the **sealed** path: there
    /// is nothing to reseal. This is the idempotence guarantee — a resealed connection
    /// carries a `credential_ref`, so a rebuilt provider takes the sealed branch and
    /// this returns `None`, which is why the reseal path cannot re-trigger once it has
    /// run. A failed grant is an `Err` (broken creds ⇒ do **not** reseal; the caller
    /// leaves the connection on the plaintext warn path and retries next sync).
    pub async fn plaintext_reseal_blob(&self) -> Result<Option<GmailTokens>> {
        let GmailAuth::Plaintext {
            client_id,
            client_secret,
            refresh_token,
        } = &self.auth
        else {
            return Ok(None);
        };
        // A successful grant proves the plaintext creds actually work *before* we
        // commit them to the vault — we never seal credentials we couldn't redeem.
        let resp = refresh_grant(&self.http, client_id, client_secret, refresh_token).await?;
        let mut blob = GmailTokens {
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            access_token: None,
            refresh_token: refresh_token.clone(),
            expiry: None,
        };
        apply_refresh(&mut blob, &resp, Utc::now());
        Ok(Some(blob))
    }

    /// All message ids carrying the synced label, paged to [`SNAPSHOT_CAP`].
    async fn list_message_ids(&self, token: &str) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("labelIds", self.label.clone()),
                ("maxResults", PAGE.to_string()),
            ];
            if let Some(pt) = &page_token {
                query.push(("pageToken", pt.clone()));
            }
            let v = self
                .get_json(&format!("{API_BASE}/messages"), token, &query)
                .await?;
            if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
                for m in msgs {
                    if let Some(id) = m.get("id").and_then(Value::as_str) {
                        ids.push(id.to_string());
                    }
                }
            }
            page_token = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() || ids.len() >= SNAPSHOT_CAP {
                if ids.len() >= SNAPSHOT_CAP {
                    // Don't truncate silently: surface that older mail beyond the
                    // cap is not synced this round (mirrors the JMAP provider).
                    tracing::warn!(
                        cap = SNAPSHOT_CAP,
                        label = %self.label,
                        "Gmail snapshot hit cap; messages beyond it are not synced this round"
                    );
                }
                break;
            }
        }
        Ok(ids)
    }

    /// `users.getProfile` → the current account `historyId` (cursor seed when a
    /// snapshot has no messages to read one from).
    async fn profile_history_id(&self, token: &str) -> Result<u64> {
        let v = self
            .get_json(&format!("{API_BASE}/profile"), token, &[])
            .await?;
        Ok(as_u64(v.get("historyId")).unwrap_or(0))
    }

    /// Fetch one message as RAW and map it to a canonical [`Email`] — but only if
    /// it still carries the synced label. Returns `(email, historyId)`; `email`
    /// is `None` when the message is gone (`404`) or no longer in this mailbox.
    async fn get_message(
        &self,
        token: &str,
        mailbox: &Mailbox,
        id: &str,
    ) -> Result<(Option<Email>, u64)> {
        let url = format!("{API_BASE}/messages/{id}");
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .query(&[("format", "RAW")])
            .send()
            .await
            .map_err(|e| Error::provider(format!("Gmail messages.get: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok((None, 0));
        }
        let resp = ensure_success(resp, "messages.get")?;
        let v: Value = crate::read_json_capped(resp, "Gmail message").await?;
        let label_ids = string_list(v.get("labelIds"));
        let hist = as_u64(v.get("historyId")).unwrap_or(0);
        if !label_ids.iter().any(|l| l == &self.label) {
            return Ok((None, hist)); // moved out of the synced label
        }
        let raw_b64 = v
            .get("raw")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::provider("Gmail message has no raw payload"))?;
        let flags = gmail_flags(&label_ids);
        // A *message-specific* failure (undecodable payload, or an unparseable /
        // multipart-bomb message) must not fail the whole batch — skip it (logged)
        // and advance the cursor, mirroring the IMAP path. Network/HTTP errors above
        // still propagate via `?` so a transient blip retries the batch intact.
        let raw = match decode_raw(raw_b64) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(id, error = %e, "skipping Gmail message with undecodable raw payload");
                return Ok((None, hist));
            }
        };
        match parse_email(
            id.to_string(),
            flags,
            &raw,
            mailbox.workspace_id,
            mailbox.id,
        ) {
            Ok(email) => Ok((Some(email), hist)),
            Err(e) => {
                tracing::warn!(id, error = %e, "skipping unparseable Gmail message");
                Ok((None, hist))
            }
        }
    }

    /// First-time snapshot: list + fetch every message in the label.
    async fn full_sync(&self, token: &str, mailbox: &Mailbox) -> Result<SyncBatch<Email>> {
        let ids = self.list_message_ids(token).await?;
        let mut upserts = Vec::new();
        let mut latest = 0u64;
        for id in &ids {
            let (email, hist) = self.get_message(token, mailbox, id).await?;
            latest = latest.max(hist);
            if let Some(e) = email {
                upserts.push(e);
            }
        }
        let next = if latest > 0 {
            latest
        } else {
            self.profile_history_id(token).await?
        };
        Ok(SyncBatch {
            upserts,
            deletions: Vec::new(),
            next_cursor: GmailCursor { h: next }.encode(),
            has_more: false,
        })
    }

    /// History-based delta from `prior` (the stored `historyId`). Returns
    /// `Ok(None)` when Gmail reports the id expired (`404`), signalling the caller
    /// to fall back to [`full_sync`](Self::full_sync).
    async fn incremental(
        &self,
        token: &str,
        mailbox: &Mailbox,
        prior: u64,
    ) -> Result<Option<SyncBatch<Email>>> {
        let mut touched: HashSet<String> = HashSet::new();
        let mut deleted: HashSet<String> = HashSet::new();
        let mut new_hist = prior;
        let mut page_token: Option<String> = None;

        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("startHistoryId", prior.to_string()),
                ("labelId", self.label.clone()),
                ("historyTypes", "messageAdded".to_string()),
                ("historyTypes", "messageDeleted".to_string()),
                ("historyTypes", "labelAdded".to_string()),
                ("historyTypes", "labelRemoved".to_string()),
            ];
            if let Some(pt) = &page_token {
                query.push(("pageToken", pt.clone()));
            }
            let resp = self
                .http
                .get(format!("{API_BASE}/history"))
                .bearer_auth(token)
                .query(&query)
                .send()
                .await
                .map_err(|e| Error::provider(format!("Gmail history.list: {e}")))?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(None); // historyId expired → caller does a full resync
            }
            let resp = ensure_success(resp, "history.list")?;
            let v: Value = crate::read_json_capped(resp, "Gmail history").await?;
            if let Some(h) = as_u64(v.get("historyId")) {
                new_hist = new_hist.max(h);
            }
            for record in v
                .get("history")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                classify_history(record, &self.label, &mut touched, &mut deleted);
            }
            page_token = v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if page_token.is_none() {
                break;
            }
        }

        let mut upserts = Vec::new();
        for id in touched.difference(&deleted) {
            let (email, _) = self.get_message(token, mailbox, id).await?;
            if let Some(e) = email {
                upserts.push(e);
            }
        }
        Ok(Some(SyncBatch {
            upserts,
            deletions: deleted.into_iter().collect(),
            next_cursor: GmailCursor { h: new_hist }.encode(),
            has_more: false,
        }))
    }

    /// GET a Gmail JSON endpoint with bearer auth + query params.
    async fn get_json(&self, url: &str, token: &str, query: &[(&str, String)]) -> Result<Value> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .query(query)
            .send()
            .await
            .map_err(|e| Error::provider(format!("Gmail GET {url}: {e}")))?;
        let resp = ensure_success(resp, "GET")?;
        crate::read_json_capped(resp, "Gmail GET").await
    }
}

#[async_trait]
impl EmailProvider for GmailProvider {
    async fn list_mailboxes(&self) -> Result<Vec<Mailbox>> {
        Ok(vec![self.mailbox()])
    }

    async fn sync(&self, mailbox: &Mailbox, cursor: Option<Cursor>) -> Result<SyncBatch<Email>> {
        let token = self.access_token().await?;
        match GmailCursor::decode(cursor.as_ref()) {
            Some(prior) if prior.h > 0 => match self.incremental(&token, mailbox, prior.h).await? {
                Some(batch) => Ok(batch),
                None => self.full_sync(&token, mailbox).await,
            },
            _ => self.full_sync(&token, mailbox).await,
        }
    }

    fn is_incremental(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Token refresh — pure decision/mutation helpers (shared by both auth paths;
// fake-clock testable, mirroring the calendar provider)
// ---------------------------------------------------------------------------

/// The subset of Google's token response the refresh grant yields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TokenResponse {
    access_token: String,
    /// Seconds until the access token expires (`expires_in`).
    expires_in: i64,
    /// A rotated refresh token, when Google returns one (usually it doesn't).
    refresh_token: Option<String>,
}

/// Whether the cached access token is absent or within `skew_secs` of expiry —
/// i.e. a refresh is due. Pure (fake-clock testable).
fn needs_refresh(tokens: &GmailTokens, now: DateTime<Utc>, skew_secs: i64) -> bool {
    match (&tokens.access_token, tokens.expiry) {
        (Some(_), Some(exp)) => exp <= now + chrono::Duration::seconds(skew_secs),
        _ => true, // no token, or no known expiry ⇒ refresh
    }
}

/// Fold a refresh response into the blob: new access token + expiry, and a rotated
/// refresh token when present. Pure (testable).
fn apply_refresh(tokens: &mut GmailTokens, resp: &TokenResponse, now: DateTime<Utc>) {
    tokens.access_token = Some(resp.access_token.clone());
    tokens.expiry = Some(now + chrono::Duration::seconds(resp.expires_in));
    if let Some(rt) = resp.refresh_token.as_ref().filter(|s| !s.is_empty()) {
        tokens.refresh_token = rt.clone();
    }
}

/// Parse Google's token endpoint JSON into a [`TokenResponse`]. Pure (testable).
fn parse_token_response(v: &Value) -> Result<TokenResponse> {
    let access_token = v
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::provider("Gmail token response has no access_token"))?;
    let expires_in = v.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
    let refresh_token = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(TokenResponse {
        access_token,
        expires_in,
        refresh_token,
    })
}

/// Exchange a refresh token for a fresh access token at Google's token endpoint.
async fn refresh_grant(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| Error::provider(format!("Gmail token request: {e}")))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::BAD_REQUEST
    {
        return Err(Error::unauthorized("Gmail refresh-token grant rejected"));
    }
    let resp = ensure_success(resp, "token")?;
    let v: Value = crate::read_json_capped(resp, "Gmail token").await?;
    parse_token_response(&v)
}

/// Reseal a legacy plaintext Gmail connection onto the encrypted store (SOUL
/// §13/§28), given a proven OAuth `blob` (from
/// [`GmailProvider::plaintext_reseal_blob`]) and a [`GmailResealer`] over the secret
/// store + connections repo. Runs the three steps in a fixed order and returns the
/// new `credential_ref`:
///
/// 1. **`seal`** — encrypt `{client_id, client_secret, access_token, refresh_token,
///    expiry}` into the vault, minting a fresh `credential_ref`.
/// 2. **`set_credential_ref`** — point the connection at that ref.
/// 3. **`scrub_config`** — strip the now-redundant plaintext keys
///    ([`GMAIL_PLAINTEXT_KEYS`]) from `config`.
///
/// ## Crash-window analysis (why this order)
/// Every partial prefix leaves a **working** connection — a working connection is
/// prioritized over a perfectly-scrubbed one:
/// - **After `seal`, before `set_credential_ref`:** a vault entry exists but nothing
///   references it (an orphan). The connection still has `credential_ref = NULL` +
///   the plaintext triplet, so the next sync re-enters the plaintext path and retries
///   the whole reseal — minting *another* orphan but ultimately succeeding. Orphans
///   are unreferenced encrypted blobs (no plaintext leak, just dead rows); they are
///   not garbage-collected here (a disclosed deferral).
/// - **After `set_credential_ref`, before `scrub_config`:** the connection now has a
///   `credential_ref`, so the next sync takes the **sealed** path (which reads only
///   the encrypted blob and ignores the plaintext `config` keys entirely) — a working,
///   secure-at-runtime connection. The plaintext keys linger in `config`; because the
///   reseal path is gated on `credential_ref` *absence*, it won't re-run to scrub
///   them. The ingest caller heals this on the next sync with a residual-scrub of a
///   sealed connection's stray plaintext keys, closing the window.
///
/// Sealing before referencing (never the reverse) guarantees the `credential_ref` a
/// connection points at is always already decryptable.
pub async fn reseal_gmail_plaintext(
    blob: &GmailTokens,
    resealer: &dyn GmailResealer,
) -> Result<String> {
    let credential_ref = resealer.seal(blob).await?;
    resealer.set_credential_ref(&credential_ref).await?;
    resealer.scrub_config(GMAIL_PLAINTEXT_KEYS).await?;
    Ok(credential_ref)
}

/// The per-mailbox cursor: the Gmail account `historyId`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct GmailCursor {
    h: u64,
}

impl GmailCursor {
    fn decode(cursor: Option<&Cursor>) -> Option<Self> {
        cursor.and_then(|c| serde_json::from_str(&c.0).ok())
    }

    fn encode(&self) -> Cursor {
        Cursor::new(serde_json::to_string(self).unwrap_or_default())
    }
}

/// Fold one `history` record into the touched/deleted id sets for `label`.
/// `messageAdded`/`labelAdded` mark a message touched (re-fetch); `messageDeleted`
/// and a `labelRemoved` that drops the synced label mark it deleted-from-mailbox.
fn classify_history(
    record: &Value,
    label: &str,
    touched: &mut HashSet<String>,
    deleted: &mut HashSet<String>,
) {
    for m in record
        .get("messagesAdded")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = m.pointer("/message/id").and_then(Value::as_str) {
            touched.insert(id.to_string());
        }
    }
    for m in record
        .get("messagesDeleted")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = m.pointer("/message/id").and_then(Value::as_str) {
            deleted.insert(id.to_string());
        }
    }
    for la in record
        .get("labelsAdded")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = la.pointer("/message/id").and_then(Value::as_str) {
            touched.insert(id.to_string());
        }
    }
    for lr in record
        .get("labelsRemoved")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = lr.pointer("/message/id").and_then(Value::as_str) else {
            continue;
        };
        if string_list(lr.get("labelIds")).iter().any(|l| l == label) {
            deleted.insert(id.to_string()); // left the synced label
        } else {
            touched.insert(id.to_string()); // a flag (UNREAD/STARRED) toggled
        }
    }
}

/// Derive provider flag tokens from a Gmail message's labels: a message is
/// `seen` unless it carries `UNREAD`, and `flagged` when it carries `STARRED`.
fn gmail_flags(label_ids: &[String]) -> Vec<String> {
    let mut flags = Vec::new();
    if !label_ids.iter().any(|l| l == "UNREAD") {
        flags.push("seen".to_string());
    }
    if label_ids.iter().any(|l| l == "STARRED") {
        flags.push("flagged".to_string());
    }
    flags.sort();
    flags
}

/// Decode Gmail's URL-safe base64 RAW payload (padding optional).
fn decode_raw(s: &str) -> Result<Vec<u8>> {
    use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
    use base64::engine::DecodePaddingMode;
    let cfg = GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent);
    let engine = GeneralPurpose::new(&base64::alphabet::URL_SAFE, cfg);
    engine
        .decode(s.trim())
        .map_err(|e| Error::provider(format!("Gmail base64 decode: {e}")))
}

/// A `Vec<String>` from a JSON string array (ignoring non-strings).
fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Gmail returns `historyId`/`internalDate` as uint64 **strings**; accept either
/// a JSON string or number.
fn as_u64(v: Option<&Value>) -> Option<u64> {
    match v {
        Some(Value::String(s)) => s.parse().ok(),
        Some(Value::Number(n)) => n.as_u64(),
        _ => None,
    }
}

fn ensure_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::unauthorized(format!(
            "Gmail {what} returned {status}"
        )));
    }
    if !status.is_success() {
        return Err(Error::provider(format!("Gmail {what} returned {status}")));
    }
    Ok(resp)
}

fn req_str(config: &Value, key: &str) -> Result<String> {
    opt_str(config, key)
        .ok_or_else(|| Error::invalid(format!("gmail email config requires a non-empty `{key}`")))
}

fn opt_str(config: &Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    #[test]
    fn from_config_requires_oauth_triplet() {
        let (ws, c) = (WorkspaceId::new(), ConnectionId::new());
        assert!(GmailProvider::from_config(ws, c, &json!({"client_id": "x"})).is_err());
        let ok = GmailProvider::from_config(
            ws,
            c,
            &json!({"client_id": "x", "client_secret": "y", "refresh_token": "z"}),
        )
        .unwrap();
        assert_eq!(ok.label, "INBOX");
        assert_eq!(ok.mailbox().external_id, "INBOX");
        assert!(matches!(ok.auth, GmailAuth::Plaintext { .. }));
    }

    /// An in-memory [`GmailTokenStore`] recording persisted blobs (test double,
    /// mirrors the calendar seam test).
    struct FakeTokenStore(Arc<tokio::sync::Mutex<GmailTokens>>);

    #[async_trait]
    impl GmailTokenStore for FakeTokenStore {
        async fn load(&self) -> Result<GmailTokens> {
            Ok(self.0.lock().await.clone())
        }
        async fn store(&self, tokens: &GmailTokens) -> Result<()> {
            *self.0.lock().await = tokens.clone();
            Ok(())
        }
    }

    fn sealed_blob() -> GmailTokens {
        GmailTokens {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            access_token: Some("at-old".into()),
            refresh_token: "rt".into(),
            expiry: Some(chrono::Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap()),
        }
    }

    #[tokio::test]
    async fn from_sealed_reads_label_from_config_not_credentials() {
        let (ws, c) = (WorkspaceId::new(), ConnectionId::new());
        let store: Arc<dyn GmailTokenStore> = Arc::new(FakeTokenStore(Arc::new(
            tokio::sync::Mutex::new(sealed_blob()),
        )));
        // No client_id/secret/refresh_token in config — the sealed path reads none.
        let p = GmailProvider::from_sealed(ws, c, &json!({ "label": "Work" }), store).unwrap();
        assert_eq!(p.label, "Work");
        assert_eq!(p.mailbox().external_id, "Work");
        assert!(p.mailbox().read_only);
        assert!(p.is_incremental());
        assert!(matches!(p.auth, GmailAuth::Sealed { .. }));
        assert_eq!(p.list_mailboxes().await.unwrap().len(), 1);
    }

    #[test]
    fn needs_refresh_respects_expiry_and_skew() {
        let t = sealed_blob();
        // Well before expiry ⇒ no refresh.
        assert!(!needs_refresh(
            &t,
            Utc.with_ymd_and_hms(2026, 7, 2, 11, 0, 0).unwrap(),
            60
        ));
        // Within the 60s skew window ⇒ refresh.
        assert!(needs_refresh(
            &t,
            Utc.with_ymd_and_hms(2026, 7, 2, 11, 59, 30).unwrap(),
            60
        ));
        // Past expiry ⇒ refresh.
        assert!(needs_refresh(
            &t,
            Utc.with_ymd_and_hms(2026, 7, 2, 13, 0, 0).unwrap(),
            60
        ));
        // No token or no expiry ⇒ refresh.
        let mut t2 = sealed_blob();
        t2.access_token = None;
        assert!(needs_refresh(&t2, Utc::now(), 60));
        let mut t3 = sealed_blob();
        t3.expiry = None;
        assert!(needs_refresh(&t3, Utc::now(), 60));
    }

    #[test]
    fn apply_refresh_updates_token_and_rotates_refresh_token() {
        let mut t = sealed_blob();
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 13, 0, 0).unwrap();
        // Google usually omits a rotated refresh token.
        apply_refresh(
            &mut t,
            &TokenResponse {
                access_token: "at-new".into(),
                expires_in: 3600,
                refresh_token: None,
            },
            now,
        );
        assert_eq!(t.access_token.as_deref(), Some("at-new"));
        assert_eq!(t.expiry, Some(now + chrono::Duration::seconds(3600)));
        assert_eq!(t.refresh_token, "rt", "unchanged when none returned");
        assert!(!needs_refresh(&t, now, 60));
        // When Google *does* rotate, the new refresh token is persisted.
        apply_refresh(
            &mut t,
            &TokenResponse {
                access_token: "at-3".into(),
                expires_in: 3600,
                refresh_token: Some("rt-new".into()),
            },
            now,
        );
        assert_eq!(t.refresh_token, "rt-new");
    }

    #[test]
    fn parse_token_response_reads_fields_with_defaults() {
        let v = json!({ "access_token": "AT", "expires_in": 1800, "refresh_token": "RT" });
        let r = parse_token_response(&v).unwrap();
        assert_eq!(r.access_token, "AT");
        assert_eq!(r.expires_in, 1800);
        assert_eq!(r.refresh_token.as_deref(), Some("RT"));
        let r = parse_token_response(&json!({ "access_token": "AT" })).unwrap();
        assert_eq!(r.expires_in, 3600);
        assert!(r.refresh_token.is_none());
        assert!(parse_token_response(&json!({})).is_err());
    }

    #[test]
    fn gmail_tokens_json_shape_matches_calendar_blob() {
        // The sealed blob must round-trip the exact fields the OAuth callback seals
        // (client_id/client_secret/access_token/refresh_token/expiry) so the unified
        // store the calendar flow writes is readable here.
        let blob = sealed_blob();
        let bytes = serde_json::to_vec(&blob).unwrap();
        let back: GmailTokens = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(blob, back);
        // A blob missing the optional fields still decodes (access_token/expiry
        // default to None).
        let minimal: GmailTokens = serde_json::from_value(
            json!({ "client_id": "c", "client_secret": "s", "refresh_token": "r" }),
        )
        .unwrap();
        assert!(minimal.access_token.is_none());
        assert!(minimal.expiry.is_none());
    }

    #[test]
    fn flags_from_labels() {
        assert_eq!(gmail_flags(&["INBOX".into()]), vec!["seen".to_string()]);
        assert_eq!(
            gmail_flags(&["INBOX".into(), "UNREAD".into()]),
            Vec::<String>::new()
        );
        assert_eq!(
            gmail_flags(&["INBOX".into(), "STARRED".into()]),
            vec!["flagged".to_string(), "seen".to_string()]
        );
    }

    #[test]
    fn decode_raw_handles_url_safe_with_and_without_padding() {
        // "hi>?" base64url is "aGk-Pw==" (uses - and _ for 62/63).
        let bytes = [0x68, 0x69, 0x3e, 0x3f];
        let padded = "aGk-Pw==";
        let unpadded = "aGk-Pw";
        assert_eq!(decode_raw(padded).unwrap(), bytes);
        assert_eq!(decode_raw(unpadded).unwrap(), bytes);
    }

    #[test]
    fn as_u64_accepts_string_or_number() {
        assert_eq!(as_u64(Some(&json!("12345"))), Some(12345));
        assert_eq!(as_u64(Some(&json!(678))), Some(678));
        assert_eq!(as_u64(Some(&json!("nope"))), None);
        assert_eq!(as_u64(None), None);
    }

    #[test]
    fn cursor_round_trips() {
        let c = GmailCursor { h: 99 }.encode();
        assert_eq!(GmailCursor::decode(Some(&c)).unwrap().h, 99);
        assert!(GmailCursor::decode(Some(&Cursor::new("opaque"))).is_none());
    }

    #[test]
    fn history_classify_routes_added_deleted_relabelled() {
        let label = "INBOX";
        let mut touched = HashSet::new();
        let mut deleted = HashSet::new();
        // A new message in the label, a deletion, a star toggle, and a move-out.
        let record = json!({
            "messagesAdded": [{ "message": { "id": "added1" } }],
            "messagesDeleted": [{ "message": { "id": "del1" } }],
            "labelsAdded": [{ "message": { "id": "star1" }, "labelIds": ["STARRED"] }],
            "labelsRemoved": [{ "message": { "id": "moved1" }, "labelIds": ["INBOX"] }]
        });
        classify_history(&record, label, &mut touched, &mut deleted);
        assert!(touched.contains("added1"));
        assert!(touched.contains("star1"));
        assert!(deleted.contains("del1"));
        assert!(deleted.contains("moved1"));
        assert!(!touched.contains("moved1"));
    }

    // --- opportunistic resealing (SOUL §13/§28) ------------------------------

    /// A [`GmailResealer`] recording the order + arguments of each call, so a test
    /// can assert the crash-safe `seal → set_credential_ref → scrub_config` sequence.
    #[derive(Default)]
    struct RecordingResealer {
        calls: tokio::sync::Mutex<Vec<&'static str>>,
        sealed: tokio::sync::Mutex<Option<GmailTokens>>,
        referenced: tokio::sync::Mutex<Option<String>>,
        scrubbed: tokio::sync::Mutex<Option<Vec<String>>>,
    }

    #[async_trait]
    impl GmailResealer for RecordingResealer {
        async fn seal(&self, tokens: &GmailTokens) -> Result<String> {
            self.calls.lock().await.push("seal");
            *self.sealed.lock().await = Some(tokens.clone());
            Ok("sec-reseal-1".to_string())
        }
        async fn set_credential_ref(&self, credential_ref: &str) -> Result<()> {
            self.calls.lock().await.push("set_credential_ref");
            *self.referenced.lock().await = Some(credential_ref.to_string());
            Ok(())
        }
        async fn scrub_config(&self, keys: &[&str]) -> Result<()> {
            self.calls.lock().await.push("scrub_config");
            *self.scrubbed.lock().await = Some(keys.iter().map(|k| (*k).to_string()).collect());
            Ok(())
        }
    }

    #[tokio::test]
    async fn reseal_seals_then_refs_then_scrubs_in_order() {
        // The fake token exchange: a successful refresh grant folded into a sealable
        // blob via the same `apply_refresh` state machine the sealed path uses.
        let mut blob = GmailTokens {
            client_id: "cid".into(),
            client_secret: "sec".into(),
            access_token: None,
            refresh_token: "rt".into(),
            expiry: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap();
        apply_refresh(
            &mut blob,
            &TokenResponse {
                access_token: "AT".into(),
                expires_in: 3600,
                refresh_token: None,
            },
            now,
        );

        let resealer = RecordingResealer::default();
        let cred_ref = reseal_gmail_plaintext(&blob, &resealer).await.unwrap();
        assert_eq!(cred_ref, "sec-reseal-1");

        // Order is the crash-safe sequence: seal → set_credential_ref → scrub.
        assert_eq!(
            *resealer.calls.lock().await,
            vec!["seal", "set_credential_ref", "scrub_config"]
        );
        // The full 5-field blob is sealed, carrying the proven access token + expiry.
        let sealed = resealer.sealed.lock().await.clone().unwrap();
        assert_eq!(sealed.client_id, "cid");
        assert_eq!(sealed.client_secret, "sec");
        assert_eq!(sealed.refresh_token, "rt");
        assert_eq!(sealed.access_token.as_deref(), Some("AT"));
        assert_eq!(sealed.expiry, Some(now + chrono::Duration::seconds(3600)));
        // set_credential_ref receives exactly the ref `seal` minted (seal-before-ref).
        assert_eq!(
            resealer.referenced.lock().await.as_deref(),
            Some("sec-reseal-1")
        );
        // Exactly the three plaintext config keys are scrubbed (ref-before-scrub).
        assert_eq!(
            resealer.scrubbed.lock().await.clone().unwrap(),
            GMAIL_PLAINTEXT_KEYS
                .iter()
                .map(|k| (*k).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn sealed_provider_yields_no_reseal_blob_is_idempotent() {
        // Idempotence: a connection already on the sealed path carries a
        // credential_ref, so a rebuilt provider takes the sealed branch and has
        // nothing to reseal — `plaintext_reseal_blob` returns None (no grant, no
        // network), which is why the reseal path can never re-trigger once it has run.
        let (ws, c) = (WorkspaceId::new(), ConnectionId::new());
        let store: Arc<dyn GmailTokenStore> = Arc::new(FakeTokenStore(Arc::new(
            tokio::sync::Mutex::new(sealed_blob()),
        )));
        let p = GmailProvider::from_sealed(ws, c, &json!({ "label": "INBOX" }), store).unwrap();
        assert!(p.plaintext_reseal_blob().await.unwrap().is_none());
    }

    #[test]
    fn plaintext_keys_are_exactly_the_config_triplet() {
        // The keys resealing scrubs are precisely the triplet `from_config` reads, so
        // a reseal never leaves a plaintext secret behind nor deletes a still-needed
        // key.
        assert_eq!(
            GMAIL_PLAINTEXT_KEYS,
            ["client_id", "client_secret", "refresh_token"]
        );
    }
}
