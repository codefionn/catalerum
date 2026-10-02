//! Weekly plans — reusable week templates applied to calendars (SOUL §8).
//!
//! Rows + repository for `weekly_plans`, `weekly_plan_entries`, and the
//! `weekly_plan_events` link table (migration `0070`). Kept in its own module
//! (rather than growing `repo.rs`/`rows.rs`) since the three tables form one
//! self-contained aggregate. Every query is workspace-filtered (SOUL §6.1/§18)
//! and uses the runtime sqlx API, portable across the Postgres and SQLite
//! backends.
//!
//! The repository only persists plans and links; *applying* a plan (computing
//! instants in the plan's timezone and writing events through the local /
//! provider write-back seam) lives in the API crate, which owns the provider
//! wiring.

use catalerum_core::model::{WeeklyPlan, WeeklyPlanApplication, WeeklyPlanEntry};
use catalerum_core::{CalendarId, EventId, WeeklyPlanEntryId, WeeklyPlanId, WorkspaceId};
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::types::Json;
use uuid::Uuid;

use crate::error::{Result, StoreError};
use crate::DbPool as PgPool;

const PLAN_COLS: &str =
    "id, workspace_id, name, description, calendar_id, timezone, created_at, updated_at";
const ENTRY_COLS: &str = "id, plan_id, weekday, start_minute, end_minute, all_day, summary, \
     location, body, labels, calendar_id";
const LINK_COLS: &str = "event_id, plan_id, entry_id, week_start";

/// Default cap on [`WeeklyPlanRepo::list_event_links`] (every read is bounded,
/// SOUL §18).
pub const DEFAULT_PLAN_LINK_LIMIT: i64 = 5000;

fn map(err: sqlx::Error) -> StoreError {
    StoreError::from_sqlx(err)
}

/// Row mirror of `weekly_plans` (entries are assembled separately).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WeeklyPlanRow {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub calendar_id: Option<Uuid>,
    pub timezone: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Row mirror of `weekly_plan_entries`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WeeklyPlanEntryRow {
    pub id: Uuid,
    pub plan_id: Uuid,
    pub weekday: i32,
    pub start_minute: i32,
    pub end_minute: i32,
    pub all_day: bool,
    pub summary: String,
    pub location: Option<String>,
    pub body: Option<String>,
    pub labels: Json<Vec<String>>,
    pub calendar_id: Option<Uuid>,
}

impl From<WeeklyPlanEntryRow> for WeeklyPlanEntry {
    fn from(r: WeeklyPlanEntryRow) -> Self {
        WeeklyPlanEntry {
            id: WeeklyPlanEntryId::from_uuid(r.id),
            plan_id: WeeklyPlanId::from_uuid(r.plan_id),
            weekday: r.weekday,
            start_minute: r.start_minute,
            end_minute: r.end_minute,
            all_day: r.all_day,
            summary: r.summary,
            location: r.location,
            body: r.body,
            labels: r.labels.0,
            calendar_id: r.calendar_id.map(CalendarId::from_uuid),
        }
    }
}

fn plan_from_parts(row: WeeklyPlanRow, entries: Vec<WeeklyPlanEntry>) -> WeeklyPlan {
    WeeklyPlan {
        id: WeeklyPlanId::from_uuid(row.id),
        workspace_id: WorkspaceId::from_uuid(row.workspace_id),
        name: row.name,
        description: row.description,
        calendar_id: row.calendar_id.map(CalendarId::from_uuid),
        timezone: row.timezone,
        entries,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// Row mirror of `weekly_plan_events` — the link between a materialised
/// calendar event and the plan entry + week it came from.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanEventLinkRow {
    pub event_id: Uuid,
    pub plan_id: Uuid,
    pub entry_id: Option<Uuid>,
    pub week_start: NaiveDate,
}

/// A materialised event's link back to its plan (see [`PlanEventLinkRow`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PlanEventLink {
    pub event_id: EventId,
    pub plan_id: WeeklyPlanId,
    /// `None` once the entry was deleted from the plan (the event is then an
    /// orphan the next re-apply of its week removes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<WeeklyPlanEntryId>,
    /// The Monday of the week the event was applied to.
    pub week_start: NaiveDate,
}

