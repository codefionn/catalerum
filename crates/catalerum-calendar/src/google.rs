//! Google Calendar provider over the Calendar v3 REST API (SOUL §8/§16 M7).
//!
//! A real, OAuth2-backed [`CalendarProvider`] for a Google account, replacing the
//! former M7 placeholder. It reads one calendar (the configured id, default
//! `primary`) via `events.list`, mapping Google's event resource to the core
//! [`Event`] model, and **writes back** via `events.insert`/`patch`/`delete`
//! (updates PATCH only the mapped fields, so server-side attendees/reminders
//! survive; deletes are idempotent on `404`/`410`). Writes need the
//! [`CALENDAR_EVENTS_SCOPE`] the connect flow now requests — a connection minted
//! under the older read-only scope keeps syncing and fails writes with a clear
//! re-connect hint.
//!
//! ## Auth — OAuth2 authorization-code + refresh grant
//! Unlike the M-stage Gmail provider (which reads a plaintext `refresh_token`
//! straight from `connections.config`), the tokens here live **encrypted at rest**
//! in the secret store: the API's `/auth/google/*` web flow performs the code
//! exchange and seals `{client_id, client_secret, access_token, refresh_token,
//! expiry}` behind the connection's `credential_ref`. The provider reaches that
//! sealed blob through the [`GoogleTokenStore`] seam (so this crate depends on no
//! secret-store / DB code, SOUL §3.2). On each sync it uses the cached access
//! token while valid, else transparently **refreshes** it with the refresh-token
//! grant — single-flight (a held [`tokio::sync::Mutex`]) — and **persists** the
//! rotated blob back through the same seam.
//!
//! ## Incrementality (SOUL §3.4)
//! The [`Cursor`] is Google's `syncToken`:
//! - First sync (no cursor): `events.list?singleEvents=false`, paged; the last
//!   page yields `nextSyncToken` → the cursor.
//! - Incremental: `events.list?syncToken=…`, paged; items with
//!   `status == "cancelled"` become [`deletions`](SyncBatch::deletions), the rest
//!   upserts; the new `nextSyncToken` is the next cursor.
//! - `410 GONE` (a stale/expired `syncToken`): the token is cleared and a full
//!   resync runs. [`is_incremental`](CalendarProvider::is_incremental) stays
//!   `true`, so the collect/sync path's snapshot-deletion reconcile (gated on
//!   `!incremental`) correctly does **not** fire on that forced full pass — a full
//!   snapshot never masquerades as "delete everything". (The trade-off, matching
//!   the Gmail `historyId`-expiry path, is that deletions that happened during the
//!   token-expiry gap aren't reconciled until they re-surface; see the report.)
//!
//! Recurrence is carried verbatim as the raw `RRULE` from Google's `recurrence[]`
//! (like the iCalendar backends store `RRULE`); local expansion and single-instance
//! exception merging are **deferred** (an exception instance is stored as its own
//! event, keyed by its Google id, so it never collides with the master).

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use catalerum_core::error::{Error, Result};
use catalerum_core::id::{CalendarId, ConnectionId, WorkspaceId};
use catalerum_core::model::{Attachment, Calendar, Cursor, Event};
use catalerum_core::provider::{CalendarProvider, NewEvent, SyncBatch};

/// Google's OAuth2 token endpoint (refresh-token grant).
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google's OAuth2 authorization endpoint (consent screen). The API's connect
/// route 302s here; exposed so the route and the provider agree on one constant.
pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Base for the Calendar v3 REST API.
const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// The read-only Calendar scope (what connections minted before write-back
/// landed hold; still sufficient for sync).
pub const CALENDAR_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

/// The events read/**write** Calendar scope the OAuth flow requests now that
/// write-back exists: covers `events.list`/`watch` (sync) plus
/// `insert`/`patch`/`delete`. A connection still holding only the read-only
/// scope keeps syncing; its writes fail with a clear `403` until re-connected.
pub const CALENDAR_EVENTS_SCOPE: &str = "https://www.googleapis.com/auth/calendar.events";

/// `events.list` page size.
const PAGE: usize = 250;

/// A generous cap on the number of pages a single sync drains, so a
/// misbehaving/looping server can't spin forever. 400 pages × 250 ≈ 100k events.
const MAX_PAGES: usize = 400;

/// Cap on bytes read from a Google API response body — bounds an unbounded or
/// malicious upstream so it can't OOM the sync worker (mirrors the CalDAV/JMAP/
/// Gmail bounded-read discipline; `reqwest::Response::json()` buffers with no
/// limit). 64 MiB is generous for a page of events.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Clock-skew leeway (seconds): refresh the access token this long *before* its
/// nominal expiry so an in-flight request never races the expiry.
const EXPIRY_SKEW_SECS: i64 = 60;

/// The OAuth material for one Google connection — sealed (AES-GCM) behind the
/// connection's `credential_ref` and reached through [`GoogleTokenStore`].
///
/// The client id/secret ride along with the tokens (rather than living in the
/// plaintext `config` blob) so the whole refresh-grant input is encrypted at rest
/// and the provider can refresh with a single decrypt — no app-config lookup in
/// the ingest worker.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleTokens {
    /// OAuth client id of the catalerum Google app (from `[google]`).
    pub client_id: String,
    /// OAuth client secret of the catalerum Google app (from `[google]`).
    pub client_secret: String,
    /// The current short-lived access token, if one has been minted. Absent
    /// forces a refresh on first use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// The long-lived refresh token (offline access). Required.
    pub refresh_token: String,
    /// When [`access_token`](Self::access_token) expires (absent ⇒ treat as
    /// already expired, forcing a refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<DateTime<Utc>>,
}

/// The persistence seam for a Google connection's [`GoogleTokens`] (SOUL §13).
///
/// Kept a trait so `catalerum-calendar` needs no secret-store / DB dependency: the
/// ingest layer supplies an implementation backed by the AES-GCM secret store
/// (keyed by the connection's `credential_ref`). The provider calls
/// [`load`](Self::load) once per sync run and [`store`](Self::store) only when a
/// refresh rotates the blob.
#[async_trait]
pub trait GoogleTokenStore: Send + Sync {
    /// Decrypt and return the connection's current OAuth material.
    async fn load(&self) -> Result<GoogleTokens>;
    /// Re-seal the (rotated) OAuth material in place.
    async fn store(&self, tokens: &GoogleTokens) -> Result<()>;
}

