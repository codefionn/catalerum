//! CalDAV / webcal calendar provider over HTTP (SOUL §8).
//!
//! Two modes, chosen by the connection's base URL scheme:
//!
//! - **CalDAV** (`http(s)://…`, RFC 4791): read-write-capable*. Discovery via
//!   `PROPFIND`; incremental [`sync`](CalDavProvider::sync) via a
//!   `sync-collection` `REPORT` (RFC 6578) when a `sync-token` cursor is held,
//!   else an initial `calendar-query` `REPORT` by time-range. Each resource
//!   carries an `ETag` and an embedded `VEVENT`. The next cursor is the
//!   returned `sync-token` (or, lacking server sync support, a synthetic
//!   ETag-set hash so re-sync stays idempotent).
//! - **webcal** (`webcal://…` / `?ics`): a read-only `GET` of a single ICS
//!   document (Internet Calendar Subscription). The cursor is the response
//!   `ETag`/`Last-Modified`, falling back to a content hash.
//!
//! \* **Write-back** (CalDAV mode only; webcal stays read-only):
//! [`create_event`](CalendarProvider::create_event) `PUT`s a fresh
//! `<uid>.ics` resource with `If-None-Match: *` (never overwrites);
//! [`update_event`](CalendarProvider::update_event) `PUT`s the same resource
//! with `If-Match: <etag>` when an ETag is held (a `412` maps to
//! [`Error::Conflict`] — the event changed server-side, re-sync first);
//! [`delete_event`](CalendarProvider::delete_event) `DELETE`s it (a `404` is
//! success — deletion is idempotent). The resource name follows the
//! `<uid>.ics` convention this provider's deletion path already relies on
//! ([`uid_from_href`]). An update rewrites the `VEVENT` from catalerum's model
//! ([`ical::event_to_ics`]), so unmapped server-side properties (`VALARM`s,
//! attendee lists) are not preserved across an update.
//!
//! ## Auth
//! HTTP Basic auth credentials come from the connection config (M2 stub:
//! `{"username":…, "password":…}` read straight from config). Real credential
//! decryption via `credential_ref` (SOUL §13) lands with the secrets vault.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use catalerum_core::error::{Error, Result};
use catalerum_core::id::{ConnectionId, WorkspaceId};
use catalerum_core::model::{Calendar, Cursor, Event};
use catalerum_core::provider::{CalendarProvider, NewEvent, SyncBatch};

use crate::ical;
use crate::multistatus::{parse_multistatus, MultiStatus};

/// Config keys read from `connections.config`.
pub mod config_keys {
    /// Canonical base-URL key the API persists for `caldav`/`webcal`
    /// connections (`POST /connections` blesses `base_url`), so the provider
    /// and the API agree on one wire name end-to-end. Required.
    pub const BASE_URL: &str = "base_url";
    /// Legacy/alias base-URL key, accepted on read for older connections.
    pub const URL: &str = "url";
    /// The base-URL keys this provider reads, in priority order: the canonical
    /// [`BASE_URL`] first, then the [`URL`] alias.
    pub const URL_KEYS: &[&str] = &[BASE_URL, URL];
    /// Optional HTTP Basic username.
    pub const USERNAME: &str = "username";
    /// Optional HTTP Basic password (M2 stub; encrypted vault lands later).
    pub const PASSWORD: &str = "password";
}

/// The transport mode, derived from the configured URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalDavMode {
    /// Full CalDAV collection (`http(s)://`): REPORT-based incremental sync.
    CalDav,
    /// Read-only ICS-over-HTTP subscription (`webcal://`): single GET.
    Webcal,
}

/// HTTP Basic credentials, resolved from config (M2 stub).
#[derive(Clone, Debug, Default)]
struct BasicAuth {
    username: Option<String>,
    password: Option<String>,
}

/// A CalDAV / webcal calendar provider (SOUL §8).
#[derive(Clone, Debug)]
pub struct CalDavProvider {
    workspace_id: WorkspaceId,
    connection_id: ConnectionId,
    mode: CalDavMode,
    /// Normalised HTTP(S) base URL (webcal:// rewritten to https://).
    url: String,
    auth: BasicAuth,
    http: reqwest::Client,
}

