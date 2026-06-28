//! Calendar REST (SOUL §8, §10, §12 — M2 calendar ingest).
//!
//! All routes are workspace-scoped to the authenticated principal's workspace —
//! the client never names a workspace; cross-workspace reach is impossible by
//! construction (SOUL §18). They are **capability-gated** ([`Auth::require`],
//! SOUL §19): listing needs `calendar:read` (every role), while creating a
//! connection and triggering a sync need `calendar:write` (a Viewer is `403
//! Forbidden`). Connections, calendars, and events are persisted via
//! `catalerum-store`; a sync request enqueues a durable `job_queue` job that the
//! decoupled ingest worker (`catalerum-ingest`) consumes. The API never calls a
//! provider or the ingest worker directly (SOUL §6.2).
//!
//! ## Local (database-native) calendars (SOUL §8/§11/§12)
//!
//! A **local** calendar has no provider connection — it lives entirely in
//! Postgres and is read-write. Its events are created/edited directly here
//! (rather than synced), which is the writable substrate the §11 automations
//! engine targets (the `CalendarEvent` trigger fires on *any* workspace event,
//! and the `create_event` tool / `CreateEvent` action write through these same
//! repositories). Event writes are restricted to local calendars: a
//! provider-backed calendar is managed by its sync, never edited in place.
//!
//! Routes:
//! - `POST   /connections`            create a calendar connection
//! - `GET    /connections`            list this workspace's connections
//! - `DELETE /connections/{id}`        remove a calendar connection (+ its synced data, `204`)
//! - `POST   /connections/{id}/sync`  enqueue an incremental-sync job (`202`)
//! - `GET    /calendars`              list this workspace's calendars
//! - `POST   /calendars`              create a local (database-native) calendar
//! - `DELETE /calendars/{id}`         delete a calendar + its events (a synced one is also excluded from re-sync)
//! - `GET    /events`                 list events (optional date range + calendar filter)
//! - `POST   /events`                 create an event on a local calendar
//! - `PUT    /events/{id}`            edit an event on a local calendar
//! - `DELETE /events/{id}`            delete an event on a local calendar

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::model::{Attachment, Calendar, Connection, ConnectionKind, Event};
use catalerum_core::{CalendarId, ConnectionId, EventId};
use catalerum_store::{DateRange, EventPatch, UpsertEvent, DEFAULT_EVENT_LIMIT};

use crate::auth::Auth;
use crate::calendar_writeback::{
    create_on_provider, delete_on_provider, merge_event_update, resolve_event_write_target,
    update_on_provider, EventWriteTarget,
};
use crate::connection_status::ConnectionView;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// The `job_queue.kind` the ingest worker dequeues to run a connection's
/// incremental calendar sync. The API and ingest are decoupled via this kind +
/// payload contract; the API only enqueues (SOUL §6.2).
pub const SYNC_CALENDAR_JOB_KIND: &str = "sync_calendar";

/// Mount the calendar routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/connections",
            get(list_connections).post(create_connection),
        )
        .route("/connections/{id}", delete(delete_connection))
        .route("/connections/{id}/sync", post(sync_connection))
        .route("/calendars", get(list_calendars).post(create_calendar))
        .route("/calendars/{id}", delete(delete_calendar))
        .route("/events", get(list_events).post(create_event))
        .route("/events/{id}", put(update_event).delete(delete_event))
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

/// The provider sub-kind of a calendar connection. The core
/// [`ConnectionKind`](catalerum_core::model::ConnectionKind) stays abstract
/// (`Calendar`); the concrete provider rides in the connection `config` blob
/// (SOUL §3.2). This is the wire token the client sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarProviderKind {
    /// A local directory of `.ics` files (read-only by default).
    Local,
    /// A CalDAV server (RFC 4791 sync-collection + ETags).
    Caldav,
    /// A `webcal://` / `https://` published ICS feed (read-only).
    Webcal,
}

impl CalendarProviderKind {
    /// The stable token persisted in `connections.config.provider`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CalendarProviderKind::Local => "local",
            CalendarProviderKind::Caldav => "caldav",
            CalendarProviderKind::Webcal => "webcal",
        }
    }
}

