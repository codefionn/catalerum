//! Outlook / Microsoft 365 calendar provider over the Microsoft Graph REST API
//! (SOUL §8).
//!
//! An OAuth2-backed [`CalendarProvider`] for a Microsoft account, mirroring the
//! Google provider's shape: it reads one calendar (the configured id, default:
//! the account's default calendar) via `GET /me/…/events`, mapping Graph's
//! event resource to the core [`Event`] model, and **writes back** via
//! `POST`/`PATCH`/`DELETE` (updates PATCH only the mapped fields so
//! server-side attendees/reminders survive; deletes are idempotent on `404`).
//!
//! ## Auth — Microsoft identity platform v2 (authorization-code + refresh)
//! Tokens live **encrypted at rest** exactly like Google's: the API's
//! `/auth/microsoft/*` web flow performs the code exchange and seals
//! `{client_id, client_secret, tenant, access_token, refresh_token, expiry}`
//! behind the connection's `credential_ref`; the provider reaches the sealed
//! blob through the [`OutlookTokenStore`] seam. Refreshes are single-flight and
//! the rotated blob (Microsoft **rotates the refresh token on every grant**) is
//! persisted back through the seam.
//!
//! ## Sync — full snapshot (SOUL §3.4)
//! Graph's delta queries for events run only over a bounded `calendarView`
//! window, so this first cut syncs the **whole event list** (masters + single
//! instances, paged via `@odata.nextLink`) and stays
//! [`is_incremental` = `false`]: the [`Cursor`] is a deterministic hash over
//! the `(id, changeKey)` set (`osnap:<sha256>`), an unchanged calendar
//! short-circuits to an empty batch, and deletions ride the collect/sync
//! layer's snapshot reconcile — exactly the local-`.ics`/webcal contract.
//! Times are requested in UTC (`Prefer: outlook.timezone="UTC"`).
//!
//! ## Recurrence
//! Graph models recurrence as a structured `patternedRecurrence`, not an
//! `RRULE`. A **common subset** maps bidirectionally
//! ([`recurrence_to_rrule`] / [`rrule_to_recurrence`]): daily/weekly/monthly/
//! yearly patterns with `INTERVAL`, weekly `BYDAY`, monthly `BYMONTHDAY` or
//! ordinal `BYDAY`, and `COUNT`/`UNTIL` ranges. An event whose server-side
//! pattern falls outside the subset still syncs — just without an `rrule`; a
//! local `RRULE` outside the subset fails a write **loudly** rather than
//! silently dropping the recurrence.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use catalerum_core::error::{Error, Result};
use catalerum_core::id::{CalendarId, ConnectionId, WorkspaceId};
use catalerum_core::model::{Calendar, Cursor, Event};
use catalerum_core::provider::{CalendarProvider, NewEvent, SyncBatch};

/// Base for the Microsoft Graph v1.0 API.
const API_BASE: &str = "https://graph.microsoft.com/v1.0";

/// The scopes the connect flow requests: offline access (a refresh token) plus
/// calendar read/write.
pub const OUTLOOK_CALENDAR_SCOPES: &str =
    "offline_access https://graph.microsoft.com/Calendars.ReadWrite";

/// Events per page (`$top`).
const PAGE: usize = 100;

/// Cap on pages a single sync drains (mirrors the Google provider's guard).
const MAX_PAGES: usize = 400;

/// Cap on bytes read from a Graph response body (bounded-read discipline).
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Clock-skew leeway (seconds) before access-token expiry forces a refresh.
const EXPIRY_SKEW_SECS: i64 = 60;

/// Cursor prefix for the snapshot hash.
const CURSOR_PREFIX: &str = "osnap:";

/// The fields a sync `$select`s — everything [`event_from_json`] maps.
const SELECT_FIELDS: &str =
    "id,subject,body,bodyPreview,start,end,isAllDay,location,categories,changeKey,recurrence,type";

/// Microsoft's OAuth2 authorization endpoint for `tenant` (the consent screen).
#[must_use]
pub fn auth_url(tenant: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
        tenant_segment(tenant)
    )
}

/// Microsoft's OAuth2 token endpoint for `tenant`.
#[must_use]
pub fn token_url(tenant: &str) -> String {
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
        tenant_segment(tenant)
    )
}

/// A tenant id/domain for the login URL path; blank falls back to the
/// multi-tenant `common` endpoint.
fn tenant_segment(tenant: &str) -> String {
    let t = tenant.trim();
    if t.is_empty() {
        "common".to_string()
    } else {
        percent_encode(t)
    }
}

/// The OAuth material for one Microsoft connection — sealed (AES-GCM) behind
/// the connection's `credential_ref` and reached through [`OutlookTokenStore`].
/// The client id/secret/tenant ride along so the refresh grant is a single
/// decrypt (the Google blob's exact posture).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutlookTokens {
    /// OAuth client (application) id of the catalerum Microsoft app.
    pub client_id: String,
    /// The confidential-client secret.
    pub client_secret: String,
    /// The Entra tenant the app authenticates against (`common` for
    /// multi-tenant + personal accounts).
    #[serde(default)]
    pub tenant: String,
    /// The current short-lived access token, if one has been minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// The long-lived refresh token (`offline_access`). Required. Microsoft
    /// rotates it on every refresh grant.
    pub refresh_token: String,
    /// When [`access_token`](Self::access_token) expires (absent ⇒ refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry: Option<DateTime<Utc>>,
}

/// The persistence seam for a Microsoft connection's [`OutlookTokens`]
/// (SOUL §13) — the [`GoogleTokenStore`](crate::google::GoogleTokenStore) twin,
/// kept a trait so this crate needs no secret-store / DB dependency.
#[async_trait]
pub trait OutlookTokenStore: Send + Sync {
    /// Decrypt and return the connection's current OAuth material.
    async fn load(&self) -> Result<OutlookTokens>;
    /// Re-seal the (rotated) OAuth material in place.
    async fn store(&self, tokens: &OutlookTokens) -> Result<()>;
}

/// An Outlook / Microsoft 365 [`CalendarProvider`] for one calendar (SOUL §8).
pub struct OutlookCalendarProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    /// A specific Graph calendar id, or `None` for the account's default
    /// calendar (`/me/events`).
    calendar_id: Option<String>,
    tokens: Arc<dyn OutlookTokenStore>,
    http: reqwest::Client,
    /// Cached, decrypted OAuth blob — refreshed under the lock (single-flight).
    cache: tokio::sync::Mutex<Option<OutlookTokens>>,
}

