use catalerum_core::id::{CalendarId, EventId, WorkspaceId};
use catalerum_core::model::{Attachment, EntityRef, Event};
use chrono::{DateTime, Utc};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::EventRow;
use super::{map, PgPool};

// ===========================================================================
// Events
// ===========================================================================

/// The full set of `events` columns, in row order.
const EVENT_COLS: &str = "id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day, \
     rrule, summary, location, body, attendees, labels, attachments, etag, sequence, \
     created_at, updated_at";

/// Default ceiling on how many events one [`EventRepo::list_by_workspace`] call
/// returns. A workspace that syncs a long-lived CalDAV calendar can accumulate
/// thousands of events; an unbounded list would balloon the API payload and the
/// client agenda. The list is bounded to this many by start (ascending);
/// callers that need a narrower view pass a date range. Normal-sized calendars
/// fall well under it and are unaffected.
pub const DEFAULT_EVENT_LIMIT: i64 = 2000;

/// An event to upsert. The id is assigned by the store on first insert and
/// preserved across updates (the conflict key is `(calendar_id, uid)`).
#[derive(Clone, Debug)]
pub struct UpsertEvent<'a> {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning calendar.
    pub calendar_id: CalendarId,
    /// iCalendar `UID` (stable across edits; the idempotency key).
    pub uid: &'a str,
    /// Event start.
    pub starts_at: DateTime<Utc>,
    /// Event end.
    pub ends_at: DateTime<Utc>,
    /// All-day flag.
    pub all_day: bool,
    /// RFC 5545 recurrence rule, verbatim.
    pub rrule: Option<&'a str>,
    /// Title / summary.
    pub summary: &'a str,
    /// Location.
    pub location: Option<&'a str>,
    /// Free-text description / body.
    pub body: Option<&'a str>,
    /// Resolved attendees as typed entity pointers.
    pub attendees: &'a [EntityRef],
    /// Category labels (iCalendar `CATEGORIES`).
    pub labels: &'a [String],
    /// File / image attachments (iCalendar `ATTACH`).
    pub attachments: &'a [Attachment],
    /// Provider ETag for idempotent incremental sync (SOUL §3.4).
    pub etag: Option<&'a str>,
    /// iCalendar `SEQUENCE` for conflict resolution.
    pub sequence: i32,
}

impl<'a> UpsertEvent<'a> {
    /// A minimal upsert: required fields only, everything else empty/default.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &'a str,
        summary: &'a str,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    ) -> Self {
        Self {
            workspace_id,
            calendar_id,
            uid,
            starts_at,
            ends_at,
            all_day: false,
            rrule: None,
            summary,
            location: None,
            body: None,
            attendees: &[],
            labels: &[],
            attachments: &[],
            etag: None,
            sequence: 0,
        }
    }
}

/// An inclusive-start / exclusive-end time window to filter events by.
#[derive(Clone, Copy, Debug, Default)]
pub struct DateRange {
    /// Lower bound: only events with `starts_at >= from` (if set).
    pub from: Option<DateTime<Utc>>,
    /// Upper bound: only events with `starts_at < to` (if set).
    pub to: Option<DateTime<Utc>>,
}

/// The editable fields of an existing event, for a direct user/agent edit on a
/// local calendar ([`EventRepo::update`]). The `uid`, `etag`, and `attendees`
/// are *not* edited here: `uid` is the immutable identity, and `etag`/attendee
/// resolution belong to the sync/graph paths, not a hand edit. `SEQUENCE` is
/// bumped automatically by the update.
#[derive(Clone, Debug)]
pub struct EventPatch<'a> {
    /// New start.
    pub starts_at: DateTime<Utc>,
    /// New end.
    pub ends_at: DateTime<Utc>,
    /// New all-day flag.
    pub all_day: bool,
    /// New title / summary.
    pub summary: &'a str,
    /// New location (cleared when `None`).
    pub location: Option<&'a str>,
    /// New free-text body (cleared when `None`).
    pub body: Option<&'a str>,
    /// New category labels (replaces the prior set; empty clears them).
    pub labels: &'a [String],
    /// New file / image attachments (replaces the prior set; empty clears them).
    pub attachments: &'a [Attachment],
    /// New RFC 5545 recurrence rule (cleared when `None`).
    pub rrule: Option<&'a str>,
}

/// CRUD for the `events` table (SOUL §6.1/§8/§10). Events upsert by
/// `(calendar_id, uid)` so incremental sync is idempotent (SOUL §3.4):
/// re-running a sync never duplicates and updates land in place.
#[derive(Clone, Debug)]
pub struct EventRepo {
    pool: PgPool,
}