impl CalDavProvider {
    /// Construct from a connection's `config` JSON (keys in [`config_keys`]).
    pub fn from_config(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        config: &serde_json::Value,
    ) -> Result<Self> {
        let raw_url = config_keys::URL_KEYS
            .iter()
            .find_map(|key| config.get(*key).and_then(serde_json::Value::as_str))
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                Error::invalid(format!(
                    "caldav connection config missing string `{}` (or `{}`)",
                    config_keys::BASE_URL,
                    config_keys::URL
                ))
            })?;

        let (mode, url) = normalize_url(raw_url)?;
        let auth = BasicAuth {
            username: str_field(config, config_keys::USERNAME),
            password: str_field(config, config_keys::PASSWORD),
        };

        let http = reqwest::Client::builder()
            .user_agent("catalerum-calendar")
            // Fail fast if the external CalDAV server is unreachable rather than
            // hanging the sync worker. No overall timeout: a REPORT can return a
            // large multistatus and shouldn't be aborted mid-transfer.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Provider(format!("build http client: {e}")))?;

        Ok(Self {
            workspace_id,
            connection_id,
            mode,
            url,
            auth,
            http,
        })
    }

    /// Construct directly (mainly for tests / explicit wiring). `url` may be a
    /// `webcal://` URL; it is normalised.
    pub fn new(
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        url: &str,
        username: Option<String>,
        password: Option<String>,
    ) -> Result<Self> {
        let (mode, url) = normalize_url(url)?;
        let http = reqwest::Client::builder()
            .user_agent("catalerum-calendar")
            // Fail fast if the external CalDAV server is unreachable rather than
            // hanging the sync worker. No overall timeout: a REPORT can return a
            // large multistatus and shouldn't be aborted mid-transfer.
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Provider(format!("build http client: {e}")))?;
        Ok(Self {
            workspace_id,
            connection_id,
            mode,
            url,
            auth: BasicAuth { username, password },
            http,
        })
    }

    /// The transport mode.
    #[must_use]
    pub fn mode(&self) -> CalDavMode {
        self.mode
    }

    /// The normalised HTTP(S) URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The single [`Calendar`] this connection represents. CalDAV discovery of
    /// *multiple* collections under a principal is a future enhancement; for M2
    /// the configured URL is the calendar, with a stable derived id.
    fn calendar(&self) -> Calendar {
        let id = catalerum_core::CalendarId::from_uuid(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("{}/{}", self.connection_id, self.url).as_bytes(),
        ));
        Calendar {
            id,
            workspace_id: self.workspace_id,
            connection_id: Some(self.connection_id),
            external_id: self.url.clone(),
            name: collection_name(&self.url),
            read_only: matches!(self.mode, CalDavMode::Webcal),
        }
    }

    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match (&self.auth.username, &self.auth.password) {
            (Some(u), p) => req.basic_auth(u, p.clone()),
            _ => req,
        }
    }

    /// Write-back is CalDAV-mode only; a webcal subscription is read-only.
    fn require_writable(&self) -> Result<()> {
        match self.mode {
            CalDavMode::CalDav => Ok(()),
            CalDavMode::Webcal => Err(Error::Unsupported(
                "a webcal subscription is read-only; events cannot be written to it".into(),
            )),
        }
    }

    /// The resource URL for an event in this collection: `{base}/{uid}.ics` —
    /// the server-conventional naming the deletion path ([`uid_from_href`])
    /// already relies on, with the UID percent-encoded for the path.
    fn event_url(&self, uid: &str) -> String {
        format!(
            "{}/{}.ics",
            self.url.trim_end_matches('/'),
            percent_encode_segment(uid)
        )
    }

    // --- webcal: a single ICS GET -----------------------------------------

    async fn sync_webcal(
        &self,
        cal: &Calendar,
        cursor: Option<Cursor>,
    ) -> Result<SyncBatch<Event>> {
        let resp = self
            .apply_auth(self.http.get(&self.url))
            .send()
            .await
            .map_err(|e| Error::Provider(format!("GET {}: {e}", self.url)))?;

        let resp = ensure_success(resp)?;
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let body = read_body_capped(resp, "GET body").await?;

        let next_cursor = etag
            .map(|e| Cursor::new(format!("etag:{}", e.trim_matches('"'))))
            .unwrap_or_else(|| crate::content_cursor(body.as_bytes()));

        let unchanged = cursor.as_ref() == Some(&next_cursor);
        let upserts = if unchanged {
            Vec::new()
        } else {
            ical::parse_calendar(&body)?
                .into_iter()
                .map(|p| p.into_event(self.workspace_id, cal.id))
                .collect()
        };

        Ok(SyncBatch {
            upserts,
            deletions: Vec::new(),
            next_cursor,
            has_more: false,
        })
    }

    // --- CalDAV: sync-collection / calendar-query REPORTs ------------------

    async fn report(&self, body: String) -> Result<MultiStatus> {
        let method = reqwest::Method::from_bytes(b"REPORT")
            .map_err(|e| Error::Provider(format!("REPORT method: {e}")))?;
        let resp = self
            .apply_auth(self.http.request(method, &self.url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/xml; charset=utf-8",
            )
            .header("Depth", "1")
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("REPORT {}: {e}", self.url)))?;

        let resp = ensure_success(resp)?;
        let xml = read_body_capped(resp, "REPORT body").await?;
        parse_multistatus(&xml)
    }

    async fn sync_caldav(
        &self,
        cal: &Calendar,
        cursor: Option<Cursor>,
    ) -> Result<SyncBatch<Event>> {
        // RFC 6578: use sync-collection with the held sync-token; on the first
        // sync (no token) fall back to a calendar-query by an open time-range.
        let sync_token = cursor
            .as_ref()
            .and_then(|c| c.0.strip_prefix("sync:"))
            .map(str::to_string);

        let report_body = match &sync_token {
            Some(token) => sync_collection_body(token),
            None => calendar_query_body(),
        };

        let ms = self.report(report_body).await?;

        let mut upserts = Vec::new();
        let mut deletions = Vec::new();

        for entry in &ms.responses {
            if entry.is_deleted() {
                if let Some(uid) = uid_from_href(&entry.href) {
                    deletions.push(uid);
                }
                continue;
            }
            let Some(data) = &entry.calendar_data else {
                continue;
            };
            for parsed in ical::parse_vevents(data)? {
                let mut event = parsed.into_event(self.workspace_id, cal.id);
                event.etag = entry.etag.clone();
                upserts.push(event);
            }
        }

        // Next cursor: the server sync-token when offered (true incremental),
        // else a deterministic hash over the returned (href,etag) set so a
        // re-sync with unchanged server state produces the same cursor and the
        // caller can short-circuit (idempotent — SOUL §3.4).
        let next_cursor = match ms.sync_token {
            Some(token) => Cursor::new(format!("sync:{token}")),
            None => etagset_cursor(&ms),
        };

        Ok(SyncBatch {
            upserts,
            deletions,
            next_cursor,
            has_more: false,
        })
    }
}