impl std::fmt::Debug for OutlookCalendarProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutlookCalendarProvider")
            .field("workspace_id", &self.workspace_id)
            .field("connection_id", &self.connection_id)
            .field("calendar_id", &self.calendar_id)
            .finish_non_exhaustive()
    }
}

impl OutlookCalendarProvider {
    /// Build from a connection's `config` JSON plus the token seam. The only
    /// config key read is the optional `calendar` (alias `calendar_id`) — a
    /// Graph calendar id; absent ⇒ the account's default calendar. OAuth
    /// material comes from `tokens`, never `config`.
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &Value,
        tokens: Arc<dyn OutlookTokenStore>,
    ) -> Result<Self> {
        let calendar_id = config
            .get("calendar")
            .or_else(|| config.get("calendar_id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "default")
            .map(str::to_string);
        let http = reqwest::Client::builder()
            .user_agent("catalerum-calendar")
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

    /// The configured Graph calendar id (`None` = the default calendar).
    #[must_use]
    pub fn calendar_id(&self) -> Option<&str> {
        self.calendar_id.as_deref()
    }

    /// The single [`Calendar`] this connection represents (multi-calendar
    /// discovery is a deferred enhancement, matching Google). Writable.
    fn calendar(&self) -> Calendar {
        let external_id = self
            .calendar_id
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let id = CalendarId::from_uuid(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("{}/{}", self.connection_id, external_id).as_bytes(),
        ));
        Calendar {
            id,
            workspace_id: self.workspace_id,
            connection_id: Some(self.connection_id),
            external_id: external_id.clone(),
            name: external_id,
            read_only: false,
        }
    }

    /// The events **collection** URL: the configured calendar's, else the
    /// default calendar's.
    fn events_url(&self) -> String {
        match &self.calendar_id {
            Some(id) => format!("{API_BASE}/me/calendars/{}/events", percent_encode(id)),
            None => format!("{API_BASE}/me/events"),
        }
    }

    /// The URL of one event. Event ids are mailbox-unique, so `/me/events/{id}`
    /// addresses the event whichever calendar holds it.
    fn event_url(&self, event_id: &str) -> String {
        format!("{API_BASE}/me/events/{}", percent_encode(event_id))
    }

    /// A valid access token, refreshing (single-flight) when absent/expiring
    /// and persisting the rotated blob — Microsoft rotates the refresh token on
    /// every grant, so persisting is what keeps the connection alive.
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
        let resp = refresh_grant(&self.http, blob).await?;
        apply_refresh(blob, &resp, now);
        self.tokens.store(blob).await?;
        Ok(blob
            .access_token
            .clone()
            .expect("apply_refresh sets the access token"))
    }

    /// GET one JSON page with the UTC-timezone preference applied.
    async fn get_page(&self, token: &str, url: &str) -> Result<Value> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Graph events list: {e}")))?;
        let resp = ensure_success(resp, "events list")?;
        read_json_capped(resp, "Graph events").await
    }

    /// Send `body` as a JSON request via `req` (create/patch).
    async fn send_json(
        &self,
        req: reqwest::RequestBuilder,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let payload = serde_json::to_vec(body)
            .map_err(|e| Error::Provider(format!("encode Graph event body: {e}")))?;
        req.header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("Prefer", "outlook.timezone=\"UTC\"")
            .body(payload)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Graph event write: {e}")))
    }
}

#[async_trait]
impl CalendarProvider for OutlookCalendarProvider {
    async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        Ok(vec![self.calendar()])
    }

    async fn sync(&self, cal: &Calendar, cursor: Option<Cursor>) -> Result<SyncBatch<Event>> {
        let token = self.access_token().await?;
        let first = format!("{}?$top={PAGE}&$select={SELECT_FIELDS}", self.events_url());
        let mut url = first;
        let mut items: Vec<Value> = Vec::new();
        for _ in 0..MAX_PAGES {
            let page = self.get_page(&token, &url).await?;
            if let Some(arr) = page.get("value").and_then(Value::as_array) {
                items.extend(arr.iter().cloned());
            }
            match page.get("@odata.nextLink").and_then(Value::as_str) {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }

        // Snapshot cursor over the (id, changeKey) set: unchanged calendar ⇒
        // same cursor ⇒ empty batch (the webcal short-circuit contract).
        let next_cursor = snapshot_cursor(&items);
        if cursor.as_ref() == Some(&next_cursor) {
            return Ok(SyncBatch {
                upserts: Vec::new(),
                deletions: Vec::new(),
                next_cursor,
                has_more: false,
            });
        }

        let mut upserts = Vec::new();
        for item in &items {
            // `/events` returns series masters and single instances (plus
            // exceptions); a defensive skip for expanded occurrences keeps a
            // master from being shadowed by its instances.
            if item.get("type").and_then(Value::as_str) == Some("occurrence") {
                continue;
            }
            // A single unmappable event must not fail the batch (the shared
            // provider discipline; this crate takes no tracing dependency).
            if let Ok(mut event) = event_from_json(item, self.workspace_id, cal.id) {
                event.calendar_id = cal.id;
                upserts.push(event);
            }
        }

        Ok(SyncBatch {
            upserts,
            deletions: Vec::new(),
            next_cursor,
            has_more: false,
        })
    }

    async fn create_event(&self, cal: &Calendar, event: NewEvent) -> Result<Event> {
        let token = self.access_token().await?;
        let body = event_write_body(
            &WriteFields {
                summary: &event.summary,
                start: event.start,
                end: event.end,
                all_day: event.all_day,
                location: event.location.as_deref(),
                body: event.body.as_deref(),
                rrule: event.rrule.as_deref(),
                labels: &event.labels,
            },
            Some(&event.attendees),
            false,
        )?;
        let url = self.events_url();
        let resp = self
            .send_json(self.http.post(&url).bearer_auth(token), &body)
            .await?;
        let resp = ensure_success(resp, "event create")?;
        let v = read_json_capped(resp, "Graph event create").await?;
        let mut created = event_from_json(&v, self.workspace_id, cal.id)?;
        created.calendar_id = cal.id;
        Ok(created)
    }

    async fn update_event(&self, event: &Event) -> Result<Event> {
        let token = self.access_token().await?;
        let body = event_write_body(
            &WriteFields {
                summary: &event.summary,
                start: event.start,
                end: event.end,
                all_day: event.all_day,
                location: event.location.as_deref(),
                body: event.body.as_deref(),
                rrule: event.rrule.as_deref(),
                labels: &event.labels,
            },
            None,
            true,
        )?;
        let url = self.event_url(&event.uid);
        let mut req = self.http.patch(&url).bearer_auth(token);
        if let Some(etag) = event.etag.as_deref().filter(|s| !s.is_empty()) {
            req = req.header(reqwest::header::IF_MATCH, etag_header_value(etag));
        }
        let resp = self.send_json(req, &body).await?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "event `{}` changed on Outlook (changeKey mismatch) — sync the calendar \
                 and retry the edit",
                event.uid
            )));
        }
        let resp = ensure_success(resp, "event update")?;
        let v = read_json_capped(resp, "Graph event update").await?;
        let mut updated = event_from_json(&v, self.workspace_id, event.calendar_id)?;
        updated.calendar_id = event.calendar_id;
        Ok(updated)
    }

    async fn delete_event(&self, event: &Event) -> Result<()> {
        let token = self.access_token().await?;
        let url = self.event_url(&event.uid);
        let mut req = self.http.delete(&url).bearer_auth(token);
        if let Some(etag) = event.etag.as_deref().filter(|s| !s.is_empty()) {
            req = req.header(reqwest::header::IF_MATCH, etag_header_value(etag));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Provider(format!("Graph event delete: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(()); // already gone — deletion is idempotent
        }
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "event `{}` changed on Outlook (changeKey mismatch) — sync the calendar \
                 and retry the delete",
                event.uid
            )));
        }
        ensure_success(resp, "event delete")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Token refresh — pure decision/mutation helpers (fake-clock tests)