/// A Google Calendar [`CalendarProvider`] for one calendar (SOUL §8).
pub struct GoogleCalendarProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    /// The calendar id this connection syncs (`primary` or a specific id).
    calendar_id: String,
    tokens: Arc<dyn GoogleTokenStore>,
    http: reqwest::Client,
    /// Cached, decrypted OAuth blob for this run — refreshed under the lock so
    /// concurrent callers single-flight the token refresh.
    cache: tokio::sync::Mutex<Option<GoogleTokens>>,
}

impl std::fmt::Debug for GoogleCalendarProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCalendarProvider")
            .field("workspace_id", &self.workspace_id)
            .field("connection_id", &self.connection_id)
            .field("calendar_id", &self.calendar_id)
            .finish_non_exhaustive()
    }
}

impl GoogleCalendarProvider {
    /// Build from a connection's `config` JSON plus the token seam. The only
    /// config key read is the optional `calendar` (alias `calendar_id`) — default
    /// `primary`; the OAuth material comes from `tokens`, never `config`.
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
        tokens: Arc<dyn GoogleTokenStore>,
    ) -> Result<Self> {
        let calendar_id = config
            .get("calendar")
            .or_else(|| config.get("calendar_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("primary")
            .to_string();
        let http = reqwest::Client::builder()
            .user_agent("catalerum-calendar")
            // Fail fast on an unreachable endpoint rather than hanging the sync
            // worker; no overall timeout (a page can be large and paginates).
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Provider(format!("build http client: {e}")))?;
        Ok(Self {
            workspace_id,
            connection_id,
            calendar_id,
            tokens,
            http,
            cache: tokio::sync::Mutex::new(None),
        })
    }

    /// Construct directly (tests / explicit wiring).
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        calendar_id: impl Into<String>,
        tokens: Arc<dyn GoogleTokenStore>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            workspace_id,
            connection_id,
            calendar_id: calendar_id.into(),
            tokens,
            http,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    /// The calendar id this connection syncs.
    #[must_use]
    pub fn calendar_id(&self) -> &str {
        &self.calendar_id
    }

    /// The single [`Calendar`] this connection represents. Multi-calendar
    /// discovery (the CalendarList API) is a deferred enhancement; the configured
    /// id is the calendar, with a stable derived id. Writable: the connect flow
    /// requests [`CALENDAR_EVENTS_SCOPE`]; a connection still holding the older
    /// read-only scope syncs fine and fails writes with a clear `403`.
    fn calendar(&self) -> Calendar {
        let id = CalendarId::from_uuid(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("{}/{}", self.connection_id, self.calendar_id).as_bytes(),
        ));
        Calendar {
            id,
            workspace_id: self.workspace_id,
            connection_id: Some(self.connection_id),
            external_id: self.calendar_id.clone(),
            name: self.calendar_id.clone(),
            read_only: false,
        }
    }

    /// A valid access token, minting a fresh one via the refresh-token grant when
    /// the cached one is absent or (about to be) expired. Single-flight: the cache
    /// lock is held across the refresh so two concurrent syncs never both refresh,
    /// and the rotated blob is persisted before the lock is released.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.cache.lock().await;
        if guard.is_none() {
            *guard = Some(self.tokens.load().await?);
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
        // Persist the rotated blob (new access token/expiry, possibly a rotated
        // refresh token) so the next run starts from it.
        self.tokens.store(blob).await?;
        Ok(blob
            .access_token
            .clone()
            .expect("apply_refresh sets the access token"))
    }

    /// GET one `events.list` page. `sync_token` present ⇒ incremental request
    /// (only `syncToken`/`pageToken`/`maxResults` — Google forbids resending
    /// filters alongside a `syncToken`); absent ⇒ initial full pull
    /// (`singleEvents=false`). Returns `Ok(None)` on `410 GONE` (stale token).
    async fn events_page(
        &self,
        token: &str,
        sync_token: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Option<Value>> {
        let mut query: Vec<(&str, String)> = vec![("maxResults", PAGE.to_string())];
        match sync_token {
            Some(tok) => query.push(("syncToken", tok.to_string())),
            None => query.push(("singleEvents", "false".to_string())),
        }
        if let Some(pt) = page_token {
            query.push(("pageToken", pt.to_string()));
        }
        // Build the URL by hand (this reqwest build gates `.query()`/`.form()`).
        let url = format!(
            "{API_BASE}/calendars/{}/events?{}",
            percent_encode(&self.calendar_id),
            encode_query(&query),
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Google events.list: {e}")))?;
        if resp.status() == reqwest::StatusCode::GONE {
            // 410: the syncToken expired — the caller clears it and full-resyncs.
            return Ok(None);
        }
        let resp = ensure_success(resp, "events.list")?;
        Ok(Some(read_json_capped(resp, "Google events").await?))
    }

    /// Drain every page of an `events.list` walk (initial or incremental) into a
    /// single batch. Returns `Ok(None)` if any page reported `410 GONE`.
    async fn drain(&self, token: &str, sync_token: Option<&str>) -> Result<Option<DrainedBatch>> {
        let cal = self.calendar();
        let mut upserts = Vec::new();
        let mut deletions = Vec::new();
        let mut next_sync: Option<String> = None;
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let Some(page) = self
                .events_page(token, sync_token, page_token.as_deref())
                .await?
            else {
                return Ok(None); // 410 — stale token
            };
            let parsed = parse_events_response(&page, self.workspace_id, cal.id)?;
            upserts.extend(parsed.upserts);
            deletions.extend(parsed.deletions);
            if let Some(ns) = parsed.next_sync_token {
                next_sync = Some(ns);
            }
            match parsed.next_page_token {
                Some(pt) => page_token = Some(pt),
                None => break,
            }
        }
        Ok(Some(DrainedBatch {
            upserts,
            deletions,
            next_sync,
        }))
    }

    /// Send `body` as a JSON request via `req` (insert/patch), mapping transport
    /// errors to a provider error.
    async fn send_json(
        &self,
        req: reqwest::RequestBuilder,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let payload = serde_json::to_vec(body)
            .map_err(|e| Error::Provider(format!("encode Google event body: {e}")))?;
        req.header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Google event write: {e}")))
    }

    // -----------------------------------------------------------------------
    // Push notification channels (SOUL §8/§16 M7 — push half)
    // -----------------------------------------------------------------------

    /// Start a Google **push channel** on this connection's calendar via
    /// `events.watch`, so Google POSTs a notification to `address` whenever the
    /// calendar changes (SOUL §8). The notification carries only channel headers —
    /// not the change — so on receipt the webhook triggers an incremental collect
    /// (the same `syncToken` pull [`sync`](CalendarProvider::sync) does).
    ///
    /// `channel_id` is a caller-chosen unique id (a UUID); `channel_token` is the
    /// opaque secret Google echoes back in `X-Goog-Channel-Token` (an HMAC-signed
    /// value the webhook verifies — the channel's authorization). `ttl_secs`, when
    /// set, requests a channel lifetime; Google clamps it to its own maximum
    /// (~1 week) and the **returned** [`WatchChannel::expiry`] is authoritative.
    pub async fn start_watch(
        &self,
        address: &str,
        channel_id: &str,
        channel_token: &str,
        ttl_secs: Option<i64>,
    ) -> Result<WatchChannel> {
        let token = self.access_token().await?;
        let url = format!(
            "{API_BASE}/calendars/{}/events/watch",
            percent_encode(&self.calendar_id),
        );
        let body = watch_request_body(channel_id, address, channel_token, ttl_secs);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| Error::Provider(format!("encode watch body: {e}")))?;
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Google events.watch: {e}")))?;
        let resp = ensure_success(resp, "events.watch")?;
        let v = read_json_capped(resp, "Google watch").await?;
        parse_watch_response(&v, channel_id)
    }

    /// Stop a previously-started push channel via `channels.stop` (SOUL §8). A
    /// `404` (the channel already expired / was never known) is treated as success
    /// — stopping is idempotent, so a stale stored channel never wedges the scan.
    pub async fn stop_watch(&self, channel_id: &str, resource_id: &str) -> Result<()> {
        let token = self.access_token().await?;
        let url = format!("{API_BASE}/channels/stop");
        let body = stop_request_body(channel_id, resource_id);
        let payload = serde_json::to_vec(&body)
            .map_err(|e| Error::Provider(format!("encode channels.stop body: {e}")))?;
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Google channels.stop: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(()); // already gone — idempotent stop
        }
        ensure_success(resp, "channels.stop")?;
        Ok(())
    }
}