impl From<PlanEventLinkRow> for PlanEventLink {
    fn from(r: PlanEventLinkRow) -> Self {
        PlanEventLink {
            event_id: EventId::from_uuid(r.event_id),
            plan_id: WeeklyPlanId::from_uuid(r.plan_id),
            entry_id: r.entry_id.map(WeeklyPlanEntryId::from_uuid),
            week_start: r.week_start,
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct ApplicationRow {
    week_start: NaiveDate,
    event_count: i64,
    applied_at: DateTime<Utc>,
}

/// The editable fields of a plan header.
#[derive(Clone, Debug)]
pub struct PlanInput<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub calendar_id: Option<CalendarId>,
    pub timezone: &'a str,
}

/// The editable fields of a plan entry. Callers validate the slot first
/// ([`WeeklyPlanEntry::validate_slot`]); the table's CHECKs are the backstop.
#[derive(Clone, Debug)]
pub struct PlanEntryInput<'a> {
    pub weekday: i32,
    pub start_minute: i32,
    pub end_minute: i32,
    pub all_day: bool,
    pub summary: &'a str,
    pub location: Option<&'a str>,
    pub body: Option<&'a str>,
    pub labels: &'a [String],
    pub calendar_id: Option<CalendarId>,
}

impl<'a> PlanEntryInput<'a> {
    /// Borrow an existing entry's fields (e.g. to copy it into another plan).
    #[must_use]
    pub fn from_entry(e: &'a WeeklyPlanEntry) -> Self {
        Self {
            weekday: e.weekday,
            start_minute: e.start_minute,
            end_minute: e.end_minute,
            all_day: e.all_day,
            summary: &e.summary,
            location: e.location.as_deref(),
            body: e.body.as_deref(),
            labels: &e.labels,
            calendar_id: e.calendar_id,
        }
    }
}

/// CRUD for weekly plans, their entries, and the plan ⇄ event links.
#[derive(Clone, Debug)]
pub struct WeeklyPlanRepo {
    pool: PgPool,
}

impl WeeklyPlanRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // --- plans -------------------------------------------------------------