// ---------------------------------------------------------------------------

/// The subset of Microsoft's token response the grants yield.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenResponse {
    pub access_token: String,
    /// Seconds until the access token expires (`expires_in`).
    pub expires_in: i64,
    /// The rotated refresh token — Microsoft returns one on **every** refresh.
    pub refresh_token: Option<String>,
}

/// Whether the cached access token is absent or within `skew_secs` of expiry.
#[must_use]
pub fn needs_refresh(tokens: &OutlookTokens, now: DateTime<Utc>, skew_secs: i64) -> bool {
    match (&tokens.access_token, tokens.expiry) {
        (Some(_), Some(exp)) => exp <= now + chrono::Duration::seconds(skew_secs),
        _ => true,
    }
}

/// Fold a token response into the blob: new access token + expiry, and the
/// rotated refresh token when present.
pub fn apply_refresh(tokens: &mut OutlookTokens, resp: &TokenResponse, now: DateTime<Utc>) {
    tokens.access_token = Some(resp.access_token.clone());
    tokens.expiry = Some(now + chrono::Duration::seconds(resp.expires_in));
    if let Some(rt) = resp.refresh_token.as_ref().filter(|s| !s.is_empty()) {
        tokens.refresh_token = rt.clone();
    }
}

/// Exchange an OAuth **authorization code** at Microsoft's token endpoint,
/// returning a ready-to-seal [`OutlookTokens`] blob. Used by the API's
/// `/auth/microsoft/callback` route. Errors if no `refresh_token` comes back
/// (the consent must include `offline_access`).
pub async fn exchange_code(
    client_id: &str,
    client_secret: &str,
    tenant: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<OutlookTokens> {
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
        ("scope", OUTLOOK_CALENDAR_SCOPES.to_string()),
    ]);
    let v = post_token_form(&http, &token_url(tenant), body, "code exchange").await?;
    let token = parse_token_response(&v)?;
    let refresh_token = token.refresh_token.ok_or_else(|| {
        Error::invalid(
            "Microsoft returned no refresh_token — the consent must include the \
             `offline_access` scope (re-connect and grant it)",
        )
    })?;
    Ok(OutlookTokens {
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        tenant: tenant.trim().to_string(),
        access_token: Some(token.access_token),
        refresh_token,
        expiry: Some(Utc::now() + chrono::Duration::seconds(token.expires_in)),
    })
}

/// Exchange the blob's refresh token for a fresh access token.
async fn refresh_grant(http: &reqwest::Client, blob: &OutlookTokens) -> Result<TokenResponse> {
    let body = encode_query(&[
        ("client_id", blob.client_id.clone()),
        ("client_secret", blob.client_secret.clone()),
        ("refresh_token", blob.refresh_token.clone()),
        ("grant_type", "refresh_token".to_string()),
        ("scope", OUTLOOK_CALENDAR_SCOPES.to_string()),
    ]);
    let v = post_token_form(http, &token_url(&blob.tenant), body, "token refresh").await?;
    parse_token_response(&v)
}

/// POST a form to the token endpoint, mapping 400/401 to a clear
/// revoked/invalid error.
async fn post_token_form(
    http: &reqwest::Client,
    url: &str,
    body: String,
    what: &str,
) -> Result<Value> {
    let resp = http
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .map_err(|e| Error::Provider(format!("Microsoft {what}: {e}")))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED
        || resp.status() == reqwest::StatusCode::BAD_REQUEST
    {
        return Err(Error::unauthorized(format!(
            "Microsoft {what} rejected (revoked or invalid credentials / bad redirect_uri)"
        )));
    }
    let resp = ensure_success(resp, what)?;
    read_json_capped(resp, "Microsoft token").await
}

/// Parse Microsoft's token endpoint JSON. Pure (testable).
fn parse_token_response(v: &Value) -> Result<TokenResponse> {
    let access_token = v
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::provider("Microsoft token response has no access_token"))?;
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

/// Map one Graph event resource to a core [`Event`]. Keyed by the Graph event
/// `id` (mailbox-unique — also how `PATCH`/`DELETE` address it); the
/// `changeKey` rides as the ETag (see [`etag_header_value`] for the `If-Match`
/// form). Body: a `text` body's content verbatim; an `html` body degrades to
/// the plain-text `bodyPreview` (catalerum stores text). Pure.
fn event_from_json(
    item: &Value,
    workspace_id: WorkspaceId,
    calendar_id: CalendarId,
) -> Result<Event> {
    let uid = item
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::provider("Graph event has no id"))?
        .to_string();
    let all_day = item
        .get("isAllDay")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let start = parse_endpoint(item.get("start"))
        .ok_or_else(|| Error::provider("Graph event has no parseable start"))?;
    let end = parse_endpoint(item.get("end")).unwrap_or_else(|| {
        if all_day {
            start + chrono::Duration::days(1)
        } else {
            start
        }
    });
    let body = match (
        item.pointer("/body/contentType").and_then(Value::as_str),
        item.pointer("/body/content").and_then(Value::as_str),
    ) {
        (Some(ct), Some(content)) if ct.eq_ignore_ascii_case("text") => {
            Some(content.trim().to_string()).filter(|s| !s.is_empty())
        }
        _ => opt_string(item.get("bodyPreview")),
    };
    Ok(Event {
        id: catalerum_core::id::EventId::new(),
        workspace_id,
        calendar_id,
        uid,
        start,
        end,
        all_day,
        rrule: item.get("recurrence").and_then(recurrence_to_rrule),
        summary: item
            .get("subject")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        location: item
            .pointer("/location/displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        attendees: Vec::new(),
        body,
        labels: item
            .get("categories")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        attachments: Vec::new(),
        etag: opt_string(item.get("changeKey")),
        sequence: 0,
    })
}