#[async_trait]
impl CalendarProvider for GoogleCalendarProvider {
    async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        // One connection maps to one calendar (the configured id). Multi-calendar
        // CalendarList discovery is a deferred enhancement.
        Ok(vec![self.calendar()])
    }

    async fn sync(&self, cal: &Calendar, cursor: Option<Cursor>) -> Result<SyncBatch<Event>> {
        let token = self.access_token().await?;
        let sync_token = decode_cursor(cursor.as_ref());

        // Incremental when a syncToken is held; a 410 falls back to a full pull.
        let drained = match &sync_token {
            Some(tok) => match self.drain(&token, Some(tok)).await? {
                Some(batch) => batch,
                None => self.drain(&token, None).await?.ok_or_else(|| {
                    Error::Provider("Google full resync unexpectedly reported 410".into())
                })?,
            },
            None => self
                .drain(&token, None)
                .await?
                .ok_or_else(|| Error::Provider("Google initial sync reported 410".into()))?,
        };

        // The syncToken only appears on the final page. If it's missing (paging
        // was interrupted by the page cap), keep the prior cursor so the next run
        // resumes rather than silently losing incrementality.
        let next_cursor = drained
            .next_sync
            .map(encode_cursor)
            .or(cursor)
            .unwrap_or_else(|| Cursor::new(String::new()));

        // Ensure every upsert points at the caller's stored calendar id.
        let upserts = drained
            .upserts
            .into_iter()
            .map(|mut e| {
                e.calendar_id = cal.id;
                e
            })
            .collect();

        Ok(SyncBatch {
            upserts,
            deletions: drained.deletions,
            next_cursor,
            has_more: false,
        })
    }

    /// Google `events.list?syncToken=…` is a true incremental delta (with
    /// `status:"cancelled"` deletions), so the consumer must not diff-reconcile.
    fn is_incremental(&self) -> bool {
        true
    }

    async fn create_event(&self, cal: &Calendar, event: NewEvent) -> Result<Event> {
        let token = self.access_token().await?;
        let url = format!(
            "{API_BASE}/calendars/{}/events",
            percent_encode(&self.calendar_id),
        );
        let body = event_write_body(
            &write_fields_from_new(&event),
            Some(&event.attendees),
            false,
        );
        let resp = self
            .send_json(self.http.post(&url).bearer_auth(token), &body)
            .await?;
        let resp = ensure_success(resp, "events.insert")?;
        let v = read_json_capped(resp, "Google events.insert").await?;
        let mut created = event_from_json(&v, self.workspace_id, cal.id)?;
        created.calendar_id = cal.id;
        Ok(created)
    }

    async fn update_event(&self, event: &Event) -> Result<Event> {
        let token = self.access_token().await?;
        let url = format!(
            "{API_BASE}/calendars/{}/events/{}",
            percent_encode(&self.calendar_id),
            percent_encode(&event.uid),
        );
        // PATCH so unmapped server-side fields (attendees, reminders, conference
        // data) are preserved; the mapped fields are sent with explicit nulls so
        // clearing e.g. the location locally clears it on Google too.
        let body = event_write_body(&write_fields_from_event(event), None, true);
        let mut req = self.http.patch(&url).bearer_auth(token);
        if let Some(etag) = event.etag.as_deref().filter(|s| !s.is_empty()) {
            req = req.header(reqwest::header::IF_MATCH, etag);
        }
        let resp = self.send_json(req, &body).await?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "event `{}` changed on Google (ETag mismatch) — sync the calendar and \
                 retry the edit",
                event.uid
            )));
        }
        let resp = ensure_success(resp, "events.patch")?;
        let v = read_json_capped(resp, "Google events.patch").await?;
        let mut updated = event_from_json(&v, self.workspace_id, event.calendar_id)?;
        updated.calendar_id = event.calendar_id;
        Ok(updated)
    }

    async fn delete_event(&self, event: &Event) -> Result<()> {
        let token = self.access_token().await?;
        let url = format!(
            "{API_BASE}/calendars/{}/events/{}",
            percent_encode(&self.calendar_id),
            percent_encode(&event.uid),
        );
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Google events.delete: {e}")))?;
        // Already gone (404) or already cancelled (410): deletion is idempotent.
        if matches!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            return Ok(());
        }
        ensure_success(resp, "events.delete")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Token refresh — factored into pure decision/mutation helpers (fake-clock tests)