#[async_trait]
impl CalendarProvider for CalDavProvider {
    async fn list_calendars(&self) -> Result<Vec<Calendar>> {
        // For M2 a connection maps to exactly one calendar (the configured
        // URL). Multi-collection PROPFIND discovery is a future enhancement.
        Ok(vec![self.calendar()])
    }

    async fn sync(&self, cal: &Calendar, cursor: Option<Cursor>) -> Result<SyncBatch<Event>> {
        match self.mode {
            CalDavMode::Webcal => self.sync_webcal(cal, cursor).await,
            CalDavMode::CalDav => self.sync_caldav(cal, cursor).await,
        }
    }

    /// CalDAV proper (`sync-collection` REPORT, RFC 6578) returns incremental
    /// deltas with deletions; webcal is a full ICS snapshot (content-hash cursor),
    /// so only the CalDAV mode is incremental.
    fn is_incremental(&self) -> bool {
        matches!(self.mode, CalDavMode::CalDav)
    }

    async fn create_event(&self, cal: &Calendar, event: NewEvent) -> Result<Event> {
        self.require_writable()?;
        // A fresh UUID names both the VEVENT UID and the `<uid>.ics` resource,
        // matching the naming convention the deletion path reads back.
        let uid = uuid::Uuid::new_v4().to_string();
        let attendees = event.attendees.clone();
        let stored = new_event_to_event(event, uid, self.workspace_id, cal.id);
        let ics = ical::event_to_ics(&stored, &attendees);
        let url = self.event_url(&stored.uid);
        let resp = self
            .apply_auth(self.http.put(&url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "text/calendar; charset=utf-8",
            )
            // Never overwrite an existing resource on create.
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .body(ics)
            .send()
            .await
            .map_err(|e| Error::Provider(format!("PUT {url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "calendar resource `{}` already exists on the server",
                stored.uid
            )));
        }
        let resp = ensure_success(resp)?;
        let etag = response_etag(&resp);
        Ok(Event { etag, ..stored })
    }