/// Resolve a Graph `start`/`end` endpoint (`{dateTime, timeZone}`) to a UTC
/// instant. Sync requests `Prefer: outlook.timezone="UTC"`, so anything other
/// than UTC is unexpected — a non-UTC zone is still accepted and read as UTC
/// (the value Graph sends under the preference) rather than dropped.
fn parse_endpoint(endpoint: Option<&Value>) -> Option<DateTime<Utc>> {
    let dt = endpoint?.get("dateTime").and_then(Value::as_str)?;
    // Graph emits fractional seconds ("2026-07-10T09:00:00.0000000").
    NaiveDateTime::parse_from_str(dt.trim(), "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|naive| Utc.from_utc_datetime(&naive))
}

fn opt_string(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Deterministic snapshot [`Cursor`] over the returned `(id, changeKey)` set —
/// unchanged server state hashes the same, so re-sync is idempotent
/// (SOUL §3.4; the CalDAV `etagset:` twin).
fn snapshot_cursor(items: &[Value]) -> Cursor {
    let mut pairs: Vec<String> = items
        .iter()
        .map(|i| {
            format!(
                "{}={}",
                i.get("id").and_then(Value::as_str).unwrap_or(""),
                i.get("changeKey").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    pairs.sort();
    let mut hasher = Sha256::new();
    hasher.update(pairs.join("\n").as_bytes());
    Cursor::new(format!("{CURSOR_PREFIX}{:x}", hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Recurrence — Graph patternedRecurrence ⇄ RRULE (common subset, pure)
// ---------------------------------------------------------------------------

const WEEKDAYS: &[(&str, &str)] = &[
    ("monday", "MO"),
    ("tuesday", "TU"),
    ("wednesday", "WE"),
    ("thursday", "TH"),
    ("friday", "FR"),
    ("saturday", "SA"),
    ("sunday", "SU"),
];

fn day_to_byday(day: &str) -> Option<&'static str> {
    WEEKDAYS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(day.trim()))
        .map(|(_, code)| *code)
}

fn byday_to_day(code: &str) -> Option<&'static str> {
    WEEKDAYS
        .iter()
        .find(|(_, c)| c.eq_ignore_ascii_case(code.trim()))
        .map(|(name, _)| *name)
}

fn index_to_ordinal(index: &str) -> Option<i32> {
    match index.trim().to_ascii_lowercase().as_str() {
        "first" => Some(1),
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "last" => Some(-1),
        _ => None,
    }
}

fn ordinal_to_index(ord: i32) -> Option<&'static str> {
    match ord {
        1 => Some("first"),
        2 => Some("second"),
        3 => Some("third"),
        4 => Some("fourth"),
        -1 => Some("last"),
        _ => None,
    }
}

/// Map a Graph `patternedRecurrence` to an `RRULE` (without the prefix), for
/// the **common subset**; `None` when the pattern falls outside it (the event
/// still syncs, recurrence-less). Pure — the fixture test target.
fn recurrence_to_rrule(recurrence: &Value) -> Option<String> {
    let pattern = recurrence.get("pattern")?;
    let ptype = pattern.get("type").and_then(Value::as_str)?;
    let interval = pattern.get("interval").and_then(Value::as_i64).unwrap_or(1);

    let mut parts: Vec<String> = Vec::new();
    match ptype {
        "daily" => parts.push("FREQ=DAILY".into()),
        "weekly" => {
            parts.push("FREQ=WEEKLY".into());
            let days: Vec<&str> = pattern
                .get("daysOfWeek")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .filter_map(day_to_byday)
                        .collect()
                })
                .unwrap_or_default();
            if !days.is_empty() {
                parts.push(format!("BYDAY={}", days.join(",")));
            }
        }
        "absoluteMonthly" => {
            parts.push("FREQ=MONTHLY".into());
            let dom = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            parts.push(format!("BYMONTHDAY={dom}"));
        }
        "relativeMonthly" => {
            parts.push("FREQ=MONTHLY".into());
            let ord = index_to_ordinal(
                pattern
                    .get("index")
                    .and_then(Value::as_str)
                    .unwrap_or("first"),
            )?;
            let day = pattern
                .get("daysOfWeek")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .and_then(day_to_byday)?;
            parts.push(format!("BYDAY={ord}{day}"));
        }
        "absoluteYearly" => {
            parts.push("FREQ=YEARLY".into());
            let month = pattern.get("month").and_then(Value::as_i64)?;
            let dom = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            parts.push(format!("BYMONTH={month}"));
            parts.push(format!("BYMONTHDAY={dom}"));
        }
        "relativeYearly" => {
            parts.push("FREQ=YEARLY".into());
            let month = pattern.get("month").and_then(Value::as_i64)?;
            let ord = index_to_ordinal(
                pattern
                    .get("index")
                    .and_then(Value::as_str)
                    .unwrap_or("first"),
            )?;
            let day = pattern
                .get("daysOfWeek")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .and_then(day_to_byday)?;
            parts.push(format!("BYMONTH={month}"));
            parts.push(format!("BYDAY={ord}{day}"));
        }
        _ => return None,
    }
    if interval > 1 {
        parts.push(format!("INTERVAL={interval}"));
    }
    match recurrence
        .pointer("/range/type")
        .and_then(Value::as_str)
        .unwrap_or("noEnd")
    {
        "noEnd" => {}
        "numbered" => {
            let n = recurrence
                .pointer("/range/numberOfOccurrences")
                .and_then(Value::as_i64)?;
            parts.push(format!("COUNT={n}"));
        }
        "endDate" => {
            let d = recurrence
                .pointer("/range/endDate")
                .and_then(Value::as_str)?;
            let date = NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").ok()?;
            parts.push(format!("UNTIL={}", date.format("%Y%m%d")));
        }
        _ => return None,
    }
    Some(parts.join(";"))
}

/// Map an `RRULE` (common subset) to a Graph `patternedRecurrence`. `start`
/// anchors `range.startDate` and fills weekly/monthly defaults from the
/// event's own date. An unsupported rule **errors** (a silent drop would write
/// a one-off event where the user asked for a series). Pure.
fn rrule_to_recurrence(rrule: &str, start: DateTime<Utc>) -> Result<Value> {
    let mut freq = None;
    let mut interval: i64 = 1;
    let mut byday: Vec<String> = Vec::new();
    let mut bymonthday: Option<i64> = None;
    let mut bymonth: Option<i64> = None;
    let mut count: Option<i64> = None;
    let mut until: Option<NaiveDate> = None;

    for part in rrule.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (key, val) = part
            .split_once('=')
            .ok_or_else(|| Error::invalid(format!("malformed RRULE part `{part}`")))?;
        match key.to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(val.to_ascii_uppercase()),
            "INTERVAL" => {
                interval = val
                    .parse()
                    .map_err(|_| Error::invalid(format!("bad RRULE INTERVAL `{val}`")))?;
            }
            "BYDAY" => byday = val.split(',').map(|s| s.trim().to_string()).collect(),
            "BYMONTHDAY" => {
                let v: i64 = val
                    .parse()
                    .map_err(|_| Error::invalid(format!("bad RRULE BYMONTHDAY `{val}`")))?;
                bymonthday = Some(v);
            }
            "BYMONTH" => {
                let v: i64 = val
                    .parse()
                    .map_err(|_| Error::invalid(format!("bad RRULE BYMONTH `{val}`")))?;
                bymonth = Some(v);
            }
            "COUNT" => {
                let v: i64 = val
                    .parse()
                    .map_err(|_| Error::invalid(format!("bad RRULE COUNT `{val}`")))?;
                count = Some(v);
            }
            "UNTIL" => {
                let date_part = val.split('T').next().unwrap_or(val);
                until = Some(
                    NaiveDate::parse_from_str(date_part, "%Y%m%d")
                        .map_err(|_| Error::invalid(format!("bad RRULE UNTIL `{val}`")))?,
                );
            }
            "WKST" => {} // irrelevant to the mapped subset; tolerated
            other => {
                return Err(Error::Unsupported(format!(
                    "RRULE key `{other}` cannot be written to an Outlook calendar \
                     (the Graph recurrence mapping covers FREQ/INTERVAL/BYDAY/\
                     BYMONTHDAY/BYMONTH/COUNT/UNTIL)"
                )));
            }
        }
    }

    let freq = freq.ok_or_else(|| Error::invalid("RRULE has no FREQ"))?;
    // Split ordinal-prefixed BYDAY ("2TU", "-1FR") from plain day codes.
    let parse_byday = |d: &str| -> Result<(Option<i32>, String)> {
        let trimmed = d.trim();
        let split = trimmed.len().saturating_sub(2);
        let (ord, day) = trimmed.split_at(split);
        let day_name =
            byday_to_day(day).ok_or_else(|| Error::invalid(format!("bad RRULE BYDAY `{d}`")))?;
        if ord.is_empty() {
            Ok((None, day_name.to_string()))
        } else {
            let n: i32 = ord
                .parse()
                .map_err(|_| Error::invalid(format!("bad RRULE BYDAY ordinal `{d}`")))?;
            Ok((Some(n), day_name.to_string()))
        }
    };

    let pattern = match freq.as_str() {
        "DAILY" => serde_json::json!({ "type": "daily", "interval": interval }),
        "WEEKLY" => {
            let days: Vec<String> = if byday.is_empty() {
                vec![weekday_name(start)]
            } else {
                byday
                    .iter()
                    .map(|d| parse_byday(d).map(|(_, day)| day))
                    .collect::<Result<_>>()?
            };
            serde_json::json!({ "type": "weekly", "interval": interval, "daysOfWeek": days })
        }
        "MONTHLY" => match (&bymonthday, byday.first()) {
            (Some(dom), _) => serde_json::json!({
                "type": "absoluteMonthly", "interval": interval, "dayOfMonth": dom
            }),
            (None, Some(d)) => {
                let (ord, day) = parse_byday(d)?;
                let index = ordinal_to_index(ord.unwrap_or(1)).ok_or_else(|| {
                    Error::Unsupported(format!(
                        "RRULE BYDAY ordinal `{d}` has no Outlook equivalent (first…fourth/last)"
                    ))
                })?;
                serde_json::json!({
                    "type": "relativeMonthly", "interval": interval,
                    "index": index, "daysOfWeek": [day]
                })
            }
            (None, None) => serde_json::json!({
                "type": "absoluteMonthly", "interval": interval, "dayOfMonth": start.day()
            }),
        },
        "YEARLY" => {
            let month = bymonth.unwrap_or_else(|| i64::from(start.month()));
            match byday.first() {
                Some(d) => {
                    let (ord, day) = parse_byday(d)?;
                    let index = ordinal_to_index(ord.unwrap_or(1)).ok_or_else(|| {
                        Error::Unsupported(format!(
                            "RRULE BYDAY ordinal `{d}` has no Outlook equivalent"
                        ))
                    })?;
                    serde_json::json!({
                        "type": "relativeYearly", "interval": interval, "month": month,
                        "index": index, "daysOfWeek": [day]
                    })
                }
                None => serde_json::json!({
                    "type": "absoluteYearly", "interval": interval, "month": month,
                    "dayOfMonth": bymonthday.unwrap_or_else(|| i64::from(start.day()))
                }),
            }
        }
        other => {
            return Err(Error::Unsupported(format!(
                "RRULE FREQ={other} cannot be written to an Outlook calendar \
                 (daily/weekly/monthly/yearly only)"
            )));
        }
    };

    let range = match (count, until) {
        (Some(n), _) => serde_json::json!({
            "type": "numbered",
            "startDate": start.date_naive().format("%Y-%m-%d").to_string(),
            "numberOfOccurrences": n
        }),
        (None, Some(d)) => serde_json::json!({
            "type": "endDate",
            "startDate": start.date_naive().format("%Y-%m-%d").to_string(),
            "endDate": d.format("%Y-%m-%d").to_string()
        }),
        (None, None) => serde_json::json!({
            "type": "noEnd",
            "startDate": start.date_naive().format("%Y-%m-%d").to_string()
        }),
    };

    Ok(serde_json::json!({ "pattern": pattern, "range": range }))
}