// ---------------------------------------------------------------------------

/// The subset of Google's token response the refresh grant yields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    /// Seconds until the access token expires (`expires_in`).
    pub expires_in: i64,
    /// A rotated refresh token, when Google returns one (usually it doesn't).
    pub refresh_token: Option<String>,
}

/// Whether the cached access token is absent or within [`EXPIRY_SKEW_SECS`] of
/// expiry — i.e. a refresh is due. Pure (fake-clock testable).
#[must_use]
pub fn needs_refresh(tokens: &GoogleTokens, now: DateTime<Utc>, skew_secs: i64) -> bool {
    match (&tokens.access_token, tokens.expiry) {
        (Some(_), Some(exp)) => exp <= now + chrono::Duration::seconds(skew_secs),
        _ => true, // no token, or no known expiry ⇒ refresh
    }
}

/// Fold a refresh response into the blob: new access token + expiry, and a
/// rotated refresh token when present. Pure (testable).
pub fn apply_refresh(tokens: &mut GoogleTokens, resp: &TokenResponse, now: DateTime<Utc>) {
    tokens.access_token = Some(resp.access_token.clone());
    tokens.expiry = Some(now + chrono::Duration::seconds(resp.expires_in));
    if let Some(rt) = resp.refresh_token.as_ref().filter(|s| !s.is_empty()) {
        tokens.refresh_token = rt.clone();
    }
}

/// Exchange an OAuth **authorization code** for the initial token set at Google's
/// token endpoint (the `authorization_code` grant), returning a ready-to-seal
/// [`GoogleTokens`] blob (client id/secret embedded for later refreshes). Used by
/// the API's `/auth/google/callback` route — which has no reqwest of its own — so
/// the HTTP client is built here. Errors if Google returns no `refresh_token`
/// (which happens unless the consent was requested with offline access + consent
/// prompt), since sync can't proceed without one.
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<GoogleTokens> {
    let http = reqwest::Client::builder()
        .user_agent("catalerum-calendar")
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| Error::Provider(format!("build http client: {e}")))?;
    let body = encode_query(&[
        ("client_id", client_id.to_string()),
        ("client_secret", client_secret.to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
        ("grant_type", "authorization_code".to_string()),
    ]);
    let resp = http
        .post(TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("Google code exchange: {e}")))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::BAD_REQUEST
    {
        return Err(Error::unauthorized(
            "Google authorization-code exchange rejected (bad code or redirect_uri mismatch)",
        ));
    }
    let resp = ensure_success(resp, "code exchange")?;
    let v: Value = read_json_capped(resp, "Google code exchange").await?;
    let token = parse_token_response(&v)?;
    let refresh_token = token.refresh_token.ok_or_else(|| {
        Error::invalid(
            "Google returned no refresh_token — the consent must request offline access \
             with a consent prompt (re-connect and grant offline access)",
        )
    })?;
    Ok(GoogleTokens {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        access_token: Some(token.access_token),
        refresh_token,
        expiry: Some(Utc::now() + chrono::Duration::seconds(token.expires_in)),
    })
}

/// Exchange a refresh token for a fresh access token at Google's token endpoint.
async fn refresh_grant(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let body = encode_query(&[
        ("client_id", client_id.to_string()),
        ("client_secret", client_secret.to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("grant_type", "refresh_token".to_string()),
    ]);
    let resp = http
        .post(TOKEN_URL)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("Google token request: {e}")))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::BAD_REQUEST
    {
        return Err(Error::unauthorized(
            "Google refresh-token grant rejected (revoked or invalid credentials)",
        ));
    }
    let resp = ensure_success(resp, "token")?;
    let v: Value = read_json_capped(resp, "Google token").await?;
    parse_token_response(&v)
}

/// Parse Google's token endpoint JSON into a [`TokenResponse`]. Pure (testable).
fn parse_token_response(v: &Value) -> Result<TokenResponse> {
    let access_token = v
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::provider("Google token response has no access_token"))?;
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

// ---------------------------------------------------------------------------
// Event mapping — pure (fixture-driven tests)
// ---------------------------------------------------------------------------

/// One drained `events.list` walk.
struct DrainedBatch {
    upserts: Vec<Event>,
    deletions: Vec<String>,
    next_sync: Option<String>,
}

/// The parsed content of a single `events.list` page.
struct ParsedPage {
    upserts: Vec<Event>,
    deletions: Vec<String>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

/// Split one `events.list` response page into upserts + deletions + the paging
/// tokens. A `status:"cancelled"` item is a deletion (keyed by the Google event
/// `id`); anything else maps to an [`Event`]. Pure — the mapping test target.
fn parse_events_response(
    page: &Value,
    workspace_id: WorkspaceId,
    calendar_id: CalendarId,
) -> Result<ParsedPage> {
    let mut upserts = Vec::new();
    let mut deletions = Vec::new();
    for item in page
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            continue; // an item with no id is unusable
        };
        if item.get("status").and_then(Value::as_str) == Some("cancelled") {
            deletions.push(id.to_string());
            continue;
        }
        // A single malformed/unmappable event must not fail the whole page — skip
        // it and keep the batch, mirroring the Gmail/IMAP paths. (This crate takes
        // no `tracing` dependency, so the skip is silent by design.)
        if let Ok(event) = event_from_json(item, workspace_id, calendar_id) {
            upserts.push(event);
        }
        let _ = id; // id is only used for the cancelled/deletion branch above
    }
    Ok(ParsedPage {
        upserts,
        deletions,
        next_page_token: page
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string),
        next_sync_token: page
            .get("nextSyncToken")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Map one Google Calendar v3 event resource to a core [`Event`]. Keyed by the
/// Google event `id` (stable, present on every item including cancelled ones, so
/// upsert and delete agree). Pure. Attendees are parsed by Google but the core
/// `Event` holds only resolved entity pointers (filled by the ingest graph step),
/// so — exactly like the iCalendar backends — `attendees` is left empty here.
fn event_from_json(
    item: &Value,
    workspace_id: WorkspaceId,
    calendar_id: CalendarId,
) -> Result<Event> {
    let uid = item
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::provider("Google event has no id"))?
        .to_string();
    let (start, start_all_day) = parse_endpoint(item.get("start"))
        .ok_or_else(|| Error::provider("Google event has no parseable start"))?;
    let (end, end_all_day) = parse_endpoint(item.get("end")).unwrap_or_else(|| {
        // No end: an all-day event is one day; a timed point event ends at start.
        if start_all_day {
            (start + chrono::Duration::days(1), true)
        } else {
            (start, false)
        }
    });
    Ok(Event {
        id: catalerum_core::id::EventId::new(),
        workspace_id,
        calendar_id,
        uid,
        start,
        end,
        all_day: start_all_day || end_all_day,
        rrule: extract_rrule(item.get("recurrence")),
        summary: item
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        location: opt_string(item.get("location")),
        attendees: Vec::new(),
        body: opt_string(item.get("description")),
        labels: Vec::new(),
        attachments: parse_attachments(item.get("attachments")),
        etag: opt_string(item.get("etag")),
        sequence: item.get("sequence").and_then(Value::as_i64).unwrap_or(0),
    })
}