    async fn update_event(&self, event: &Event) -> Result<Event> {
        self.require_writable()?;
        // The core event's attendees are resolved entity pointers with no
        // calendar address, so none are written (see the module docs).
        let ics = ical::event_to_ics(event, &[]);
        let url = self.event_url(&event.uid);
        let mut req = self
            .apply_auth(self.http.put(&url))
            .header(
                reqwest::header::CONTENT_TYPE,
                "text/calendar; charset=utf-8",
            )
            .body(ics);
        if let Some(etag) = event.etag.as_deref().filter(|s| !s.is_empty()) {
            req = req.header(reqwest::header::IF_MATCH, etag_header_value(etag));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Provider(format!("PUT {url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "event `{}` changed on the calendar server (ETag mismatch) — sync the \
                 calendar and retry the edit",
                event.uid
            )));
        }
        let resp = ensure_success(resp)?;
        // The server's new ETag (when offered) rides back so the caller stores
        // the current one; absent, the next sync refreshes it.
        let etag = response_etag(&resp).or_else(|| event.etag.clone());
        Ok(Event {
            etag,
            ..event.clone()
        })
    }

    async fn delete_event(&self, event: &Event) -> Result<()> {
        self.require_writable()?;
        let url = self.event_url(&event.uid);
        let mut req = self.apply_auth(self.http.delete(&url));
        if let Some(etag) = event.etag.as_deref().filter(|s| !s.is_empty()) {
            req = req.header(reqwest::header::IF_MATCH, etag_header_value(etag));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::Provider(format!("DELETE {url}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(()); // already gone — deletion is idempotent
        }
        if resp.status() == reqwest::StatusCode::PRECONDITION_FAILED {
            return Err(Error::Conflict(format!(
                "event `{}` changed on the calendar server (ETag mismatch) — sync the \
                 calendar and retry the delete",
                event.uid
            )));
        }
        ensure_success(resp)?;
        Ok(())
    }
}

// --- request bodies --------------------------------------------------------

/// RFC 6578 `sync-collection` REPORT requesting getetag + calendar-data for
/// everything changed since `token`.
fn sync_collection_body(token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:sync-token>{}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
</d:sync-collection>"#,
        xml_escape(token)
    )
}

/// RFC 4791 `calendar-query` REPORT for all VEVENTs (open time-range): the
/// initial full pull when no sync-token is held yet.
fn calendar_query_body() -> String {
    r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop>
    <d:getetag/>
    <c:calendar-data/>
  </d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"/>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
        .to_string()
}

// --- helpers ---------------------------------------------------------------

fn str_field(config: &serde_json::Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Classify + normalise a calendar URL. `webcal://` is rewritten to `https://`
/// (the de-facto meaning of the scheme); `?ics` / `.ics` HTTP URLs are also
/// treated as webcal subscriptions.
fn normalize_url(raw: &str) -> Result<(CalDavMode, String)> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("webcal://") {
        return Ok((CalDavMode::Webcal, format!("https://{rest}")));
    }
    if let Some(rest) = trimmed.strip_prefix("webcals://") {
        return Ok((CalDavMode::Webcal, format!("https://{rest}")));
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        // A bare `.ics` document over HTTP is a read-only subscription.
        let mode = if trimmed
            .split('?')
            .next()
            .unwrap_or(trimmed)
            .ends_with(".ics")
        {
            CalDavMode::Webcal
        } else {
            CalDavMode::CalDav
        };
        return Ok((mode, trimmed.to_string()));
    }
    Err(Error::invalid(format!(
        "unsupported calendar URL scheme: {raw}"
    )))
}