/// The Graph day-of-week name of a UTC instant's weekday.
fn weekday_name(dt: DateTime<Utc>) -> String {
    match dt.weekday() {
        chrono::Weekday::Mon => "monday",
        chrono::Weekday::Tue => "tuesday",
        chrono::Weekday::Wed => "wednesday",
        chrono::Weekday::Thu => "thursday",
        chrono::Weekday::Fri => "friday",
        chrono::Weekday::Sat => "saturday",
        chrono::Weekday::Sun => "sunday",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Event write-back — pure request shaping (fixture-tested)
// ---------------------------------------------------------------------------

/// The provider-writable fields, shared by the create/update shaping paths.
struct WriteFields<'a> {
    summary: &'a str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    all_day: bool,
    location: Option<&'a str>,
    body: Option<&'a str>,
    rrule: Option<&'a str>,
    labels: &'a [String],
}

/// Shape a Graph `{dateTime, timeZone}` endpoint (UTC; all-day events carry
/// their date's midnight, which Graph requires alongside `isAllDay`).
fn endpoint_value(dt: DateTime<Utc>, all_day: bool) -> Value {
    let formatted = if all_day {
        format!("{}T00:00:00", dt.date_naive().format("%Y-%m-%d"))
    } else {
        dt.format("%Y-%m-%dT%H:%M:%S").to_string()
    };
    serde_json::json!({ "dateTime": formatted, "timeZone": "UTC" })
}

/// Shape the Graph event resource for create (`patch = false`) or `PATCH`
/// (`patch = true`). On a patch, cleared optionals ride as explicit `null`
/// (Graph's clear-a-field semantics); on create they are omitted. `categories`
/// (labels) are written on both paths — the local list is authoritative, it
/// came from the server's at sync time. `attendees` only on create (the stored
/// event holds entity pointers, not addresses), so a patch preserves the
/// server-side list. Errors when the `RRULE` falls outside the mapped subset.
fn event_write_body(
    f: &WriteFields<'_>,
    attendees: Option<&[String]>,
    patch: bool,
) -> Result<Value> {
    let mut body = serde_json::Map::new();
    body.insert("subject".to_string(), Value::String(f.summary.to_string()));
    body.insert("start".to_string(), endpoint_value(f.start, f.all_day));
    body.insert("end".to_string(), endpoint_value(f.end, f.all_day));
    body.insert("isAllDay".to_string(), Value::Bool(f.all_day));
    match f.location.map(str::trim).filter(|s| !s.is_empty()) {
        Some(loc) => {
            body.insert(
                "location".to_string(),
                serde_json::json!({ "displayName": loc }),
            );
        }
        None if patch => {
            body.insert("location".to_string(), Value::Null);
        }
        None => {}
    }
    match f.body.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => {
            body.insert(
                "body".to_string(),
                serde_json::json!({ "contentType": "text", "content": text }),
            );
        }
        None if patch => {
            body.insert(
                "body".to_string(),
                serde_json::json!({ "contentType": "text", "content": "" }),
            );
        }
        None => {}
    }
    body.insert(
        "categories".to_string(),
        Value::Array(f.labels.iter().map(|l| Value::String(l.clone())).collect()),
    );
    match f.rrule.map(str::trim).filter(|s| !s.is_empty()) {
        Some(rrule) => {
            body.insert(
                "recurrence".to_string(),
                rrule_to_recurrence(rrule, f.start)?,
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
                (!email.is_empty()).then(|| {
                    serde_json::json!({
                        "emailAddress": { "address": email }, "type": "required"
                    })
                })
            })
            .collect();
        if !entries.is_empty() {
            body.insert("attendees".to_string(), Value::Array(entries));
        }
    }
    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// HTTP helpers (the Google provider's bounded-read discipline)
// ---------------------------------------------------------------------------

/// Shape a stored changeKey for an `If-Match` header: Graph's ETags are weak
/// (`W/"…"`); a bare changeKey gains that wrapper, an already-shaped value
/// passes verbatim.
fn etag_header_value(etag: &str) -> String {
    let t = etag.trim();
    if t.starts_with("W/") || t.starts_with('"') {
        t.to_string()
    } else {
        format!("W/\"{t}\"")
    }
}

fn ensure_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::unauthorized(format!(
            "Microsoft Graph {what} returned {status}"
        )));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::NotFound);
    }
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "Microsoft Graph {what} returned {status}"
        )));
    }
    Ok(resp)
}