/// Resolve a Google `start`/`end` endpoint object to an absolute UTC instant plus
/// an `all_day` flag. Timed: `dateTime` (RFC3339). All-day: `date` (YYYY-MM-DD) ⇒
/// midnight UTC. Returns `None` when neither is present/parseable.
fn parse_endpoint(endpoint: Option<&Value>) -> Option<(DateTime<Utc>, bool)> {
    let endpoint = endpoint?;
    if let Some(dt) = endpoint.get("dateTime").and_then(Value::as_str) {
        return DateTime::parse_from_rfc3339(dt.trim())
            .ok()
            .map(|d| (d.with_timezone(&Utc), false));
    }
    if let Some(date) = endpoint.get("date").and_then(Value::as_str) {
        return NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|naive| (Utc.from_utc_datetime(&naive), true));
    }
    None
}

/// Extract the verbatim `RRULE` (without its `RRULE:` prefix) from Google's
/// `recurrence[]` array. Other lines (`EXDATE`, `RDATE`) are ignored for now.
fn extract_rrule(recurrence: Option<&Value>) -> Option<String> {
    recurrence
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .find_map(|line| {
            let t = line.trim();
            t.strip_prefix("RRULE:")
                .filter(|r| !r.is_empty())
                .map(str::to_string)
        })
}

/// Map Google `attachments[]` to core [`Attachment`]s (`fileUrl` → `url`, `title`
/// → `filename`, `mimeType` → `content_type`).
fn parse_attachments(attachments: Option<&Value>) -> Vec<Attachment> {
    attachments
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let url = a.get("fileUrl").and_then(Value::as_str)?.to_string();
                    Some(Attachment {
                        url,
                        filename: opt_string(a.get("title")),
                        content_type: opt_string(a.get("mimeType")),
                        size: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Event write-back — pure request shaping (fixture-tested)
// ---------------------------------------------------------------------------

/// The provider-writable fields of an event, shared by the create
/// ([`NewEvent`]) and update ([`Event`]) shaping paths.
struct WriteFields<'a> {
    summary: &'a str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    all_day: bool,
    location: Option<&'a str>,
    body: Option<&'a str>,
    rrule: Option<&'a str>,
}

fn write_fields_from_new(e: &NewEvent) -> WriteFields<'_> {
    WriteFields {
        summary: &e.summary,
        start: e.start,
        end: e.end,
        all_day: e.all_day,
        location: e.location.as_deref(),
        body: e.body.as_deref(),
        rrule: e.rrule.as_deref(),
    }
}

fn write_fields_from_event(e: &Event) -> WriteFields<'_> {
    WriteFields {
        summary: &e.summary,
        start: e.start,
        end: e.end,
        all_day: e.all_day,
        location: e.location.as_deref(),
        body: e.body.as_deref(),
        rrule: e.rrule.as_deref(),
    }
}

/// Shape one Google `start`/`end` endpoint: a timed instant becomes
/// `{dateTime}` (RFC3339 UTC), an all-day stamp becomes `{date}`. On a patch
/// the unused twin field rides as an explicit `null` — that is Google's
/// documented way to *switch* an event between timed and all-day.
fn endpoint_value(dt: DateTime<Utc>, all_day: bool, patch: bool) -> Value {
    let mut obj = serde_json::Map::new();
    if all_day {
        obj.insert(
            "date".to_string(),
            Value::String(dt.date_naive().format("%Y-%m-%d").to_string()),
        );
        if patch {
            obj.insert("dateTime".to_string(), Value::Null);
        }
    } else {
        obj.insert(
            "dateTime".to_string(),
            Value::String(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        );
        if patch {
            obj.insert("date".to_string(), Value::Null);
        }
    }
    Value::Object(obj)
}

/// Shape the Google event resource for `events.insert` (`patch = false`) or
/// `events.patch` (`patch = true`). On a patch, an unset optional field rides
/// as an explicit `null` so clearing it locally clears it on Google (an omitted
/// key would silently keep the stale value); on an insert unset fields are
/// omitted. `attendees` (create only — the stored event holds no addresses):
/// `Some(list)` writes `[{email}]` entries with any `mailto:` prefix stripped,
/// `None` omits the key so a patch preserves the server-side attendee list.
/// Pure — the write-shaping test target.
fn event_write_body(f: &WriteFields<'_>, attendees: Option<&[String]>, patch: bool) -> Value {
    let mut body = serde_json::Map::new();
    body.insert("summary".to_string(), Value::String(f.summary.to_string()));
    body.insert(
        "start".to_string(),
        endpoint_value(f.start, f.all_day, patch),
    );
    body.insert("end".to_string(), endpoint_value(f.end, f.all_day, patch));
    let set_or_clear =
        |body: &mut serde_json::Map<String, Value>, key: &str, v: Option<&str>| match v
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                body.insert(key.to_string(), Value::String(s.to_string()));
            }
            None if patch => {
                body.insert(key.to_string(), Value::Null);
            }
            None => {}
        };
    set_or_clear(&mut body, "location", f.location);
    set_or_clear(&mut body, "description", f.body);
    match f.rrule.map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => {
            body.insert(
                "recurrence".to_string(),
                Value::Array(vec![Value::String(format!("RRULE:{r}"))]),
            );
        }
        None if patch => {
            body.insert("recurrence".to_string(), Value::Null);
        }
        None => {}
    }
    if let Some(list) = attendees {
        let entries: Vec<Value> = list
            .iter()
            .filter_map(|a| {
                let a = a.trim();
                let email = a.strip_prefix("mailto:").unwrap_or(a).trim();
                (!email.is_empty()).then(|| serde_json::json!({ "email": email }))
            })
            .collect();
        if !entries.is_empty() {
            body.insert("attendees".to_string(), Value::Array(entries));
        }
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// Cursor + HTTP helpers
// ---------------------------------------------------------------------------

/// Cursor prefix for a Google `syncToken`.
const CURSOR_PREFIX: &str = "gsync:";

/// Encode a `syncToken` as a [`Cursor`].
fn encode_cursor(sync_token: String) -> Cursor {
    Cursor::new(format!("{CURSOR_PREFIX}{sync_token}"))
}

/// Recover the `syncToken` from a [`Cursor`] (a non-Google/empty cursor ⇒ `None`,
/// forcing a full sync).
fn decode_cursor(cursor: Option<&Cursor>) -> Option<String> {
    cursor
        .and_then(|c| c.0.strip_prefix(CURSOR_PREFIX))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Encode `pairs` as an `application/x-www-form-urlencoded` string — used for
/// both the query string and the token POST body (this reqwest build gates the
/// `.query()`/`.form()` builders, so we assemble them by hand).
fn encode_query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode a calendar id for use in a URL path segment (`primary` is a
/// no-op; a real id may contain `@`/`.`). Encodes everything outside the
/// URL-unreserved set.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn ensure_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status == reqwest::StatusCode::FORBIDDEN
        && matches!(what, "events.insert" | "events.patch" | "events.delete")
    {
        // The most likely 403 on a write is a connection minted before
        // write-back landed, still holding the read-only scope.
        return Err(Error::unauthorized(format!(
            "Google {what} returned 403 — the connection may hold the older read-only \
             scope; re-connect it (GET /auth/google/connect?kind=calendar&connection=…) \
             to grant event write access"
        )));
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::unauthorized(format!(
            "Google {what} returned {status}"
        )));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::NotFound);
    }
    if !status.is_success() {
        return Err(Error::Provider(format!("Google {what} returned {status}")));
    }
    Ok(resp)
}