/// A readable calendar name from the collection URL's last path segment.
fn collection_name(url: &str) -> String {
    let path = url.split('?').next().unwrap_or(url);
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .map(|s| percent_decode(s.trim_end_matches(".ics")))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "calendar".to_string())
}

/// Percent-decode a URL path segment (`%20` → space, `%C3%A9` → `é`); a malformed
/// or truncated `%` escape is kept literal, and invalid UTF-8 degrades lossily.
/// A plain-ASCII segment (the common UID case) is returned unchanged.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode a UID for use as a URL path segment (RFC 3986 unreserved set
/// kept verbatim) — the inverse of [`percent_decode`], so an encoded resource
/// name decodes back to the raw UID the deletion path compares against.
fn percent_encode_segment(s: &str) -> String {
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

/// Shape a stored ETag for an `If-Match` header: an already-quoted (or weak
/// `W/"…"`) value passes verbatim; a bare opaque tag gains the RFC 7232 quotes.
fn etag_header_value(etag: &str) -> String {
    let t = etag.trim();
    if t.starts_with('"') || t.starts_with("W/") {
        t.to_string()
    } else {
        format!("\"{t}\"")
    }
}

/// The `ETag` header of a write response, verbatim (as [`sync_caldav`] stores
/// them), when the server offers one.
fn response_etag(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Promote a [`NewEvent`] into the core [`Event`] a successful `PUT` persists:
/// the CalDAV `PUT` returns no body, so the written event IS our serialization
/// under the freshly minted `uid`. Attendees stay empty on the core event (they
/// are resolved entity pointers, filled by the ingest graph step).
fn new_event_to_event(
    event: NewEvent,
    uid: String,
    workspace_id: WorkspaceId,
    calendar_id: catalerum_core::CalendarId,
) -> Event {
    Event {
        id: catalerum_core::EventId::new(),
        workspace_id,
        calendar_id,
        uid,
        start: event.start,
        end: event.end,
        all_day: event.all_day,
        rrule: event.rrule,
        summary: event.summary,
        location: event.location,
        attendees: Vec::new(),
        body: event.body,
        labels: event.labels,
        attachments: event.attachments,
        etag: None,
        sequence: 0,
    }
}

/// Derive a stable event UID from a resource href when the deleted resource has
/// no body to read a real UID from. CalDAV servers conventionally name the
/// resource `<uid>.ics`, so the file stem is the UID for our deletion path.
fn uid_from_href(href: &str) -> Option<String> {
    let last = href.trim_end_matches('/').rsplit('/').next()?;
    // A deleted event is always an `<uid>.ics` resource; anything else (e.g. a
    // collection href ending in `/`) is not an event and yields no UID.
    let stem = last.strip_suffix(".ics")?;
    if stem.is_empty() {
        None
    } else {
        // The href segment is URL-encoded; decode so a UID with spaces/non-ASCII
        // matches the raw VEVENT UID we stored on ingest (else its delete is missed).
        Some(percent_decode(stem))
    }
}

fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(Error::unauthorized(format!(
            "calendar server returned {status}"
        )));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err(Error::NotFound);
    }
    // 207 Multi-Status is the success code for REPORT; `is_success()` covers it.
    if !status.is_success() {
        return Err(Error::Provider(format!(
            "calendar server returned {status}"
        )));
    }
    Ok(resp)
}

/// Cap on bytes read from a CalDAV/webcal response body. Generous for a real
/// calendar (many years of events) but bounds an unbounded or malicious server
/// response so it can't OOM the sync worker — `reqwest::Response::text()` buffers
/// the *entire* body with no limit, and a subscribed external feed is untrusted.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024; // 64 MiB