/// Read a response body as JSON, capped at [`MAX_RESPONSE_BYTES`].
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

/// `application/x-www-form-urlencoded` encoding (hand-rolled: this reqwest
/// build gates `.form()`/`.query()`).
fn encode_query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode outside the URL-unreserved set.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::Mutex as TokioMutex;

    fn ids() -> (WorkspaceId, CalendarId) {
        (WorkspaceId::new(), CalendarId::new())
    }

    // --- event mapping -------------------------------------------------------

    #[test]
    fn maps_timed_event_with_text_body_categories_and_changekey() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "AAMkAD-1",
            "subject": "Weekly sync",
            "bodyPreview": "preview text",
            "body": { "contentType": "text", "content": "full agenda\n" },
            "start": { "dateTime": "2026-07-10T09:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-07-10T10:00:00.0000000", "timeZone": "UTC" },
            "isAllDay": false,
            "location": { "displayName": "Room 5" },
            "categories": ["Work", " team "],
            "changeKey": "CQAAABYA",
            "type": "singleInstance"
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert_eq!(e.uid, "AAMkAD-1");
        assert_eq!(e.summary, "Weekly sync");
        assert_eq!(e.body.as_deref(), Some("full agenda"));
        assert_eq!(e.location.as_deref(), Some("Room 5"));
        assert_eq!(e.labels, vec!["Work".to_string(), "team".to_string()]);
        assert_eq!(e.etag.as_deref(), Some("CQAAABYA"));
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap());
        assert!(!e.all_day);
        assert!(e.rrule.is_none());
    }

    #[test]
    fn html_body_degrades_to_body_preview() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "x", "subject": "s",
            "bodyPreview": "plain preview",
            "body": { "contentType": "html", "content": "<p>hi</p>" },
            "start": { "dateTime": "2026-07-10T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-07-10T10:00:00", "timeZone": "UTC" }
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert_eq!(e.body.as_deref(), Some("plain preview"));
    }

    #[test]
    fn maps_all_day_event() {
        let (ws, cal) = ids();
        let item = json!({
            "id": "d", "subject": "Holiday", "isAllDay": true,
            "start": { "dateTime": "2026-07-04T00:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-07-05T00:00:00.0000000", "timeZone": "UTC" }
        });
        let e = event_from_json(&item, ws, cal).unwrap();
        assert!(e.all_day);
        assert_eq!(e.start, Utc.with_ymd_and_hms(2026, 7, 4, 0, 0, 0).unwrap());
        assert_eq!(e.end, Utc.with_ymd_and_hms(2026, 7, 5, 0, 0, 0).unwrap());
    }

    // --- snapshot cursor -----------------------------------------------------

    #[test]
    fn snapshot_cursor_is_stable_and_order_independent() {
        let a = vec![
            json!({ "id": "1", "changeKey": "a" }),
            json!({ "id": "2", "changeKey": "b" }),
        ];
        let b = vec![
            json!({ "id": "2", "changeKey": "b" }),
            json!({ "id": "1", "changeKey": "a" }),
        ];
        assert_eq!(snapshot_cursor(&a), snapshot_cursor(&b));
        assert!(snapshot_cursor(&a).0.starts_with(CURSOR_PREFIX));
        // A changed changeKey changes the cursor.
        let c = vec![
            json!({ "id": "1", "changeKey": "a2" }),
            json!({ "id": "2", "changeKey": "b" }),
        ];
        assert_ne!(snapshot_cursor(&a), snapshot_cursor(&c));
    }

    // --- recurrence mapping --------------------------------------------------

    #[test]
    fn graph_weekly_pattern_maps_to_rrule_and_back() {
        let rec = json!({
            "pattern": { "type": "weekly", "interval": 2, "daysOfWeek": ["monday", "thursday"] },
            "range": { "type": "numbered", "startDate": "2026-07-06", "numberOfOccurrences": 10 }
        });
        let rrule = recurrence_to_rrule(&rec).unwrap();
        assert_eq!(rrule, "FREQ=WEEKLY;BYDAY=MO,TH;INTERVAL=2;COUNT=10");

        let start = Utc.with_ymd_and_hms(2026, 7, 6, 9, 0, 0).unwrap();
        let round = rrule_to_recurrence(&rrule, start).unwrap();
        assert_eq!(round["pattern"]["type"], json!("weekly"));
        assert_eq!(round["pattern"]["interval"], json!(2));
        assert_eq!(
            round["pattern"]["daysOfWeek"],
            json!(["monday", "thursday"])
        );
        assert_eq!(round["range"]["type"], json!("numbered"));
        assert_eq!(round["range"]["numberOfOccurrences"], json!(10));
        assert_eq!(round["range"]["startDate"], json!("2026-07-06"));
    }

    #[test]
    fn graph_relative_monthly_maps_to_ordinal_byday() {
        let rec = json!({
            "pattern": { "type": "relativeMonthly", "interval": 1,
                         "index": "second", "daysOfWeek": ["tuesday"] },
            "range": { "type": "endDate", "startDate": "2026-01-01", "endDate": "2026-12-31" }
        });
        assert_eq!(
            recurrence_to_rrule(&rec).unwrap(),
            "FREQ=MONTHLY;BYDAY=2TU;UNTIL=20261231"
        );
        let start = Utc.with_ymd_and_hms(2026, 1, 13, 9, 0, 0).unwrap();
        let round = rrule_to_recurrence("FREQ=MONTHLY;BYDAY=2TU;UNTIL=20261231", start).unwrap();
        assert_eq!(round["pattern"]["type"], json!("relativeMonthly"));
        assert_eq!(round["pattern"]["index"], json!("second"));
        assert_eq!(round["pattern"]["daysOfWeek"], json!(["tuesday"]));
        assert_eq!(round["range"]["endDate"], json!("2026-12-31"));
    }

    #[test]
    fn daily_yearly_and_defaults_round_trip() {
        let start = Utc.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap(); // a Friday
        let daily = rrule_to_recurrence("FREQ=DAILY", start).unwrap();
        assert_eq!(daily["pattern"]["type"], json!("daily"));
        assert_eq!(daily["range"]["type"], json!("noEnd"));

        // Weekly with no BYDAY defaults to the start's weekday.
        let weekly = rrule_to_recurrence("FREQ=WEEKLY", start).unwrap();
        assert_eq!(weekly["pattern"]["daysOfWeek"], json!(["friday"]));

        // Monthly with neither BYMONTHDAY nor BYDAY pins the start's day.
        let monthly = rrule_to_recurrence("FREQ=MONTHLY", start).unwrap();
        assert_eq!(monthly["pattern"]["type"], json!("absoluteMonthly"));
        assert_eq!(monthly["pattern"]["dayOfMonth"], json!(10));

        let yearly = rrule_to_recurrence("FREQ=YEARLY", start).unwrap();
        assert_eq!(yearly["pattern"]["type"], json!("absoluteYearly"));
        assert_eq!(yearly["pattern"]["month"], json!(7));
        assert_eq!(yearly["pattern"]["dayOfMonth"], json!(10));

        // The absoluteYearly shape round-trips back to an RRULE.
        let rec = json!({
            "pattern": { "type": "absoluteYearly", "interval": 1, "month": 7, "dayOfMonth": 10 },
            "range": { "type": "noEnd", "startDate": "2026-07-10" }
        });
        assert_eq!(
            recurrence_to_rrule(&rec).unwrap(),
            "FREQ=YEARLY;BYMONTH=7;BYMONTHDAY=10"
        );
    }

    #[test]
    fn unsupported_rrules_fail_loudly_not_silently() {
        let start = Utc::now();
        assert!(matches!(
            rrule_to_recurrence("FREQ=HOURLY", start),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            rrule_to_recurrence("FREQ=MONTHLY;BYSETPOS=2", start),
            Err(Error::Unsupported(_))
        ));
        // An out-of-range Graph pattern reads back as None (event still syncs).
        assert!(recurrence_to_rrule(&json!({
            "pattern": { "type": "weirdType" }, "range": { "type": "noEnd" }
        }))
        .is_none());
    }

    // --- write-body shaping ----------------------------------------------------

    fn fields(all_day: bool) -> (DateTime<Utc>, DateTime<Utc>) {
        let start = Utc.with_ymd_and_hms(2026, 7, 10, 9, 0, 0).unwrap();
        let end = if all_day {
            Utc.with_ymd_and_hms(2026, 7, 11, 0, 0, 0).unwrap()
        } else {
            Utc.with_ymd_and_hms(2026, 7, 10, 10, 0, 0).unwrap()
        };
        (start, end)
    }

    #[test]
    fn create_body_maps_fields_and_attendees() {
        let (start, end) = fields(false);
        let labels = vec!["Work".to_string()];
        let b = event_write_body(
            &WriteFields {
                summary: "Sync",
                start,
                end,
                all_day: false,
                location: Some("Room 5"),
                body: Some("agenda"),
                rrule: None,
                labels: &labels,
            },
            Some(&["mailto:a@b.com".to_string(), "c@d.com".to_string()]),
            false,
        )
        .unwrap();
        assert_eq!(b["subject"], json!("Sync"));
        assert_eq!(
            b["start"],
            json!({ "dateTime": "2026-07-10T09:00:00", "timeZone": "UTC" })
        );
        assert_eq!(b["isAllDay"], json!(false));
        assert_eq!(b["location"], json!({ "displayName": "Room 5" }));
        assert_eq!(
            b["body"],
            json!({ "contentType": "text", "content": "agenda" })
        );
        assert_eq!(b["categories"], json!(["Work"]));
        assert_eq!(
            b["attendees"],
            json!([
                { "emailAddress": { "address": "a@b.com" }, "type": "required" },
                { "emailAddress": { "address": "c@d.com" }, "type": "required" }
            ])
        );
        assert!(b.get("recurrence").is_none(), "unset ⇒ omitted on create");
    }

    #[test]
    fn patch_body_clears_unset_fields_and_omits_attendees() {
        let (start, end) = fields(true);
        let b = event_write_body(
            &WriteFields {
                summary: "Holiday",
                start,
                end,
                all_day: true,
                location: None,
                body: None,
                rrule: None,
                labels: &[],
            },
            None,
            true,
        )
        .unwrap();
        assert_eq!(b["isAllDay"], json!(true));
        assert_eq!(
            b["start"],
            json!({ "dateTime": "2026-07-10T00:00:00", "timeZone": "UTC" })
        );
        assert_eq!(b["location"], Value::Null);
        assert_eq!(b["recurrence"], Value::Null);
        assert_eq!(b["categories"], json!([]));
        assert!(b.get("attendees").is_none(), "patch preserves attendees");
    }

    #[test]
    fn unsupported_rrule_fails_the_write_body() {
        let (start, end) = fields(false);
        assert!(matches!(
            event_write_body(
                &WriteFields {
                    summary: "s",
                    start,
                    end,
                    all_day: false,
                    location: None,
                    body: None,
                    rrule: Some("FREQ=MINUTELY"),
                    labels: &[],
                },
                None,
                false,
            ),
            Err(Error::Unsupported(_))
        ));
    }

    // --- token machinery -------------------------------------------------------

    fn blob() -> OutlookTokens {
        OutlookTokens {
            client_id: "cid".into(),
            client_secret: "secret".into(),
            tenant: "common".into(),
            access_token: Some("at-old".into()),
            refresh_token: "rt".into(),
            expiry: Some(Utc.with_ymd_and_hms(2026, 7, 7, 12, 0, 0).unwrap()),
        }
    }

    #[test]
    fn needs_refresh_respects_expiry_skew_and_absence() {
        let t = blob();
        assert!(!needs_refresh(
            &t,
            Utc.with_ymd_and_hms(2026, 7, 7, 11, 0, 0).unwrap(),
            60
        ));
        assert!(needs_refresh(
            &t,
            Utc.with_ymd_and_hms(2026, 7, 7, 11, 59, 30).unwrap(),
            60
        ));
        let mut t = blob();
        t.access_token = None;
        assert!(needs_refresh(&t, Utc::now(), 60));
    }

    #[test]
    fn apply_refresh_always_keeps_the_rotated_refresh_token() {
        let mut t = blob();
        let now = Utc.with_ymd_and_hms(2026, 7, 7, 13, 0, 0).unwrap();
        // Microsoft rotates the refresh token on every grant.
        apply_refresh(
            &mut t,
            &TokenResponse {
                access_token: "at-new".into(),
                expires_in: 3600,
                refresh_token: Some("rt-2".into()),
            },
            now,
        );
        assert_eq!(t.access_token.as_deref(), Some("at-new"));
        assert_eq!(t.refresh_token, "rt-2");
        assert!(!needs_refresh(&t, now, 60));
    }

    #[test]
    fn parse_token_response_reads_fields() {
        let v = json!({ "access_token": "AT", "expires_in": 1800, "refresh_token": "RT" });
        let r = parse_token_response(&v).unwrap();
        assert_eq!((r.access_token.as_str(), r.expires_in), ("AT", 1800));
        assert_eq!(r.refresh_token.as_deref(), Some("RT"));
        assert!(parse_token_response(&json!({})).is_err());
    }

    // --- endpoints + construction ----------------------------------------------

    #[test]
    fn auth_and_token_urls_are_tenant_scoped_with_common_fallback() {
        assert_eq!(
            auth_url("contoso.com"),
            "https://login.microsoftonline.com/contoso.com/oauth2/v2.0/authorize"
        );
        assert_eq!(
            token_url(""),
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
    }

    struct FakeTokenStore(Arc<TokioMutex<OutlookTokens>>);

    #[async_trait]
    impl OutlookTokenStore for FakeTokenStore {
        async fn load(&self) -> Result<OutlookTokens> {
            Ok(self.0.lock().await.clone())
        }
        async fn store(&self, tokens: &OutlookTokens) -> Result<()> {
            *self.0.lock().await = tokens.clone();
            Ok(())
        }
    }

    #[tokio::test]
    async fn from_config_reads_calendar_id_and_builds_urls() {
        let (ws, conn) = (WorkspaceId::new(), ConnectionId::new());
        let store: Arc<dyn OutlookTokenStore> =
            Arc::new(FakeTokenStore(Arc::new(TokioMutex::new(blob()))));

        let p = OutlookCalendarProvider::from_config(ws, conn, &json!({}), store.clone()).unwrap();
        assert!(p.calendar_id().is_none());
        assert_eq!(p.events_url(), format!("{API_BASE}/me/events"));
        assert_eq!(p.calendar().external_id, "default");
        assert!(!p.calendar().read_only);
        assert!(!p.is_incremental(), "snapshot sync");
        assert_eq!(p.list_calendars().await.unwrap().len(), 1);

        let p = OutlookCalendarProvider::from_config(
            ws,
            conn,
            &json!({ "calendar": "AAMkCal=" }),
            store,
        )
        .unwrap();
        assert_eq!(p.calendar_id(), Some("AAMkCal="));
        assert_eq!(
            p.events_url(),
            format!("{API_BASE}/me/calendars/AAMkCal%3D/events")
        );
        assert_eq!(p.event_url("id/1"), format!("{API_BASE}/me/events/id%2F1"));
    }

    #[test]
    fn etag_header_wraps_bare_change_keys_as_weak() {
        assert_eq!(etag_header_value("CQAAABYA"), "W/\"CQAAABYA\"");
        assert_eq!(etag_header_value("W/\"x\""), "W/\"x\"");
        assert_eq!(etag_header_value("\"x\""), "\"x\"");
    }
}