/// Read a response body as JSON, capped at [`MAX_RESPONSE_BYTES`] (streamed via
/// `chunk()` so an unbounded upstream can't OOM the worker — the same discipline
/// the CalDAV body read uses).
async fn read_json_capped(mut resp: reqwest::Response, what: &str) -> Result<Value> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::Provider(format!("read {what}: {e}")))?
    {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Provider(format!(
                "{what} exceeds the {MAX_RESPONSE_BYTES}-byte cap; refusing to buffer it"
            )));
        }
        buf.extend_from_slice(chunk.as_ref());
    }
    serde_json::from_slice(&buf).map_err(|e| Error::Provider(format!("parse {what} JSON: {e}")))
}

// ---------------------------------------------------------------------------
// Push channel — pure request shaping / response parsing (fixture-tested)
// ---------------------------------------------------------------------------

/// A live Google push **channel** on a calendar (SOUL §8): the ids needed to stop
/// it (`channel_id` + `resource_id`) and when Google will let it expire. This is
/// exactly the state persisted on the connection (the ingest scan renews it before
/// [`expiry`](Self::expiry) and stops it by these ids).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchChannel {
    /// The channel id we chose (echoed back by Google) — the `id` for a later stop.
    pub channel_id: String,
    /// Google's opaque `resourceId` for the watched calendar — required to stop.
    pub resource_id: String,
    /// When Google will expire the channel (absent ⇒ unknown; the scan then treats
    /// it as due-for-renewal so a watch never silently lapses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<DateTime<Utc>>,
}

/// Shape the `events.watch` request body: a `web_hook` channel POSTing to
/// `address`, carrying our `token` (echoed as `X-Goog-Channel-Token`), with an
/// optional `params.ttl` (seconds, as Google wants it — a string). Pure.
fn watch_request_body(
    channel_id: &str,
    address: &str,
    token: &str,
    ttl_secs: Option<i64>,
) -> Value {
    let mut body = serde_json::json!({
        "id": channel_id,
        "type": "web_hook",
        "address": address,
        "token": token,
    });
    if let Some(ttl) = ttl_secs.filter(|t| *t > 0) {
        body["params"] = serde_json::json!({ "ttl": ttl.to_string() });
    }
    body
}

/// Shape the `channels.stop` request body: the `{id, resourceId}` pair. Pure.
fn stop_request_body(channel_id: &str, resource_id: &str) -> Value {
    serde_json::json!({ "id": channel_id, "resourceId": resource_id })
}

/// Parse an `events.watch` response (`api#channel`) into a [`WatchChannel`]: the
/// echoed `id` (falling back to the one we sent), the required `resourceId`, and
/// the `expiration` (Google's string of **milliseconds** since the Unix epoch).
/// Pure — the watch-response test target. A missing `resourceId` is an error (we
/// couldn't stop the channel later); a missing/unparseable `expiration` is left
/// `None` (the scan renews it eagerly rather than trusting an unknown lifetime).
fn parse_watch_response(v: &Value, fallback_channel_id: &str) -> Result<WatchChannel> {
    let channel_id = v
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_channel_id)
        .to_string();
    let resource_id = v
        .get("resourceId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::provider("Google watch response has no resourceId"))?
        .to_string();
    let expiry = v
        .get("expiration")
        .and_then(parse_epoch_millis)
        .and_then(DateTime::<Utc>::from_timestamp_millis);
    Ok(WatchChannel {
        channel_id,
        resource_id,
        expiry,
    })
}