/// Read a response body to a `String`, capped at [`MAX_RESPONSE_BYTES`]. Exceeding
/// the cap errors rather than buffering further (a truncated iCalendar/XML wouldn't
/// parse anyway, so failing the sync with a clear error is the right outcome).
async fn read_body_capped(mut resp: reqwest::Response, what: &str) -> Result<String> {
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
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Deterministic cursor over the (href, etag) pairs of a multistatus — used
/// when the server doesn't return a sync-token, so unchanged state hashes the
/// same and re-sync is idempotent.
fn etagset_cursor(ms: &MultiStatus) -> Cursor {
    let mut pairs: Vec<String> = ms
        .responses
        .iter()
        .map(|r| format!("{}={}", r.href, r.etag.as_deref().unwrap_or("")))
        .collect();
    pairs.sort();
    let mut hasher = Sha256::new();
    hasher.update(pairs.join("\n").as_bytes());
    Cursor::new(format!("etagset:{:x}", hasher.finalize()))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webcal_url_normalises_to_https_and_is_readonly() {
        let p = CalDavProvider::new(
            WorkspaceId::new(),
            ConnectionId::new(),
            "webcal://example.com/feed.ics",
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.mode(), CalDavMode::Webcal);
        assert_eq!(p.url(), "https://example.com/feed.ics");
        assert!(p.calendar().read_only);
    }

    #[test]
    fn https_collection_is_caldav_readwrite_capable() {
        let p = CalDavProvider::new(
            WorkspaceId::new(),
            ConnectionId::new(),
            "https://dav.example.com/cal/work/",
            Some("user".into()),
            Some("pass".into()),
        )
        .unwrap();
        assert_eq!(p.mode(), CalDavMode::CalDav);
        assert!(!p.calendar().read_only);
        assert_eq!(p.calendar().name, "work");
    }

    #[test]
    fn bare_ics_over_http_is_webcal() {
        let (mode, url) = normalize_url("https://example.com/a/b.ics").unwrap();
        assert_eq!(mode, CalDavMode::Webcal);
        assert_eq!(url, "https://example.com/a/b.ics");
    }

    #[test]
    fn unknown_scheme_rejected() {
        assert!(normalize_url("ftp://x/y").is_err());
    }

    #[test]
    fn uid_from_href_uses_file_stem() {
        assert_eq!(
            uid_from_href("/cal/work/evt-1.ics").as_deref(),
            Some("evt-1")
        );
        assert_eq!(uid_from_href("/cal/work/").as_deref(), None);
        // A UID with characters that must be percent-encoded in the href is decoded
        // back to its raw form so it matches the stored VEVENT UID on deletion.
        assert_eq!(
            uid_from_href("/cal/My%20Event%20%231.ics").as_deref(),
            Some("My Event #1")
        );
        assert_eq!(
            uid_from_href("/cal/caf%C3%A9.ics").as_deref(),
            Some("caf\u{e9}")
        );
    }

    #[test]
    fn percent_decode_handles_escapes_and_malformed() {
        assert_eq!(percent_decode("plain-uid@host"), "plain-uid@host"); // no-op
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%C3%A9"), "\u{e9}");
        // Malformed/truncated escapes stay literal rather than panicking.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%ZZ"), "%ZZ");
        assert_eq!(percent_decode("%2"), "%2");
    }

    #[test]
    fn collection_name_decodes_the_path_segment() {
        assert_eq!(
            collection_name("https://d/cal/My%20Calendar/"),
            "My Calendar"
        );
        assert_eq!(collection_name("https://d/dav/work.ics"), "work");
    }

    #[test]
    fn from_config_requires_url() {
        let cfg = serde_json::json!({ "username": "u" });
        assert!(
            CalDavProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).is_err()
        );
        let ok = serde_json::json!({ "url": "https://d/cal/", "username": "u", "password": "p" });
        assert!(CalDavProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &ok).is_ok());
    }

    #[test]
    fn from_config_reads_base_url_key() {
        // The canonical `base_url` key the API persists must build a provider.
        let cfg = serde_json::json!({ "base_url": "https://d/cal/" });
        let p = CalDavProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).unwrap();
        assert_eq!(p.url(), "https://d/cal/");

        // `base_url` wins over the legacy `url` alias when both are present.
        let cfg = serde_json::json!({ "base_url": "https://canonical/cal/", "url": "https://legacy/cal/" });
        let p = CalDavProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &cfg).unwrap();
        assert_eq!(p.url(), "https://canonical/cal/");

        // An empty base_url is treated as missing.
        let empty = serde_json::json!({ "base_url": "  " });
        assert!(
            CalDavProvider::from_config(WorkspaceId::new(), ConnectionId::new(), &empty).is_err()
        );
    }

    #[test]
    fn sync_collection_body_includes_token() {
        let body = sync_collection_body("tok-123");
        assert!(body.contains("sync-collection"));
        assert!(body.contains("tok-123"));
        assert!(body.contains("calendar-data"));
    }

    #[tokio::test]
    async fn webcal_refuses_write_back() {
        let p = CalDavProvider::new(
            WorkspaceId::new(),
            ConnectionId::new(),
            "webcal://example.com/feed.ics",
            None,
            None,
        )
        .unwrap();
        let cal = p.calendar();
        let e = NewEvent {
            summary: "x".into(),
            start: chrono::Utc::now(),
            end: chrono::Utc::now(),
            all_day: false,
            location: None,
            body: None,
            rrule: None,
            attendees: Vec::new(),
            labels: Vec::new(),
            attachments: Vec::new(),
        };
        assert!(matches!(
            p.create_event(&cal, e).await,
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn event_url_joins_and_encodes_the_uid() {
        let p = CalDavProvider::new(
            WorkspaceId::new(),
            ConnectionId::new(),
            "https://dav.example.com/cal/work/",
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            p.event_url("evt-1"),
            "https://dav.example.com/cal/work/evt-1.ics"
        );
        // Encoding is the exact inverse of the deletion path's decode.
        assert_eq!(
            p.event_url("My Event #1"),
            "https://dav.example.com/cal/work/My%20Event%20%231.ics"
        );
        assert_eq!(
            uid_from_href("/cal/work/My%20Event%20%231.ics").as_deref(),
            Some("My Event #1")
        );
    }

    #[test]
    fn etag_header_value_quotes_bare_tags_only() {
        assert_eq!(etag_header_value("abc"), "\"abc\"");
        assert_eq!(etag_header_value("\"abc\""), "\"abc\"");
        assert_eq!(etag_header_value("W/\"abc\""), "W/\"abc\"");
    }

    #[test]
    fn new_event_promotes_every_field_with_fresh_identity() {
        let ws = WorkspaceId::new();
        let cal = catalerum_core::CalendarId::new();
        let start = chrono::Utc::now();
        let end = start + chrono::Duration::hours(1);
        let e = new_event_to_event(
            NewEvent {
                summary: "s".into(),
                start,
                end,
                all_day: true,
                location: Some("l".into()),
                body: Some("b".into()),
                rrule: Some("FREQ=DAILY".into()),
                attendees: vec!["a@b".into()],
                labels: vec!["work".into()],
                attachments: Vec::new(),
            },
            "uid-1".into(),
            ws,
            cal,
        );
        assert_eq!(e.uid, "uid-1");
        assert_eq!((e.workspace_id, e.calendar_id), (ws, cal));
        assert!(e.all_day);
        assert_eq!(e.summary, "s");
        assert_eq!(e.labels, vec!["work".to_string()]);
        assert!(e.attendees.is_empty(), "entity pointers stay unresolved");
        assert_eq!(e.sequence, 0);
        assert!(e.etag.is_none());
    }

    #[test]
    fn etagset_cursor_is_stable_and_order_independent() {
        use crate::multistatus::ResponseEntry;
        let a = MultiStatus {
            responses: vec![
                ResponseEntry {
                    href: "/x".into(),
                    etag: Some("1".into()),
                    ..Default::default()
                },
                ResponseEntry {
                    href: "/y".into(),
                    etag: Some("2".into()),
                    ..Default::default()
                },
            ],
            sync_token: None,
        };
        let b = MultiStatus {
            responses: vec![
                ResponseEntry {
                    href: "/y".into(),
                    etag: Some("2".into()),
                    ..Default::default()
                },
                ResponseEntry {
                    href: "/x".into(),
                    etag: Some("1".into()),
                    ..Default::default()
                },
            ],
            sync_token: None,
        };
        assert_eq!(etagset_cursor(&a), etagset_cursor(&b));
    }
}