impl EventRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert or update an event by `(calendar_id, uid)` (SOUL §3.4). On
    /// conflict every mutable field is refreshed and `updated_at` bumped; the
    /// row id is preserved. Returns the stored event.
    pub async fn upsert_by_uid(&self, event: &UpsertEvent<'_>) -> Result<Event> {
        let id = EventId::new().into_uuid();
        let row: EventRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO events
                 (id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day,
                  rrule, summary, location, body, attendees, labels, attachments, etag, sequence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             ON CONFLICT (calendar_id, uid) DO UPDATE SET
                 starts_at   = EXCLUDED.starts_at,
                 ends_at     = EXCLUDED.ends_at,
                 all_day     = EXCLUDED.all_day,
                 rrule       = EXCLUDED.rrule,
                 summary     = EXCLUDED.summary,
                 location    = EXCLUDED.location,
                 body        = EXCLUDED.body,
                 attendees   = EXCLUDED.attendees,
                 labels      = EXCLUDED.labels,
                 attachments = EXCLUDED.attachments,
                 etag        = EXCLUDED.etag,
                 sequence    = EXCLUDED.sequence,
                 updated_at  = CURRENT_TIMESTAMP
             RETURNING {EVENT_COLS}"
        ))
        .bind(id)
        .bind(event.workspace_id.into_uuid())
        .bind(event.calendar_id.into_uuid())
        .bind(event.uid)
        .bind(event.starts_at)
        .bind(event.ends_at)
        .bind(event.all_day)
        .bind(event.rrule)
        .bind(event.summary)
        .bind(event.location)
        .bind(event.body)
        .bind(Json(event.attendees.to_vec()))
        .bind(Json(event.labels.to_vec()))
        .bind(Json(event.attachments.to_vec()))
        .bind(event.etag)
        .bind(event.sequence)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Insert a brand-new event (a direct create on a local calendar, SOUL
    /// §8/§11), assigning it a fresh id. Unlike [`upsert_by_uid`] this never
    /// updates an existing row: the caller supplies a freshly-minted `uid`
    /// (e.g. a UUID), so the `(calendar_id, uid)` key cannot collide — a
    /// collision surfaces as [`StoreError::Conflict`] rather than silently
    /// overwriting. The target calendar's writability is enforced by the caller
    /// (the API/tool rejects a read-only / provider calendar).
    ///
    /// [`upsert_by_uid`]: EventRepo::upsert_by_uid
    pub async fn create(&self, event: &UpsertEvent<'_>) -> Result<Event> {
        let id = EventId::new().into_uuid();
        let row: EventRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO events
                 (id, workspace_id, calendar_id, uid, starts_at, ends_at, all_day,
                  rrule, summary, location, body, attendees, labels, attachments, etag, sequence)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
             RETURNING {EVENT_COLS}"
        ))
        .bind(id)
        .bind(event.workspace_id.into_uuid())
        .bind(event.calendar_id.into_uuid())
        .bind(event.uid)
        .bind(event.starts_at)
        .bind(event.ends_at)
        .bind(event.all_day)
        .bind(event.rrule)
        .bind(event.summary)
        .bind(event.location)
        .bind(event.body)
        .bind(Json(event.attendees.to_vec()))
        .bind(Json(event.labels.to_vec()))
        .bind(Json(event.attachments.to_vec()))
        .bind(event.etag)
        .bind(event.sequence)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Apply a direct edit to an existing event by id (SOUL §8/§11),
    /// workspace-scoped. Bumps `SEQUENCE` and `updated_at`; the immutable `uid`
    /// is preserved. Returns [`StoreError::NotFound`] if no such event exists in
    /// the workspace. Caller enforces that the owning calendar is writable.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: EventId,
        patch: &EventPatch<'_>,
    ) -> Result<Event> {
        let row: EventRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE events SET
                 starts_at   = $3,
                 ends_at     = $4,
                 all_day     = $5,
                 summary     = $6,
                 location    = $7,
                 body        = $8,
                 rrule       = $9,
                 labels      = $10,
                 attachments = $11,
                 sequence    = sequence + 1,
                 updated_at  = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {EVENT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(patch.starts_at)
        .bind(patch.ends_at)
        .bind(patch.all_day)
        .bind(patch.summary)
        .bind(patch.location)
        .bind(patch.body)
        .bind(patch.rrule)
        .bind(Json(patch.labels.to_vec()))
        .bind(Json(patch.attachments.to_vec()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Delete a single event by its id, workspace-scoped (a direct delete on a
    /// local calendar). Returns [`StoreError::NotFound`] if no row was removed.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: EventId) -> Result<()> {
        let res = sqlx::query("DELETE FROM events WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Fetch an event, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: EventId) -> Result<Event> {
        let row: EventRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {EVENT_COLS} FROM events
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an event by `(calendar_id, uid)`, workspace-scoped — the calendar twin of
    /// [`EmailRepo::get_by_uid`](crate::repo::EmailRepo::get_by_uid). `StoreError::NotFound`
    /// when no such event exists; used by `WriteEvent` to probe whether an upsert is a
    /// first write vs an idempotent redelivery of an already-stored event (SOUL §11/§29).
    pub async fn get_by_uid(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &str,
    ) -> Result<Event> {
        let row: EventRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1 AND calendar_id = $2 AND uid = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.into_uuid())
        .bind(uid)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's events, ordered by start time ascending, with an
    /// optional date range (`starts_at` window) and an optional single-calendar
    /// filter. The query is always workspace-filtered.
    ///
    /// `limit` bounds the result to the first `limit` events by start (a `limit`
    /// below 1 is floored to 1) — so an open-ended query against a long-lived
    /// calendar can't return an unbounded set (see [`DEFAULT_EVENT_LIMIT`]).
    /// Callers that want a specific slice (e.g. "soonest from now") pass a `from`
    /// bound and the count they need; reconciliation that must see *every* row
    /// passes `i64::MAX`.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: Option<CalendarId>,
        range: DateRange,
        limit: i64,
    ) -> Result<Vec<Event>> {
        // Bind every optional filter and gate it with an `IS NULL OR` clause so
        // the SQL text stays constant (no dynamic string building).
        let rows: Vec<EventRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1
               AND ($2::uuid        IS NULL OR calendar_id = $2)
               AND ($3::timestamptz IS NULL OR starts_at  >= $3)
               AND ($4::timestamptz IS NULL OR starts_at  <  $4)
             ORDER BY starts_at ASC, id ASC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.map(CalendarId::into_uuid))
        .bind(range.from)
        .bind(range.to)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Event::from).collect())
    }

    /// Search events by the **content** of their summary, location, body, or an
    /// attendee's display name — `query` matched as a **literal case-insensitive
    /// substring** (`strpos`, not `LIKE`, so a user's `%`/`_` are literal),
    /// workspace-scoped, `LIMIT`-bounded (floored at 1). Unlike
    /// [`Self::list_by_workspace`] the order is `starts_at` **descending** (most
    /// recent first) so past events are as reachable as future ones — "when did
    /// I last meet X" surfaces the latest match first; an optional [`DateRange`]
    /// still narrows the window. A blank query returns nothing (no "match
    /// everything"). The content-search complement to `list_by_workspace`'s
    /// date filtering — drives the `search_events` agent tool.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        range: DateRange,
        limit: i64,
    ) -> Result<Vec<Event>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Attendees are a JSONB array of `EntityRef`s; match only their
        // `display_name` values (not the raw JSON text, whose constant keys /
        // UUIDs would false-positive).
        let rows: Vec<EventRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {EVENT_COLS} FROM events
             WHERE workspace_id = $1
               AND ($3::timestamptz IS NULL OR starts_at >= $3)
               AND ($4::timestamptz IS NULL OR starts_at <  $4)
               AND (strpos(lower(summary), lower($2)) > 0
                 OR strpos(lower(coalesce(location, '')), lower($2)) > 0
                 OR strpos(lower(coalesce(body, '')), lower($2)) > 0
                 OR EXISTS (SELECT 1 FROM jsonb_array_elements(attendees) a
                            WHERE strpos(lower(coalesce(a->>'display_name', '')), lower($2)) > 0))
             ORDER BY starts_at DESC, id DESC
             LIMIT $5"
        ))
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(range.from)
        .bind(range.to)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Event::from).collect())
    }

    /// Delete every event of a calendar, workspace-scoped. Returns the count
    /// removed. Used when a calendar is reset / removed from sync.
    pub async fn delete_by_calendar(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
    ) -> Result<u64> {
        let res = sqlx::query("DELETE FROM events WHERE workspace_id = $1 AND calendar_id = $2")
            .bind(workspace_id.into_uuid())
            .bind(calendar_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Delete a single event by its provider `uid` within a calendar,
    /// workspace-scoped. Used to apply provider deletions during incremental
    /// sync. Returns `true` if a row was removed.
    pub async fn delete_by_uid(
        &self,
        workspace_id: WorkspaceId,
        calendar_id: CalendarId,
        uid: &str,
    ) -> Result<bool> {
        let res = sqlx::query(
            "DELETE FROM events
             WHERE workspace_id = $1 AND calendar_id = $2 AND uid = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(calendar_id.into_uuid())
        .bind(uid)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}