/// Read Google's `expiration` (ms since epoch) — a JSON string in practice, but a
/// bare number is tolerated. `None` if absent/unparseable. Pure.
fn parse_epoch_millis(v: &Value) -> Option<i64> {
    match v {
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::Mutex as TokioMutex;

    fn ids() -> (WorkspaceId, CalendarId) {
        (WorkspaceId::new(), CalendarId::new())
    }

    // --- event mapping -----------------------------------------------------

    #[test]
    fn maps_timed_event_with_recurrence_and_attachment() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "evt-1",
            "status": "confirmed",
            "summary": "Weekly sync",
            "location": "Room 5",
            "description": "Agenda inside",
            "start": { "dateTime": "2026-07-02T10:00:00-07:00", "timeZone": "America/Los_Angeles" },
            "end": { "dateTime": "2026-07-02T11:00:00-07:00" },
            "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=TH", "EXDATE;TZID=UTC:20260709T170000Z"],
            "attachments": [
                { "fileUrl": "https://drive.google.com/x", "title": "brief.pdf", "mimeType": "application/pdf" }
            ],
            "etag": "\"etag-1\"",
            "sequence": 3
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert_eq!(e.uid, "evt-1");
        assert_eq!(e.summary, "Weekly sync");
        assert_eq!(e.location.as_deref(), Some("Room 5"));
        assert_eq!(e.body.as_deref(), Some("Agenda inside"));
        // 10:00 PDT (-07:00) == 17:00 UTC.
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 7, 2, 17, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 7, 2, 18, 0, 0).unwrap());
        assert_eq!(e.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=TH"));
        assert_eq!(e.attachments.len(), 1);
        assert_eq!(e.attachments[0].url, "https://drive.google.com/x");
        assert_eq!(e.attachments[0].filename.as_deref(), Some("brief.pdf"));
        assert_eq!(
            e.attachments[0].content_type.as_deref(),
            Some("application/pdf")
        );
        assert_eq!(e.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(e.sequence, 3);
        assert!(!e.all_day);
        assert!(
            e.attendees.is_empty(),
            "attendees resolved later, empty here"
        );
    }

    #[test]
    fn maps_all_day_event_to_midnight_utc() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "evt-day",
            "summary": "Holiday",
            "start": { "date": "2026-07-04" },
            "end": { "date": "2026-07-05" }
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap());
        assert!(e.all_day);
        assert!(e.rrule.is_none());
    }

    #[test]
    fn all_day_event_without_end_spans_one_day() {
        let (ws, cal) = ids();
        let item = json!({ "id": "d", "start": { "date": "2026-07-04" } });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap());
    }

    #[test]
    fn parse_page_splits_upserts_and_cancelled_deletions() {
        let (ws, cal) = ids();
        let page = json!({
            "items": [
                { "id": "a", "status": "confirmed", "summary": "A",
                  "start": { "dateTime": "2026-07-02T10:00:00Z" }, "end": { "dateTime": "2026-07-02T11:00:00Z" } },
                { "id": "b", "status": "cancelled" },
                { "id": "c", "status": "confirmed", "summary": "C",
                  "start": { "date": "2026-07-03" } }
            ],
            "nextSyncToken": "TOKEN-2"
        });
        let parsed = parse_events_response(&page, ws, cal).unwrap();
        let upsert_ids: Vec<_> = parsed.upserts.iter().map(|e| e.uid.as_str()).collect();
        assert_eq!(upsert_ids, vec!["a", "c"]);
        assert_eq!(parsed.deletions, vec!["b".to_string()]);
        assert_eq!(parsed.next_sync_token.as_deref(), Some("TOKEN-2"));
        assert!(parsed.next_page_token.is_none());
    }

    #[test]
    fn parse_page_skips_unmappable_but_keeps_the_rest() {
        let (ws, cal) = ids();
        let page = json!({
            "items": [
                { "id": "bad", "status": "confirmed" }, // no start ⇒ skipped
                { "id": "good", "status": "confirmed", "start": { "date": "2026-07-03" } }
            ]
        });
        let parsed = parse_events_response(&page, ws, cal).unwrap();
        assert_eq!(parsed.upserts.len(), 1);
        assert_eq!(parsed.upserts[0].uid, "good");
    }

    // --- cursor round-trip -------------------------------------------------

    #[test]
    fn sync_token_cursor_round_trips() {
        let c = encode_cursor("abc-123".to_string());
        assert_eq!(c.0, "gsync:abc-123");
        assert_eq!(decode_cursor(Some(&c)).as_deref(), Some("abc-123"));
        // A foreign or empty cursor decodes to None ⇒ full sync.
        assert!(decode_cursor(Some(&Cursor::new("sync:caldav"))).is_none());
        assert!(decode_cursor(Some(&Cursor::new("gsync:"))).is_none());
        assert!(decode_cursor(None).is_none());
    }

    // --- token refresh state machine (fake clock) --------------------------

    fn blob() -> GoogleTokens {
        GoogleTokens {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            access_token: Some("at-old".into()),
            refresh_token: "rt".into(),
            expiry: Some(Utc.with_ymd_and_hms(2026, 7, 2, 12, 0, 0).unwrap()),
        }
    }

    #[test]
    fn needs_refresh_respects_expiry_and_skew() {
        let t = blob();
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
    }

    #[test]
    fn needs_refresh_when_no_token_or_no_expiry() {
        let mut t = blob();
        t.access_token = None;
        assert!(needs_refresh(&t, Utc::now(), 60));
        let mut t = blob();
        t.expiry = None;
        assert!(needs_refresh(&t, Utc::now(), 60));
    }

    #[test]
    fn apply_refresh_updates_token_and_rotates_refresh_token() {
        let mut t = blob();
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
        assert!(
            !needs_refresh(&t, now, 60),
            "fresh token no longer needs refresh"
        );
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
        // Missing expires_in defaults to one hour; missing refresh_token ⇒ None.
        let r = parse_token_response(&json!({ "access_token": "AT" })).unwrap();
        assert_eq!(r.expires_in, 3600);
        assert!(r.refresh_token.is_none());
        assert!(parse_token_response(&json!({})).is_err());
    }

    // --- token store seam + provider construction --------------------------

    /// An in-memory [`GoogleTokenStore`] recording persisted blobs (test double).
    struct FakeTokenStore(Arc<TokioMutex<GoogleTokens>>);

    #[async_trait]
    impl GoogleTokenStore for FakeTokenStore {
        async fn load(&self) -> Result<GoogleTokens> {
            Ok(self.0.lock().await.clone())
        }
        async fn store(&self, tokens: &GoogleTokens) -> Result<()> {
            *self.0.lock().await = tokens.clone();
            Ok(())
        }
    }

    #[test]
    fn from_config_reads_calendar_id_default_primary() {
        let (ws, conn) = (WorkspaceId::new(), ConnectionId::new());
        let store: Arc<dyn GoogleTokenStore> =
            Arc::new(FakeTokenStore(Arc::new(TokioMutex::new(blob()))));
        let p = GoogleCalendarProvider::from_config(ws, conn, &json!({}), store.clone()).unwrap();
        assert_eq!(p.calendar_id(), "primary");
        let p = GoogleCalendarProvider::from_config(
            ws,
            conn,
            &json!({ "calendar": "team@group.calendar.google.com" }),
            store,
        )
        .unwrap();
        assert_eq!(p.calendar_id(), "team@group.calendar.google.com");
        assert!(
            !p.calendar().read_only,
            "write-back makes the calendar writable"
        );
        assert_eq!(p.calendar().external_id, "team@group.calendar.google.com");
    }

    #[tokio::test]
    async fn is_incremental_is_true() {
        let (ws, conn) = (WorkspaceId::new(), ConnectionId::new());
        let store: Arc<dyn GoogleTokenStore> =
            Arc::new(FakeTokenStore(Arc::new(TokioMutex::new(blob()))));
        let p = GoogleCalendarProvider::from_config(ws, conn, &json!({}), store).unwrap();
        assert!(p.is_incremental());
        assert_eq!(p.list_calendars().await.unwrap().len(), 1);
    }

    #[test]
    fn percent_encode_leaves_primary_untouched_and_escapes_at() {
        assert_eq!(percent_encode("primary"), "primary");
        assert_eq!(percent_encode("a@b.com"), "a%40b.com");
    }

    // --- event write-back request shaping -----------------------------------

    #[test]
    fn insert_body_maps_fields_and_omits_unset_keys() {
        let e = NewEvent {
            summary: "Sync".into(),
            start: Utc.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap(),
            all_day: false,
            location: Some("Room 5".into()),
            body: None,
            rrule: Some("FREQ=WEEKLY;BYDAY=FR".into()),
            attendees: vec!["mailto:a@b.com".into(), "c@d.com".into(), "  ".into()],
            labels: Vec::new(),
            attachments: Vec::new(),
        };
        let b = event_write_body(&write_fields_from_new(&e), Some(&e.attendees), false);
        assert_eq!(b["summary"], json!("Sync"));
        assert_eq!(b["start"], json!({ "dateTime": "2026-07-10T09:00:00Z" }));
        assert_eq!(b["end"], json!({ "dateTime": "2026-07-10T10:00:00Z" }));
        assert_eq!(b["location"], json!("Room 5"));
        assert_eq!(b["recurrence"], json!(["RRULE:FREQ=WEEKLY;BYDAY=FR"]));
        // `mailto:` is stripped, blanks dropped.
        assert_eq!(
            b["attendees"],
            json!([{ "email": "a@b.com" }, { "email": "c@d.com" }])
        );
        // Unset fields are OMITTED on insert (no explicit nulls).
        assert!(b.get("description").is_none());
    }

    #[test]
    fn insert_body_all_day_uses_date_values() {
        let e = NewEvent {
            summary: "Holiday".into(),
            start: Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap(),
            all_day: true,
            location: None,
            body: None,
            rrule: None,
            attendees: Vec::new(),
            labels: Vec::new(),
            attachments: Vec::new(),
        };
        let b = event_write_body(&write_fields_from_new(&e), None, false);
        assert_eq!(b["start"], json!({ "date": "2026-07-04" }));
        assert_eq!(b["end"], json!({ "date": "2026-07-05" }));
    }

    #[test]
    fn patch_body_clears_unset_fields_with_explicit_nulls() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "evt-1", "summary": "Solo",
            "start": { "dateTime": "2026-07-10T09:00:00Z" },
            "end": { "dateTime": "2026-07-10T10:00:00Z" }
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        let b = event_write_body(&write_fields_from_event(&e), None, true);
        // Cleared optionals ride as explicit nulls so the patch removes them.
        assert_eq!(b["location"], Value::Null);
        assert_eq!(b["description"], Value::Null);
        assert_eq!(b["recurrence"], Value::Null);
        // The unused endpoint twin is nulled so timed↔all-day flips apply.
        assert_eq!(
            b["start"],
            json!({ "dateTime": "2026-07-10T09:00:00Z", "date": null })
        );
        // No attendees key: the server-side attendee list is preserved.
        assert!(b.get("attendees").is_none());
    }

    // --- push channel request shaping / response parsing -------------------

    #[test]
    fn watch_request_body_shapes_web_hook_with_optional_ttl() {
        let b = watch_request_body(
            "chan-1",
            "https://app/webhooks/google/calendar",
            "tok.sig",
            None,
        );
        assert_eq!(b["id"], json!("chan-1"));
        assert_eq!(b["type"], json!("web_hook"));
        assert_eq!(b["address"], json!("https://app/webhooks/google/calendar"));
        assert_eq!(b["token"], json!("tok.sig"));
        assert!(b.get("params").is_none(), "no ttl ⇒ no params key");
        // A TTL is carried as a string under params (Google's shape); ≤0 is dropped.
        let b = watch_request_body("c", "https://a/w", "t", Some(604_800));
        assert_eq!(b["params"]["ttl"], json!("604800"));
        assert!(watch_request_body("c", "https://a/w", "t", Some(0))
            .get("params")
            .is_none());
    }

    #[test]
    fn stop_request_body_carries_id_and_resource_id() {
        let b = stop_request_body("chan-1", "res-9");
        assert_eq!(b["id"], json!("chan-1"));
        assert_eq!(b["resourceId"], json!("res-9"));
    }

    #[test]
    fn parse_watch_response_reads_ids_and_ms_expiry() {
        let v = json!({
            "kind": "api#channel",
            "id": "chan-1",
            "resourceId": "res-9",
            "resourceUri": "https://www.googleapis.com/calendar/v3/calendars/primary/events",
            "expiration": "1751000000000"
        });
        let w = parse_watch_response(&v, "fallback").unwrap();
        assert_eq!(w.channel_id, "chan-1");
        assert_eq!(w.resource_id, "res-9");
        // 1_751_000_000_000 ms = 1_751_000_000 s.
        assert_eq!(w.expiry, DateTime::from_timestamp(1_751_000_000, 0));
    }

    #[test]
    fn parse_watch_response_falls_back_channel_id_and_tolerates_missing_expiry() {
        // No echoed id ⇒ the one we sent; a numeric expiration is tolerated.
        let v = json!({ "resourceId": "res-9", "expiration": 1_751_000_000_000i64 });
        let w = parse_watch_response(&v, "sent-id").unwrap();
        assert_eq!(w.channel_id, "sent-id");
        assert_eq!(w.resource_id, "res-9");
        assert!(w.expiry.is_some());
        // No expiration ⇒ None (scan renews eagerly).
        let v = json!({ "id": "c", "resourceId": "r" });
        assert!(parse_watch_response(&v, "x").unwrap().expiry.is_none());
        // No resourceId ⇒ error (couldn't stop it later).
        assert!(parse_watch_response(&json!({ "id": "c" }), "x").is_err());
    }
}