/// Body for `POST /connections`. Creates a calendar connection.
///
/// The provider `kind` and `config` are stored in the connection's `config`
/// JSONB; the abstract core kind is always `Calendar`. `credentials` is an
/// opaque secret-store reference (never plaintext, SOUL §13).
#[derive(Debug, Deserialize)]
pub struct CreateConnection {
    /// Provider sub-kind: `local` | `caldav` | `webcal`.
    pub kind: CalendarProviderKind,
    /// Human-readable name for the connection.
    pub name: String,
    /// Per-provider settings: `{ "dir": "..." }` for local, `{ "base_url": "..." }`
    /// for caldav/webcal. Defaults to `{}`.
    #[serde(default)]
    pub config: serde_json::Value,
    /// Optional secret-store reference for credentials (SOUL §13).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

async fn create_connection(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateConnection>,
) -> ApiResult<(StatusCode, Json<Connection>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("connection name must not be empty"));
    }
    let config = build_calendar_config(body.kind, body.config).map_err(ApiError::bad_request)?;

    let connection = state
        .store()
        .connections()
        .create(
            ws,
            ConnectionKind::Calendar,
            &body.name,
            body.credentials.as_deref(),
            Some(config),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(connection)))
}

/// Validate a calendar connection's per-provider `config` and stamp the provider
/// sub-kind into it, so the ingest worker can pick the concrete
/// `CalendarProvider`. The abstract core kind is always Calendar (SOUL §3.2).
/// `pub(crate)` so the `create_calendar_connection` LLM tool builds the identical
/// blob (SOUL §7/§8) — one config shape, two authoring surfaces.
pub(crate) fn build_calendar_config(
    kind: CalendarProviderKind,
    config: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let required = match kind {
        CalendarProviderKind::Local => "dir",
        CalendarProviderKind::Caldav | CalendarProviderKind::Webcal => "base_url",
    };
    let present = config
        .get(required)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if !present {
        return Err(format!(
            "calendar connection of kind '{}' requires a non-empty config.{required}",
            kind.as_str()
        ));
    }
    let mut config = match config {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(map) => map,
        other => {
            // Non-object config: wrap it so `provider` can always be set.
            let mut map = serde_json::Map::new();
            map.insert("settings".to_string(), other);
            map
        }
    };
    config.insert(
        "provider".to_string(),
        serde_json::Value::String(kind.as_str().to_string()),
    );
    Ok(serde_json::Value::Object(config))
}

/// `GET /connections` — this workspace's connections, newest first, each
/// annotated with its **collect status** (SOUL §29): `collecting` is `true` only
/// when an enabled automation heads a `CollectCalendar` trigger at it. A
/// `collecting: false` calendar source is **dormant** — configured but nothing
/// will ever ingest from it (adding a connection provisions nothing, §10) — the
/// cue the UI turns into an inline "idle" warning. Presentation-only; no mutation.
async fn list_connections(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<ConnectionView>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    let connections = state.store().connections().list_by_workspace(ws).await?;
    // Scan the workspace's automations once to mark which sources are live
    // (SOUL §29). Read-only projection; only the derived `collecting` boolean is
    // exposed, never automation contents.
    let automations = state.store().automations().list_by_workspace(ws).await?;
    Ok(Json(crate::connection_status::annotate(
        connections,
        &automations,
    )))
}

/// Response for `POST /connections/{id}/sync`: the enqueued job's id and kind.
#[derive(Debug, Serialize)]
pub struct SyncEnqueued {
    /// The `job_queue` row id of the enqueued sync job.
    pub job_id: uuid::Uuid,
    /// The job kind (always [`SYNC_CALENDAR_JOB_KIND`]).
    pub kind: &'static str,
    /// The connection being synced.
    pub connection_id: ConnectionId,
}

/// The `job_queue.payload` shape for a `sync_calendar` job (the ingest contract
/// shared with `catalerum-ingest`'s worker). Carries the connection to sync and
/// its owning workspace, so the worker has an authoritative sync scope without
/// re-reading the job row. `workspace_id` is omitted on the wire when `None`
/// (the worker then falls back to the job's `workspace_id` column), but the API
/// always sets it.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyncCalendarPayload {
    /// The workspace that owns the connection (every row is workspace-scoped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<catalerum_core::WorkspaceId>,
    /// The connection to sync.
    pub connection_id: ConnectionId,
}