    /// Create an empty plan.
    pub async fn create(&self, workspace_id: WorkspaceId, input: &PlanInput<'_>) -> Result<WeeklyPlan> {
        let row: WeeklyPlanRow = sqlx::query_as(&format!(
            "INSERT INTO weekly_plans (id, workspace_id, name, description, calendar_id, timezone)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {PLAN_COLS}"
        ))
        .bind(WeeklyPlanId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(input.name)
        .bind(input.description)
        .bind(input.calendar_id.map(CalendarId::into_uuid))
        .bind(input.timezone)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(plan_from_parts(row, Vec::new()))
    }

    /// Fetch a plan with its entries (weekday, then start order).
    pub async fn get(&self, workspace_id: WorkspaceId, id: WeeklyPlanId) -> Result<WeeklyPlan> {
        let row: WeeklyPlanRow = sqlx::query_as(&format!(
            "SELECT {PLAN_COLS} FROM weekly_plans WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let entries = self.entries_of(workspace_id, id).await?;
        Ok(plan_from_parts(row, entries))
    }

    /// List the workspace's plans (by name), each with its entries.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<WeeklyPlan>> {
        let rows: Vec<WeeklyPlanRow> = sqlx::query_as(&format!(
            "SELECT {PLAN_COLS} FROM weekly_plans WHERE workspace_id = $1
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        // One query for every entry in the workspace, grouped in memory.
        let entries: Vec<WeeklyPlanEntryRow> = sqlx::query_as(&format!(
            "SELECT {ENTRY_COLS} FROM weekly_plan_entries WHERE workspace_id = $1
             ORDER BY weekday ASC, all_day DESC, start_minute ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        let mut by_plan: std::collections::HashMap<Uuid, Vec<WeeklyPlanEntry>> =
            std::collections::HashMap::new();
        for e in entries {
            by_plan.entry(e.plan_id).or_default().push(e.into());
        }
        Ok(rows
            .into_iter()
            .map(|r| {
                let entries = by_plan.remove(&r.id).unwrap_or_default();
                plan_from_parts(r, entries)
            })
            .collect())
    }

    /// Replace a plan's header fields. `NotFound` if absent.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: WeeklyPlanId,
        input: &PlanInput<'_>,
    ) -> Result<WeeklyPlan> {
        let row: WeeklyPlanRow = sqlx::query_as(&format!(
            "UPDATE weekly_plans SET
                 name = $3, description = $4, calendar_id = $5, timezone = $6,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {PLAN_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(input.name)
        .bind(input.description)
        .bind(input.calendar_id.map(CalendarId::into_uuid))
        .bind(input.timezone)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let entries = self.entries_of(workspace_id, id).await?;
        Ok(plan_from_parts(row, entries))
    }

    /// Delete a plan and its entries + links. The events it materialised stay
    /// (they are ordinary calendar events). `NotFound` if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: WeeklyPlanId) -> Result<()> {
        let res = sqlx::query("DELETE FROM weekly_plans WHERE id = $1 AND workspace_id = $2")
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

    /// Copy a plan (header + entries, not its applications) under a new name —
    /// the "start next week's plan from this one" flow.
    pub async fn duplicate(
        &self,
        workspace_id: WorkspaceId,
        id: WeeklyPlanId,
        name: &str,
    ) -> Result<WeeklyPlan> {
        let source = self.get(workspace_id, id).await?;
        let mut tx = self.pool.begin().await.map_err(map)?;
        let new_id = WeeklyPlanId::new();
        let row: WeeklyPlanRow = sqlx::query_as(&format!(
            "INSERT INTO weekly_plans (id, workspace_id, name, description, calendar_id, timezone)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {PLAN_COLS}"
        ))
        .bind(new_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(source.description.as_deref())
        .bind(source.calendar_id.map(CalendarId::into_uuid))
        .bind(&source.timezone)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;
        let mut entries = Vec::with_capacity(source.entries.len());
        for e in &source.entries {
            let input = PlanEntryInput::from_entry(e);
            let entry: WeeklyPlanEntryRow = insert_entry_query(workspace_id, new_id, &input)
                .fetch_one(&mut *tx)
                .await
                .map_err(map)?;
            entries.push(entry.into());
        }
        tx.commit().await.map_err(map)?;
        Ok(plan_from_parts(row, entries))
    }

    // --- entries -----------------------------------------------------------

    async fn entries_of(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
    ) -> Result<Vec<WeeklyPlanEntry>> {
        let rows: Vec<WeeklyPlanEntryRow> = sqlx::query_as(&format!(
            "SELECT {ENTRY_COLS} FROM weekly_plan_entries
             WHERE plan_id = $1 AND workspace_id = $2
             ORDER BY weekday ASC, all_day DESC, start_minute ASC, id ASC"
        ))
        .bind(plan_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(WeeklyPlanEntry::from).collect())
    }

    /// Bump a plan's `updated_at` after an entry change.
    async fn touch(&self, workspace_id: WorkspaceId, plan_id: WeeklyPlanId) -> Result<()> {
        sqlx::query(
            "UPDATE weekly_plans SET updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(plan_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Add an entry to a plan. `NotFound` if the plan is not in the workspace.
    pub async fn add_entry(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        input: &PlanEntryInput<'_>,
    ) -> Result<WeeklyPlanEntry> {
        // Existence check first so a foreign plan id is NotFound, not an FK error.
        sqlx::query("SELECT id FROM weekly_plans WHERE id = $1 AND workspace_id = $2")
            .bind(plan_id.into_uuid())
            .bind(workspace_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        let row: WeeklyPlanEntryRow = insert_entry_query(workspace_id, plan_id, input)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        self.touch(workspace_id, plan_id).await?;
        Ok(row.into())
    }

    /// Replace an entry's fields. `NotFound` unless the entry belongs to
    /// `plan_id` in this workspace.
    pub async fn update_entry(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        entry_id: WeeklyPlanEntryId,
        input: &PlanEntryInput<'_>,
    ) -> Result<WeeklyPlanEntry> {
        let row: WeeklyPlanEntryRow = sqlx::query_as(&format!(
            "UPDATE weekly_plan_entries SET
                 weekday = $4, start_minute = $5, end_minute = $6, all_day = $7,
                 summary = $8, location = $9, body = $10, labels = $11, calendar_id = $12,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND plan_id = $2 AND workspace_id = $3
             RETURNING {ENTRY_COLS}"
        ))
        .bind(entry_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(input.weekday)
        .bind(input.start_minute)
        .bind(input.end_minute)
        .bind(input.all_day)
        .bind(input.summary)
        .bind(input.location)
        .bind(input.body)
        .bind(Json(input.labels.to_vec()))
        .bind(input.calendar_id.map(CalendarId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        self.touch(workspace_id, plan_id).await?;
        Ok(row.into())
    }

    /// Remove an entry. Its already-applied events keep a link with a NULL
    /// entry, so the next re-apply of their week removes them. `NotFound` unless
    /// the entry belongs to `plan_id` in this workspace.
    pub async fn delete_entry(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        entry_id: WeeklyPlanEntryId,
    ) -> Result<()> {
        let res = sqlx::query(
            "DELETE FROM weekly_plan_entries
             WHERE id = $1 AND plan_id = $2 AND workspace_id = $3",
        )
        .bind(entry_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        self.touch(workspace_id, plan_id).await
    }

    // --- plan ⇄ event links -------------------------------------------------

    /// Record (or re-point) the link for a materialised event. Upserts by
    /// `event_id`; an older link holding the same `(entry, week)` slot is
    /// cleared first so the unique slot index never conflicts.
    pub async fn link_event(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        entry_id: WeeklyPlanEntryId,
        event_id: EventId,
        week_start: NaiveDate,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        sqlx::query(
            "DELETE FROM weekly_plan_events
             WHERE workspace_id = $1 AND entry_id = $2 AND week_start = $3 AND event_id <> $4",
        )
        .bind(workspace_id.into_uuid())
        .bind(entry_id.into_uuid())
        .bind(week_start)
        .bind(event_id.into_uuid())
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        sqlx::query(
            "INSERT INTO weekly_plan_events (event_id, workspace_id, plan_id, entry_id, week_start)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (event_id) DO UPDATE SET
                 plan_id = EXCLUDED.plan_id, entry_id = EXCLUDED.entry_id,
                 week_start = EXCLUDED.week_start, updated_at = CURRENT_TIMESTAMP",
        )
        .bind(event_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(entry_id.into_uuid())
        .bind(week_start)
        .execute(&mut *tx)
        .await
        .map_err(map)?;
        tx.commit().await.map_err(map)?;
        Ok(())
    }

    /// Refresh the `updated_at` of a week's links (a re-apply with no changes
    /// still counts as "applied now").
    pub async fn touch_week(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        week_start: NaiveDate,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE weekly_plan_events SET updated_at = CURRENT_TIMESTAMP
             WHERE workspace_id = $1 AND plan_id = $2 AND week_start = $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(week_start)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Drop one event's link (the event itself is untouched).
    pub async fn unlink_event(&self, workspace_id: WorkspaceId, event_id: EventId) -> Result<()> {
        sqlx::query("DELETE FROM weekly_plan_events WHERE event_id = $1 AND workspace_id = $2")
            .bind(event_id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(())
    }

    /// Every link a plan holds in one week.
    pub async fn links_for_week(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
        week_start: NaiveDate,
    ) -> Result<Vec<PlanEventLink>> {
        let rows: Vec<PlanEventLinkRow> = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM weekly_plan_events
             WHERE workspace_id = $1 AND plan_id = $2 AND week_start = $3
             ORDER BY event_id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(week_start)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(PlanEventLink::from).collect())
    }

    /// Every link in the workspace (bounded, newest weeks first) — lets a
    /// calendar view mark which events came from which plan.
    pub async fn list_event_links(
        &self,
        workspace_id: WorkspaceId,
        limit: i64,
    ) -> Result<Vec<PlanEventLink>> {
        let rows: Vec<PlanEventLinkRow> = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM weekly_plan_events
             WHERE workspace_id = $1
             ORDER BY week_start DESC, event_id ASC
             LIMIT $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(PlanEventLink::from).collect())
    }

    /// The link of one event, if it was materialised by a plan.
    pub async fn link_for_event(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
    ) -> Result<Option<PlanEventLink>> {
        let row: Option<PlanEventLinkRow> = sqlx::query_as(&format!(
            "SELECT {LINK_COLS} FROM weekly_plan_events
             WHERE workspace_id = $1 AND event_id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(event_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(PlanEventLink::from))
    }

    /// The weeks a plan has been applied to (newest first), with how many
    /// linked events each still holds.
    pub async fn applications(
        &self,
        workspace_id: WorkspaceId,
        plan_id: WeeklyPlanId,
    ) -> Result<Vec<WeeklyPlanApplication>> {
        let rows: Vec<ApplicationRow> = sqlx::query_as(
            "SELECT week_start, COUNT(*) AS event_count, MAX(updated_at) AS applied_at
             FROM weekly_plan_events
             WHERE workspace_id = $1 AND plan_id = $2
             GROUP BY week_start
             ORDER BY week_start DESC",
        )
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(|r| WeeklyPlanApplication {
                plan_id,
                week_start: r.week_start,
                event_count: r.event_count,
                applied_at: r.applied_at,
            })
            .collect())
    }
}

/// The active backend's sqlx database type.
type Db = <crate::ActiveBackend as crate::RepositoryBackend>::Database;

/// The shared entry INSERT, usable on the pool or inside a transaction.
fn insert_entry_query<'q>(
    workspace_id: WorkspaceId,
    plan_id: WeeklyPlanId,
    input: &PlanEntryInput<'q>,
) -> sqlx::query::QueryAs<'q, Db, WeeklyPlanEntryRow, <Db as sqlx::Database>::Arguments<'q>> {
    sqlx::query_as(INSERT_ENTRY_SQL)
        .bind(WeeklyPlanEntryId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(plan_id.into_uuid())
        .bind(input.weekday)
        .bind(input.start_minute)
        .bind(input.end_minute)
        .bind(input.all_day)
        .bind(input.summary)
        .bind(input.location)
        .bind(input.body)
        .bind(Json(input.labels.to_vec()))
        .bind(input.calendar_id.map(CalendarId::into_uuid))
}

const INSERT_ENTRY_SQL: &str = "INSERT INTO weekly_plan_entries
     (id, workspace_id, plan_id, weekday, start_minute, end_minute, all_day,
      summary, location, body, labels, calendar_id)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
     RETURNING id, plan_id, weekday, start_minute, end_minute, all_day, summary,
               location, body, labels, calendar_id";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_entry_sql_returns_entry_cols() {
        let returning = INSERT_ENTRY_SQL
            .split("RETURNING")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(returning, ENTRY_COLS.split_whitespace().collect::<Vec<_>>().join(" "));
    }
}