/// `DELETE /connections/{id}` — remove a calendar connection (source) and, via
/// the `ON DELETE CASCADE` FKs, its synced calendars + events. The external
/// source is untouched (re-adding the connection re-syncs). Gated `calendar:write`
/// — symmetric with `create_connection`, since removing a source you added is a
/// management op, not the higher-stakes `calendar:delete` (which destroys
/// hand-authored *local* events). `404` for a foreign/unknown id; `400` if it
/// isn't a calendar connection (remove an email source via `/email/connections`).
async fn delete_connection(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let connection = state
        .store()
        .connections()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != ConnectionKind::Calendar {
        return Err(ApiError::bad_request(
            "this endpoint removes calendar connections; remove an email source via /email/connections",
        ));
    }
    state.store().connections().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn sync_connection(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<(StatusCode, Json<SyncEnqueued>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    // Verify the connection exists and belongs to the caller's workspace before
    // enqueueing — never enqueue a sync for another workspace's connection.
    let connection = state.store().connections().get(ws, id).await?;
    if connection.kind != ConnectionKind::Calendar {
        return Err(ApiError::bad_request(
            "connection is not a calendar connection",
        ));
    }

    let payload = serde_json::to_value(SyncCalendarPayload {
        workspace_id: Some(ws),
        connection_id: id,
    })
    .map_err(|e| ApiError::internal(format!("failed to encode sync payload: {e}")))?;
    let job = state
        .store()
        .job_queue()
        .enqueue(Some(ws), SYNC_CALENDAR_JOB_KIND, payload, None)
        .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(SyncEnqueued {
            job_id: job.id,
            kind: SYNC_CALENDAR_JOB_KIND,
            connection_id: id,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Calendars
// ---------------------------------------------------------------------------

async fn list_calendars(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<Calendar>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    let calendars = state.store().calendars().list_by_workspace(ws).await?;
    Ok(Json(calendars))
}

/// Body for `POST /calendars`. Creates a **local** (database-native) calendar:
/// no provider connection, read-write, never synced (SOUL §8/§11).
#[derive(Debug, Deserialize)]
pub struct CreateCalendar {
    /// Human-readable calendar name (must be non-empty).
    pub name: String,
}

async fn create_calendar(
    State(state): State<AppState>,
    auth: Auth,
    Json(req): Json<CreateCalendar>,
) -> ApiResult<(StatusCode, Json<Calendar>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("calendar name must not be empty"));
    }
    // Get-or-create by name (shared with the `create_calendar` tool and a
    // `WriteEvent` name redirect), so a double-submit — or re-creating a
    // calendar the user already made — returns the existing one instead of a
    // duplicate. The auto "default" calendar uses a stable reserved key.
    let calendar =
        crate::action_runner::get_or_create_local_calendar_by_name(state.store(), ws, name)
            .await
            .map_err(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(calendar)))
}

async fn delete_calendar(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<CalendarId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Delete, "calendar")?;
    let ws = auth.principal().workspace_id;
    let calendar = state
        .store()
        .calendars()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    // A provider (synced) calendar would be re-`upsert`ed by the next connection
    // sync, so deleting the row alone wouldn't stick. Record a persistent
    // exclusion on `(connection_id, external_id)` first — both ingest sync paths
    // skip excluded calendars — then drop the row (cascading its synced events).
    // The external calendar is untouched; removing + re-adding the source clears
    // the exclusion (FK cascade) and re-syncs it.
    if let Some(connection_id) = calendar.connection_id {
        state
            .store()
            .calendars()
            .exclude(ws, connection_id, &calendar.external_id)
            .await?;
    }
    state.store().calendars().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Event writes (local calendars + provider write-back, SOUL §8/§11)
// ---------------------------------------------------------------------------

/// Resolve where a write to `calendar_id` lands: a local calendar keeps the
/// store-only path; a **writable provider calendar** (CalDAV, Google, Outlook)
/// gets a live provider for write-back (`crate::calendar_writeback`); a
/// read-only calendar is refused with `400`.
async fn event_write_target(
    state: &AppState,
    ws: catalerum_core::WorkspaceId,
    calendar_id: CalendarId,
) -> ApiResult<EventWriteTarget> {
    let calendar = state
        .store()
        .calendars()
        .get(ws, calendar_id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    Ok(resolve_event_write_target(state.store(), state.secret_store(), ws, calendar).await?)
}

/// Trim a string field, treating all-whitespace as absent (`None`).
fn clean(value: Option<&String>) -> Option<&str> {
    value.map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Normalize event labels: trim, drop blanks, and dedup case-insensitively
/// (keeping the first-seen casing). Mirrors how the graph projection derives a
/// `:Topic` per label, so what is stored matches what is projected.
fn clean_labels(labels: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    labels
        .iter()
        .filter_map(|l| {
            let label = l.trim();
            (!label.is_empty() && seen.insert(label.to_lowercase())).then(|| label.to_string())
        })
        .collect()
}

/// Normalize attachments: drop any with a blank `url`, trim the `url`, and trim
/// the optional string fields to absence when blank. Keeps the descriptor faithful
/// without persisting junk.
fn clean_attachments(attachments: Vec<Attachment>) -> Vec<Attachment> {
    attachments
        .into_iter()
        .filter_map(|a| {
            let url = a.url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            let tidy =
                |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            Some(Attachment {
                url,
                filename: tidy(a.filename),
                content_type: tidy(a.content_type),
                size: a.size,
            })
        })
        .collect()
}

/// Body for `POST /events`. Creates an event on a local calendar (SOUL §8/§11).
#[derive(Debug, Deserialize)]
pub struct CreateEvent {
    /// The local calendar to add the event to.
    pub calendar_id: CalendarId,
    /// Event title (must be non-empty).
    pub summary: String,
    /// Start (RFC 3339). The offset is kept (not collapsed to UTC on parse) so an
    /// all-day start can be pinned to midnight UTC of the *written* date.
    pub start: DateTime<FixedOffset>,
    /// End (RFC 3339); must not precede `start`.
    pub end: DateTime<FixedOffset>,
    /// All-day flag.
    #[serde(default)]
    pub all_day: bool,
    /// Optional location.
    #[serde(default)]
    pub location: Option<String>,
    /// Optional free-text description / body.
    #[serde(default)]
    pub body: Option<String>,
    /// Optional RFC 5545 recurrence rule.
    #[serde(default)]
    pub rrule: Option<String>,
    /// Category labels (iCalendar `CATEGORIES`); deduped + trimmed server-side.
    #[serde(default)]
    pub labels: Vec<String>,
    /// File / image attachments (iCalendar `ATTACH`); blank-`url` entries dropped.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

async fn create_event(
    State(state): State<AppState>,
    auth: Auth,
    Json(req): Json<CreateEvent>,
) -> ApiResult<(StatusCode, Json<Event>)> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let summary = req.summary.trim();
    if summary.is_empty() {
        return Err(ApiError::bad_request("event summary must not be empty"));
    }
    // All-day endpoints pin to midnight UTC of the written date (see
    // `normalize_event_span`); timed ones collapse to their UTC instant.
    let (start, end) =
        crate::calendar_writeback::normalize_event_span(req.start, req.end, req.all_day);
    if end < start {
        return Err(ApiError::bad_request("`end` must not precede `start`"));
    }
    let target = event_write_target(&state, ws, req.calendar_id).await?;

    let labels = clean_labels(&req.labels);
    let attachments = clean_attachments(req.attachments);

    let event = match target {
        EventWriteTarget::Local(_) => {
            // A freshly-minted UID: the event's stable identity. It can never
            // collide with an existing `(calendar_id, uid)`, so this is a pure
            // insert.
            let uid = uuid::Uuid::new_v4().to_string();
            state
                .store()
                .events()
                .create(&UpsertEvent {
                    workspace_id: ws,
                    calendar_id: req.calendar_id,
                    uid: &uid,
                    starts_at: start,
                    ends_at: end,
                    all_day: req.all_day,
                    rrule: clean(req.rrule.as_ref()),
                    summary,
                    location: clean(req.location.as_ref()),
                    body: clean(req.body.as_ref()),
                    attendees: &[],
                    labels: &labels,
                    attachments: &attachments,
                    etag: None,
                    sequence: 0,
                })
                .await?
        }
        // Provider write-back (SOUL §8): the provider mints the uid/ETag; the
        // store mirrors its canonical event.
        EventWriteTarget::Provider { calendar, provider } => {
            create_on_provider(
                state.store(),
                &calendar,
                &provider,
                catalerum_core::provider::NewEvent {
                    summary: summary.to_string(),
                    start,
                    end,
                    all_day: req.all_day,
                    location: clean(req.location.as_ref()).map(str::to_string),
                    body: clean(req.body.as_ref()).map(str::to_string),
                    rrule: clean(req.rrule.as_ref()).map(str::to_string),
                    attendees: Vec::new(),
                    labels,
                    attachments,
                },
            )
            .await?
        }
    };
    // Best-effort: project the new event (+ its label topics) into the derived
    // graph (SOUL §6.3/§8). A no-op when no graph is configured; never fails the
    // write.
    state.enqueue_event_projection(ws, event.id).await;
    Ok((StatusCode::CREATED, Json(event)))
}

/// Body for `PUT /events/{id}`. A full replacement of an event's editable
/// fields; `uid` is immutable and `SEQUENCE` is bumped by the store.
#[derive(Debug, Deserialize)]
pub struct UpdateEvent {
    /// New title (must be non-empty).
    pub summary: String,
    /// New start (RFC 3339). Offset kept (see [`CreateEvent::start`]) so an
    /// all-day start pins to midnight UTC of the written date.
    pub start: DateTime<FixedOffset>,
    /// New end (RFC 3339); must not precede `start`.
    pub end: DateTime<FixedOffset>,
    /// New all-day flag.
    #[serde(default)]
    pub all_day: bool,
    /// New location (cleared when absent/empty).
    #[serde(default)]
    pub location: Option<String>,
    /// New body (cleared when absent/empty).
    #[serde(default)]
    pub body: Option<String>,
    /// New recurrence rule (cleared when absent/empty).
    #[serde(default)]
    pub rrule: Option<String>,
    /// New category labels (replaces the prior set; empty clears them).
    #[serde(default)]
    pub labels: Vec<String>,
    /// New attachments (replaces the prior set; empty clears them).
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

async fn update_event(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<EventId>,
    Json(req): Json<UpdateEvent>,
) -> ApiResult<Json<Event>> {
    auth.require(Action::Write, "calendar")?;
    let ws = auth.principal().workspace_id;
    let summary = req.summary.trim();
    if summary.is_empty() {
        return Err(ApiError::bad_request("event summary must not be empty"));
    }
    // All-day endpoints pin to midnight UTC of the written date; timed ones
    // collapse to their UTC instant (see `normalize_event_span`).
    let (start, end) =
        crate::calendar_writeback::normalize_event_span(req.start, req.end, req.all_day);
    if end < start {
        return Err(ApiError::bad_request("`end` must not precede `start`"));
    }
    // The event must exist in this workspace and live on a writable calendar
    // before we touch it.
    let existing = state
        .store()
        .events()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let target = event_write_target(&state, ws, existing.calendar_id).await?;

    let labels = clean_labels(&req.labels);
    let attachments = clean_attachments(req.attachments);

    let event = match target {
        EventWriteTarget::Local(_) => {
            state
                .store()
                .events()
                .update(
                    ws,
                    id,
                    &EventPatch {
                        starts_at: start,
                        ends_at: end,
                        all_day: req.all_day,
                        summary,
                        location: clean(req.location.as_ref()),
                        body: clean(req.body.as_ref()),
                        labels: &labels,
                        attachments: &attachments,
                        rrule: clean(req.rrule.as_ref()),
                    },
                )
                .await?
        }
        // Provider write-back: push the merged full state, mirror the result.
        EventWriteTarget::Provider { provider, .. } => {
            let merged = merge_event_update(
                &existing,
                summary,
                start,
                end,
                req.all_day,
                clean(req.location.as_ref()),
                clean(req.body.as_ref()),
                clean(req.rrule.as_ref()),
                labels,
                attachments,
            );
            update_on_provider(state.store(), &provider, &merged).await?
        }
    };
    // Re-project so the graph reflects the edited summary/location/labels.
    state.enqueue_event_projection(ws, event.id).await;
    Ok(Json(event))
}

async fn delete_event(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<EventId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Delete, "calendar")?;
    let ws = auth.principal().workspace_id;
    let existing = state
        .store()
        .events()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    match event_write_target(&state, ws, existing.calendar_id).await? {
        EventWriteTarget::Local(_) => {
            state.store().events().delete(ws, id).await?;
        }
        // Provider write-back: delete there first (idempotent on already-gone),
        // then drop the local mirror.
        EventWriteTarget::Provider { provider, .. } => {
            delete_on_provider(state.store(), ws, &provider, &existing).await?;
        }
    }
    // Reconcile the projection: the worker finds the event gone and purges its
    // `:Event` node (SOUL §6.3/§8). Best-effort; never fails the delete.
    state.enqueue_event_projection(ws, id).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// Query for `GET /events`. All fields optional: the workspace's events,
/// narrowable by date range (`starts_at >= from`, `starts_at < to`) and/or a
/// single calendar, and bounded by `limit` (the most recent matches, presented
/// ascending — see [`DEFAULT_EVENT_LIMIT`](catalerum_store::DEFAULT_EVENT_LIMIT)).
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    /// Lower bound (inclusive) on event start, RFC 3339.
    #[serde(default)]
    pub from: Option<DateTime<Utc>>,
    /// Upper bound (exclusive) on event start, RFC 3339.
    #[serde(default)]
    pub to: Option<DateTime<Utc>>,
    /// Restrict to a single calendar.
    #[serde(default)]
    pub calendar_id: Option<CalendarId>,
    /// Max events to return; clamps to `[1, DEFAULT_EVENT_LIMIT]`, default cap.
    #[serde(default)]
    pub limit: Option<u32>,
}

async fn list_events(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Json<Vec<Event>>> {
    auth.require(Action::Read, "calendar")?;
    let ws = auth.principal().workspace_id;
    if let (Some(from), Some(to)) = (query.from, query.to) {
        if to < from {
            return Err(ApiError::bad_request("`to` must not precede `from`"));
        }
    }
    // If a calendar filter is given, confirm it belongs to the workspace so a
    // cross-workspace id can never silently widen the (empty) result set.
    if let Some(calendar_id) = query.calendar_id {
        state
            .store()
            .calendars()
            .get(ws, calendar_id)
            .await
            .map_err(|_| ApiError::NotFound)?;
    }
    let range = DateRange {
        from: query.from,
        to: query.to,
    };
    let limit = i64::from(
        query
            .limit
            .map(|n| n.clamp(1, DEFAULT_EVENT_LIMIT as u32))
            .unwrap_or(DEFAULT_EVENT_LIMIT as u32),
    );
    let events = state
        .store()
        .events()
        .list_by_workspace(ws, query.calendar_id, range, limit)
        .await?;
    Ok(Json(events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_kind_deserializes_snake_case() {
        let body: CreateConnection = serde_json::from_str(
            r#"{"kind":"caldav","name":"work","config":{"base_url":"https://dav.example/cal"}}"#,
        )
        .unwrap();
        assert_eq!(body.kind, CalendarProviderKind::Caldav);
        assert_eq!(body.name, "work");
        assert!(body.credentials.is_none());
    }

    #[test]
    fn provider_kind_tokens_are_stable() {
        assert_eq!(CalendarProviderKind::Local.as_str(), "local");
        assert_eq!(CalendarProviderKind::Caldav.as_str(), "caldav");
        assert_eq!(CalendarProviderKind::Webcal.as_str(), "webcal");
    }

    #[test]
    fn build_calendar_config_requires_dir_for_local_and_stamps_provider() {
        let ok = serde_json::json!({"dir": "/srv/cal"});
        let cfg = build_calendar_config(CalendarProviderKind::Local, ok).unwrap();
        assert_eq!(cfg["provider"], "local");
        assert_eq!(cfg["dir"], "/srv/cal");

        let missing = serde_json::json!({});
        assert!(build_calendar_config(CalendarProviderKind::Local, missing).is_err());

        let empty = serde_json::json!({"dir": "  "});
        assert!(build_calendar_config(CalendarProviderKind::Local, empty).is_err());
    }

    #[test]
    fn build_calendar_config_requires_base_url_for_remote() {
        let ok = serde_json::json!({"base_url": "https://dav.example/cal"});
        let cfg = build_calendar_config(CalendarProviderKind::Caldav, ok.clone()).unwrap();
        assert_eq!(cfg["provider"], "caldav");
        assert!(build_calendar_config(CalendarProviderKind::Webcal, ok).is_ok());

        let missing = serde_json::json!({"dir": "/srv/cal"});
        assert!(build_calendar_config(CalendarProviderKind::Caldav, missing.clone()).is_err());
        assert!(build_calendar_config(CalendarProviderKind::Webcal, missing).is_err());
    }

    fn parse_events_query(query: &str) -> EventsQuery {
        // Drive the same `Query` extractor axum uses, so the test exercises the
        // real query-decoding path without taking a direct dep on the codec.
        let uri: axum::http::Uri = format!("/events?{query}").parse().unwrap();
        Query::<EventsQuery>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn events_query_parses_optional_fields() {
        // Empty query -> all None.
        let q = parse_events_query("");
        assert!(q.from.is_none() && q.to.is_none() && q.calendar_id.is_none());

        let q = parse_events_query("from=2026-06-01T00:00:00Z&to=2026-07-01T00:00:00Z");
        assert!(q.from.is_some() && q.to.is_some());

        let id = CalendarId::new();
        let q = parse_events_query(&format!("calendar_id={id}"));
        assert_eq!(q.calendar_id, Some(id));
    }

    #[test]
    fn sync_payload_round_trips() {
        let ws = catalerum_core::WorkspaceId::new();
        let id = ConnectionId::new();
        let payload = serde_json::to_value(SyncCalendarPayload {
            workspace_id: Some(ws),
            connection_id: id,
        })
        .unwrap();
        // Both ids land on the wire so the ingest worker has an authoritative
        // sync scope without re-reading the job row.
        assert_eq!(
            payload.get("workspace_id").and_then(|v| v.as_str()),
            Some(ws.to_string().as_str())
        );
        let back: SyncCalendarPayload = serde_json::from_value(payload).unwrap();
        assert_eq!(back.connection_id, id);
        assert_eq!(back.workspace_id, Some(ws));
    }

    #[test]
    fn clean_trims_and_treats_blank_as_absent() {
        assert_eq!(clean(Some(&"  hi ".to_string())), Some("hi"));
        assert_eq!(clean(Some(&"   ".to_string())), None);
        assert_eq!(clean(None), None);
    }

    #[test]
    fn clean_labels_trims_drops_blanks_and_dedups_case_insensitively() {
        let labels = vec![
            " Work ".to_string(),
            "work".to_string(), // dup of "Work" (case-insensitive)
            "  ".to_string(),   // blank → dropped
            "Travel".to_string(),
        ];
        // First-seen casing kept ("Work"), dup + blank removed.
        assert_eq!(
            clean_labels(&labels),
            vec!["Work".to_string(), "Travel".to_string()]
        );
    }

    #[test]
    fn clean_attachments_drops_blank_url_and_trims_fields() {
        let atts = vec![
            Attachment {
                url: "  https://example.com/a.pdf  ".to_string(),
                filename: Some("  a.pdf ".to_string()),
                content_type: Some("  ".to_string()), // blank → None
                size: Some(10),
            },
            Attachment {
                url: "   ".to_string(), // blank url → dropped entirely
                filename: Some("ghost".to_string()),
                content_type: None,
                size: None,
            },
        ];
        let out = clean_attachments(atts);
        assert_eq!(out.len(), 1, "blank-url attachment dropped");
        assert_eq!(out[0].url, "https://example.com/a.pdf");
        assert_eq!(out[0].filename.as_deref(), Some("a.pdf"));
        assert_eq!(out[0].content_type, None, "blank content_type → None");
        assert_eq!(out[0].size, Some(10));
    }

    #[test]
    fn create_event_decodes_labels_and_attachments() {
        let cal = CalendarId::new();
        let req: CreateEvent = serde_json::from_str(&format!(
            r#"{{"calendar_id":"{cal}","summary":"Trip","start":"2026-06-18T09:00:00Z",
                "end":"2026-06-18T10:00:00Z","labels":["travel","fun"],
                "attachments":[{{"url":"https://x/itin.pdf","filename":"itin.pdf","content_type":"application/pdf"}}]}}"#
        ))
        .unwrap();
        assert_eq!(req.labels, vec!["travel".to_string(), "fun".to_string()]);
        assert_eq!(req.attachments.len(), 1);
        assert_eq!(req.attachments[0].url, "https://x/itin.pdf");
        // Absent labels/attachments default to empty (back-compat with old clients).
        let bare: CreateEvent = serde_json::from_str(&format!(
            r#"{{"calendar_id":"{cal}","summary":"X","start":"2026-06-18T09:00:00Z","end":"2026-06-18T10:00:00Z"}}"#
        ))
        .unwrap();
        assert!(bare.labels.is_empty() && bare.attachments.is_empty());
    }

    #[test]
    fn update_event_decodes_labels_and_attachments() {
        let req: UpdateEvent = serde_json::from_str(
            r#"{"summary":"Sync","start":"2026-06-18T10:00:00Z","end":"2026-06-18T11:00:00Z",
                "labels":["work"],"attachments":[{"url":"/storage/objects/events/x.png","content_type":"image/png"}]}"#,
        )
        .unwrap();
        assert_eq!(req.labels, vec!["work".to_string()]);
        assert_eq!(
            req.attachments[0].content_type.as_deref(),
            Some("image/png")
        );
    }

    #[test]
    fn create_event_decodes_with_defaults() {
        let cal = CalendarId::new();
        let req: CreateEvent = serde_json::from_str(&format!(
            r#"{{"calendar_id":"{cal}","summary":"Standup","start":"2026-06-18T09:00:00Z","end":"2026-06-18T09:30:00Z"}}"#
        ))
        .unwrap();
        assert_eq!(req.calendar_id, cal);
        assert_eq!(req.summary, "Standup");
        assert!(!req.all_day);
        assert!(req.location.is_none() && req.body.is_none() && req.rrule.is_none());
        assert!(req.end >= req.start);
    }

    #[test]
    fn all_day_offset_start_stays_on_written_date() {
        use chrono::TimeZone;
        // A Berlin (+02:00) all-day event for the 7th must normalize to
        // 2026-07-07T00:00:00Z, not the previous UTC day — the week/month
        // off-by-one this route now prevents at the source.
        let cal = CalendarId::new();
        let req: CreateEvent = serde_json::from_str(&format!(
            r#"{{"calendar_id":"{cal}","summary":"Holiday",
                 "start":"2026-07-07T00:00:00+02:00","end":"2026-07-08T00:00:00+02:00",
                 "all_day":true}}"#
        ))
        .unwrap();
        let (start, end) =
            crate::calendar_writeback::normalize_event_span(req.start, req.end, req.all_day);
        assert_eq!(start, Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0).unwrap());
        assert_eq!(end, Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap());
    }

    #[test]
    fn create_calendar_requires_only_a_name() {
        let req: CreateCalendar = serde_json::from_str(r#"{"name":"Personal"}"#).unwrap();
        assert_eq!(req.name, "Personal");
    }

    #[test]
    fn update_event_decodes_optional_fields() {
        let req: UpdateEvent = serde_json::from_str(
            r#"{"summary":"Sync","start":"2026-06-18T10:00:00Z","end":"2026-06-18T11:00:00Z","all_day":true,"location":"Room 2"}"#,
        )
        .unwrap();
        assert_eq!(req.summary, "Sync");
        assert!(req.all_day);
        assert_eq!(req.location.as_deref(), Some("Room 2"));
        assert!(req.rrule.is_none());
    }

    #[test]
    fn sync_payload_decodes_connection_only_shape() {
        // The worker also accepts a payload with no workspace_id (resolving it
        // from the job row); the API's type must deserialize that shape too so
        // the two sides share one contract.
        let id = ConnectionId::new();
        let payload = serde_json::json!({ "connection_id": id });
        let back: SyncCalendarPayload = serde_json::from_value(payload).unwrap();
        assert_eq!(back.connection_id, id);
        assert_eq!(back.workspace_id, None);
    }
}
