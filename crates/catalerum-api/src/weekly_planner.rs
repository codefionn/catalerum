//! Applying weekly plans to the calendar (SOUL §8).
//!
//! A [`WeeklyPlan`] is a template of wall-clock slots; **applying** it to a week
//! materialises one real calendar event per entry and records a link row
//! (`weekly_plan_events`) for each. Applying is a **sync**, not an append, so it
//! is idempotent (SOUL §3.4):
//!
//! - an entry with no linked event in that week → the event is created;
//! - an entry whose linked event still exists → the event is updated in place
//!   (skipped when already identical); if the entry now targets a different
//!   calendar the old event is removed and a new one created there;
//! - a linked event whose entry was deleted from the plan → removed.
//!
//! **Un-applying** a week removes exactly the events linked to the plan there.
//! Events the user created by hand are never touched, and edits made directly
//! on a linked event are overwritten by the next re-apply of its week.
//!
//! Writes go through the same seam as every other event write
//! ([`crate::calendar_writeback`]): local calendars are written in the store,
//! writable provider calendars (CalDAV/Google/Outlook) are written back to the
//! provider first. Shared by the REST routes and the LLM tools so both surfaces
//! behave identically; callers enqueue graph projections for the returned ids.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Serialize;

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Calendar, Event, WeeklyPlan, WeeklyPlanEntry, MINUTES_PER_DAY};
use catalerum_core::{CalendarId, EventId, WorkspaceId};
use catalerum_store::{EventPatch, PlanEntryInput, SecretStore, Store, StoreError, UpsertEvent};

use crate::calendar_writeback::{
    create_on_provider, delete_on_provider, merge_event_update, resolve_event_write_target,
    update_on_provider, EventWriteTarget,
};

/// The most consecutive weeks one apply call may cover.
pub const MAX_APPLY_WEEKS: u32 = 12;

/// `external_id` of the auto-provisioned default local calendar (shared with
/// `create_event`), used when neither the entry nor the plan names a calendar.
const DEFAULT_LOCAL_CALENDAR: &str = crate::tools::DEFAULT_LOCAL_CALENDAR;

/// What applying (or un-applying) one week did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct WeekOutcome {
    /// The Monday the week starts on.
    pub week_start: NaiveDate,
    pub created: Vec<EventId>,
    pub updated: Vec<EventId>,
    /// Linked events already matching the plan (left untouched).
    pub unchanged: Vec<EventId>,
    pub removed: Vec<EventId>,
}

impl WeekOutcome {
    fn new(week_start: NaiveDate) -> Self {
        Self {
            week_start,
            ..Self::default()
        }
    }

    /// Every event id whose graph projection should be reconciled.
    #[must_use]
    pub fn touched(&self) -> impl Iterator<Item = EventId> + '_ {
        self.created
            .iter()
            .chain(&self.updated)
            .chain(&self.removed)
            .copied()
    }
}

/// The Monday of the ISO week containing `date`.
#[must_use]
pub fn monday_of(date: NaiveDate) -> NaiveDate {
    date - Duration::days(i64::from(date.weekday().num_days_from_monday()))
}

/// Parse an IANA timezone name, with an actionable error.
pub fn parse_timezone(name: &str) -> Result<Tz> {
    name.trim().parse::<Tz>().map_err(|_| {
        Error::invalid(format!(
            "unknown timezone `{name}` — use an IANA name like `Europe/Berlin` or `UTC`"
        ))
    })
}

/// Parse `HH:MM` (24h; `24:00` allowed as an end) into minutes after midnight.
pub fn parse_hhmm(value: &str) -> Option<i32> {
    let (h, m) = value.trim().split_once(':')?;
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if !(0..=59).contains(&m) || !(0..=24).contains(&h) || (h == 24 && m != 0) {
        return None;
    }
    Some(h * 60 + m)
}

/// Render minutes after midnight as `HH:MM`.
#[must_use]
pub fn format_hhmm(minutes: i32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Parse a weekday: `0`–`6` (Monday = 0) or an English day name / 3-letter
/// abbreviation (`"wednesday"`, `"Wed"`).
pub fn parse_weekday(value: &str) -> Option<i32> {
    let v = value.trim().to_ascii_lowercase();
    if let Ok(n) = v.parse::<i32>() {
        return (0..=6).contains(&n).then_some(n);
    }
    WEEKDAYS
        .iter()
        .position(|d| v == *d || (v.len() >= 3 && d.starts_with(&v)))
        .map(|i| i as i32)
}

/// English weekday names, Monday first (index = stored `weekday`).
pub const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// A validated, normalized plan entry, ready to persist. Built by both the
/// REST routes and the LLM tools so they accept exactly the same slots.
#[derive(Clone, Debug)]
pub struct EntryDraft {
    pub weekday: i32,
    pub start_minute: i32,
    pub end_minute: i32,
    pub all_day: bool,
    pub summary: String,
    pub location: Option<String>,
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub calendar_id: Option<CalendarId>,
}

impl EntryDraft {
    /// Validate + normalize. `start`/`end` are `HH:MM` and required unless
    /// `all_day` (an all-day entry stores the full day, `00:00`–`24:00`).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        weekday: i32,
        start: Option<&str>,
        end: Option<&str>,
        all_day: bool,
        summary: &str,
        location: Option<&str>,
        body: Option<&str>,
        labels: &[String],
        calendar_id: Option<CalendarId>,
    ) -> Result<Self> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(Error::invalid("entry summary must not be empty"));
        }
        let (start_minute, end_minute) = if all_day {
            (0, MINUTES_PER_DAY)
        } else {
            let parse = |field: &str, v: Option<&str>| {
                let v = v.ok_or_else(|| {
                    Error::invalid(format!("`{field}` (HH:MM) is required unless all_day"))
                })?;
                parse_hhmm(v)
                    .ok_or_else(|| Error::invalid(format!("`{field}` must be HH:MM, got `{v}`")))
            };
            (parse("start", start)?, parse("end", end)?)
        };
        WeeklyPlanEntry::validate_slot(weekday, start_minute, end_minute, all_day)
            .map_err(Error::invalid)?;
        let tidy = |v: Option<&str>| v.map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        Ok(Self {
            weekday,
            start_minute,
            end_minute,
            all_day,
            summary: summary.to_string(),
            location: tidy(location),
            body: tidy(body),
            labels: crate::routes::calendar::clean_labels(labels),
            calendar_id,
        })
    }

    /// Borrow as the store's input shape.
    #[must_use]
    pub fn as_input(&self) -> PlanEntryInput<'_> {
        PlanEntryInput {
            weekday: self.weekday,
            start_minute: self.start_minute,
            end_minute: self.end_minute,
            all_day: self.all_day,
            summary: &self.summary,
            location: self.location.as_deref(),
            body: self.body.as_deref(),
            labels: &self.labels,
            calendar_id: self.calendar_id,
        }
    }
}

/// Check a calendar a plan/entry targets: it must exist in the workspace and be
/// writable (events can't be applied to a read-only subscription).
pub async fn ensure_target_calendar(
    store: &Store,
    ws: WorkspaceId,
    calendar_id: Option<CalendarId>,
) -> Result<()> {
    let Some(id) = calendar_id else {
        return Ok(());
    };
    let cal = store.calendars().get(ws, id).await.map_err(|e| match e {
        StoreError::NotFound => Error::invalid(format!("unknown calendar_id {id}")),
        other => other.into(),
    })?;
    if cal.read_only {
        return Err(Error::invalid(format!(
            "calendar `{}` is read-only; pick a writable calendar for the plan",
            cal.name
        )));
    }
    Ok(())
}

/// The UTC instant of local wall-clock `minute` on `date` in `tz`. An ambiguous
/// time (DST fall-back) takes the earlier instant; a skipped one (spring-forward
/// gap) shifts forward by an hour, like most calendar apps.
fn local_instant(tz: Tz, date: NaiveDate, minute: i32) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).expect("midnight is valid")
        + Duration::minutes(i64::from(minute));
    tz.from_local_datetime(&naive)
        .earliest()
        .or_else(|| tz.from_local_datetime(&(naive + Duration::hours(1))).earliest())
        .map_or_else(|| Utc.from_utc_datetime(&naive), |t| t.with_timezone(&Utc))
}

/// The concrete `[start, end)` an entry occupies in the week starting
/// `week_start`. All-day spans pin to midnight UTC of the date (the store's
/// all-day convention, see `normalize_event_span`).
#[must_use]
pub fn entry_span(
    tz: Tz,
    week_start: NaiveDate,
    entry: &WeeklyPlanEntry,
) -> (DateTime<Utc>, DateTime<Utc>) {
    let date = week_start + Duration::days(i64::from(entry.weekday));
    if entry.all_day {
        let start = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("midnight"));
        (start, start + Duration::days(1))
    } else {
        (
            local_instant(tz, date, entry.start_minute),
            local_instant(tz, date, entry.end_minute),
        )
    }
}

/// Resolves + caches write targets per calendar for one apply call, so a
/// provider (and its OAuth seam) is built once per calendar, not per entry.
struct Targets<'a> {
    store: &'a Store,
    secrets: Option<&'a Arc<SecretStore>>,
    ws: WorkspaceId,
    by_id: HashMap<CalendarId, EventWriteTarget>,
    default_id: Option<CalendarId>,
}

impl<'a> Targets<'a> {
    fn new(store: &'a Store, secrets: Option<&'a Arc<SecretStore>>, ws: WorkspaceId) -> Self {
        Self {
            store,
            secrets,
            ws,
            by_id: HashMap::new(),
            default_id: None,
        }
    }

    /// The calendar an entry writes to: its own, else the plan's, else the
    /// workspace default local calendar (created on first use).
    async fn calendar_for(
        &mut self,
        plan: &WeeklyPlan,
        entry: &WeeklyPlanEntry,
    ) -> Result<CalendarId> {
        if let Some(id) = entry.calendar_id.or(plan.calendar_id) {
            return Ok(id);
        }
        if let Some(id) = self.default_id {
            return Ok(id);
        }
        let cal = self
            .store
            .calendars()
            .upsert_local(self.ws, DEFAULT_LOCAL_CALENDAR, "Calendar")
            .await?;
        self.default_id = Some(cal.id);
        self.by_id.insert(cal.id, EventWriteTarget::Local(cal.clone()));
        Ok(cal.id)
    }

    async fn target(&mut self, calendar_id: CalendarId) -> Result<EventWriteTarget> {
        if let Some(t) = self.by_id.get(&calendar_id) {
            return Ok(t.clone());
        }
        let calendar: Calendar = self
            .store
            .calendars()
            .get(self.ws, calendar_id)
            .await
            .map_err(|e| match e {
                StoreError::NotFound => {
                    Error::invalid(format!("calendar {calendar_id} no longer exists"))
                }
                other => other.into(),
            })?;
        let t = resolve_event_write_target(self.store, self.secrets, self.ws, calendar).await?;
        self.by_id.insert(calendar_id, t.clone());
        Ok(t)
    }

    /// Delete an existing event through its calendar's write path.
    async fn delete_event(&mut self, event: &Event) -> Result<()> {
        match self.target(event.calendar_id).await? {
            EventWriteTarget::Local(_) => match self.store.events().delete(self.ws, event.id).await
            {
                Ok(()) | Err(StoreError::NotFound) => Ok(()),
                Err(e) => Err(e.into()),
            },
            EventWriteTarget::Provider { provider, .. } => {
                delete_on_provider(self.store, self.ws, &provider, event).await
            }
        }
    }
}

/// Whether `event` already matches what `entry` would write.
fn event_matches(
    event: &Event,
    entry: &WeeklyPlanEntry,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> bool {
    event.summary == entry.summary
        && event.start == start
        && event.end == end
        && event.all_day == entry.all_day
        && event.location == entry.location
        && event.body == entry.body
        && event.labels == entry.labels
        && event.rrule.is_none()
}

/// Apply `plan` to `weeks` consecutive weeks starting with the week containing
/// `from`. See the module docs for the sync semantics.
pub async fn apply_plan(
    store: &Store,
    secrets: Option<&Arc<SecretStore>>,
    ws: WorkspaceId,
    plan: &WeeklyPlan,
    from: NaiveDate,
    weeks: u32,
) -> Result<Vec<WeekOutcome>> {
    if weeks == 0 || weeks > MAX_APPLY_WEEKS {
        return Err(Error::invalid(format!(
            "`weeks` must be between 1 and {MAX_APPLY_WEEKS}"
        )));
    }
    let tz = parse_timezone(&plan.timezone)?;
    let mut targets = Targets::new(store, secrets, ws);
    let first = monday_of(from);
    let mut out = Vec::with_capacity(weeks as usize);
    for i in 0..weeks {
        let week = first + Duration::weeks(i64::from(i));
        out.push(apply_week(store, &mut targets, tz, plan, week).await?);
    }
    Ok(out)
}

async fn apply_week(
    store: &Store,
    targets: &mut Targets<'_>,
    tz: Tz,
    plan: &WeeklyPlan,
    week: NaiveDate,
) -> Result<WeekOutcome> {
    let ws = targets.ws;
    let repo = store.weekly_plans();
    let mut outcome = WeekOutcome::new(week);
    let links = repo.links_for_week(ws, plan.id, week).await?;
    let mut linked = HashMap::new();
    for link in links {
        match link.entry_id.filter(|id| plan.entries.iter().any(|e| e.id == *id)) {
            Some(entry_id) => {
                linked.insert(entry_id, link.event_id);
            }
            // The entry was removed from the plan: drop its event.
            None => {
                if let Ok(event) = store.events().get(ws, link.event_id).await {
                    targets.delete_event(&event).await?;
                }
                repo.unlink_event(ws, link.event_id).await?;
                outcome.removed.push(link.event_id);
            }
        }
    }

    for entry in &plan.entries {
        let (start, end) = entry_span(tz, week, entry);
        let calendar_id = targets.calendar_for(plan, entry).await?;
        let existing = match linked.get(&entry.id) {
            Some(event_id) => store.events().get(ws, *event_id).await.ok(),
            None => None,
        };
        let event = match existing {
            Some(event) if event.calendar_id == calendar_id => {
                if event_matches(&event, entry, start, end) {
                    outcome.unchanged.push(event.id);
                    continue;
                }
                let updated = update_event(store, targets, &event, entry, start, end).await?;
                outcome.updated.push(updated.id);
                updated
            }
            other => {
                // Moved to another calendar: remove the old copy first.
                if let Some(old) = other {
                    targets.delete_event(&old).await?;
                    outcome.removed.push(old.id);
                }
                let created =
                    create_event(store, targets, calendar_id, entry, start, end).await?;
                outcome.created.push(created.id);
                created
            }
        };
        repo.link_event(ws, plan.id, entry.id, event.id, week)
            .await?;
    }
    repo.touch_week(ws, plan.id, week).await?;
    Ok(outcome)
}

async fn create_event(
    store: &Store,
    targets: &mut Targets<'_>,
    calendar_id: CalendarId,
    entry: &WeeklyPlanEntry,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Event> {
    match targets.target(calendar_id).await? {
        EventWriteTarget::Local(calendar) => {
            let uid = uuid::Uuid::new_v4().to_string();
            Ok(store
                .events()
                .create(&UpsertEvent {
                    workspace_id: targets.ws,
                    calendar_id: calendar.id,
                    uid: &uid,
                    starts_at: start,
                    ends_at: end,
                    all_day: entry.all_day,
                    rrule: None,
                    summary: &entry.summary,
                    location: entry.location.as_deref(),
                    body: entry.body.as_deref(),
                    attendees: &[],
                    labels: &entry.labels,
                    attachments: &[],
                    etag: None,
                    sequence: 0,
                })
                .await?)
        }
        EventWriteTarget::Provider { calendar, provider } => {
            create_on_provider(
                store,
                &calendar,
                &provider,
                catalerum_core::provider::NewEvent {
                    summary: entry.summary.clone(),
                    start,
                    end,
                    all_day: entry.all_day,
                    location: entry.location.clone(),
                    body: entry.body.clone(),
                    rrule: None,
                    attendees: Vec::new(),
                    labels: entry.labels.clone(),
                    attachments: Vec::new(),
                },
            )
            .await
        }
    }
}

async fn update_event(
    store: &Store,
    targets: &mut Targets<'_>,
    existing: &Event,
    entry: &WeeklyPlanEntry,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
) -> Result<Event> {
    match targets.target(existing.calendar_id).await? {
        EventWriteTarget::Local(_) => Ok(store
            .events()
            .update(
                targets.ws,
                existing.id,
                &EventPatch {
                    starts_at: start,
                    ends_at: end,
                    all_day: entry.all_day,
                    summary: &entry.summary,
                    location: entry.location.as_deref(),
                    body: entry.body.as_deref(),
                    labels: &entry.labels,
                    // Attachments added by hand on the event survive a re-apply.
                    attachments: &existing.attachments,
                    rrule: None,
                },
            )
            .await?),
        EventWriteTarget::Provider { provider, .. } => {
            let merged = merge_event_update(
                existing,
                &entry.summary,
                start,
                end,
                entry.all_day,
                entry.location.as_deref(),
                entry.body.as_deref(),
                None,
                entry.labels.clone(),
                existing.attachments.clone(),
            );
            update_on_provider(store, &provider, &merged).await
        }
    }
}

/// Remove every event `plan` materialised in the week containing `week`
/// (un-apply). Events the user created by hand are untouched.
pub async fn unapply_week(
    store: &Store,
    secrets: Option<&Arc<SecretStore>>,
    ws: WorkspaceId,
    plan: &WeeklyPlan,
    week: NaiveDate,
) -> Result<WeekOutcome> {
    let week = monday_of(week);
    let repo = store.weekly_plans();
    let mut targets = Targets::new(store, secrets, ws);
    let mut outcome = WeekOutcome::new(week);
    for link in repo.links_for_week(ws, plan.id, week).await? {
        if let Ok(event) = store.events().get(ws, link.event_id).await {
            targets.delete_event(&event).await?;
        }
        repo.unlink_event(ws, link.event_id).await?;
        outcome.removed.push(link.event_id);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{WeeklyPlanEntryId, WeeklyPlanId};

    fn entry(weekday: i32, start: i32, end: i32, all_day: bool) -> WeeklyPlanEntry {
        WeeklyPlanEntry {
            id: WeeklyPlanEntryId::new(),
            plan_id: WeeklyPlanId::new(),
            weekday,
            start_minute: start,
            end_minute: end,
            all_day,
            summary: "x".into(),
            location: None,
            body: None,
            labels: Vec::new(),
            calendar_id: None,
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn monday_of_normalizes_any_day() {
        // 2026-10-05 is a Monday.
        assert_eq!(monday_of(d(2026, 10, 5)), d(2026, 10, 5));
        assert_eq!(monday_of(d(2026, 10, 8)), d(2026, 10, 5));
        assert_eq!(monday_of(d(2026, 10, 11)), d(2026, 10, 5)); // Sunday
        assert_eq!(monday_of(d(2026, 10, 12)), d(2026, 10, 12));
    }

    #[test]
    fn hhmm_round_trips_and_rejects_garbage() {
        assert_eq!(parse_hhmm("09:30"), Some(570));
        assert_eq!(parse_hhmm("0:00"), Some(0));
        assert_eq!(parse_hhmm("24:00"), Some(1440));
        assert_eq!(parse_hhmm("24:01"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("noon"), None);
        assert_eq!(format_hhmm(570), "09:30");
        assert_eq!(format_hhmm(1440), "24:00");
    }

    #[test]
    fn timed_entry_lands_on_its_weekday_in_the_plan_timezone() {
        let tz = parse_timezone("Europe/Berlin").unwrap();
        // Wednesday 09:00–10:30 Berlin (CEST, +02:00) in the week of 2026-10-05.
        let (s, e) = entry_span(tz, d(2026, 10, 5), &entry(2, 540, 630, false));
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 10, 7, 7, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 10, 7, 8, 30, 0).unwrap());
    }

    #[test]
    fn wall_clock_is_kept_across_dst() {
        let tz = parse_timezone("Europe/Berlin").unwrap();
        let e = entry(0, 540, 600, false);
        // Monday 09:00 in CEST (+2) vs. after the switch to CET (+1).
        let (summer, _) = entry_span(tz, d(2026, 10, 19), &e);
        let (winter, _) = entry_span(tz, d(2026, 10, 26), &e);
        assert_eq!(summer, Utc.with_ymd_and_hms(2026, 10, 19, 7, 0, 0).unwrap());
        assert_eq!(winter, Utc.with_ymd_and_hms(2026, 10, 26, 8, 0, 0).unwrap());
    }

    #[test]
    fn end_of_day_and_all_day_spans() {
        let tz = parse_timezone("UTC").unwrap();
        let (s, e) = entry_span(tz, d(2026, 10, 5), &entry(6, 1380, 1440, false));
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 10, 11, 23, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 10, 12, 0, 0, 0).unwrap());

        let berlin = parse_timezone("Europe/Berlin").unwrap();
        let (s, e) = entry_span(berlin, d(2026, 10, 5), &entry(4, 0, 0, true));
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 10, 9, 0, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap());
    }

    #[test]
    fn spring_forward_gap_shifts_an_hour() {
        let tz = parse_timezone("Europe/Berlin").unwrap();
        // 2026-03-29 02:30 does not exist in Berlin; it becomes 03:30 CEST.
        let (s, _) = entry_span(tz, d(2026, 3, 23), &entry(6, 150, 200, false));
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 3, 29, 1, 30, 0).unwrap());
    }

    #[test]
    fn weekday_parses_numbers_and_names() {
        assert_eq!(parse_weekday("0"), Some(0));
        assert_eq!(parse_weekday("6"), Some(6));
        assert_eq!(parse_weekday("7"), None);
        assert_eq!(parse_weekday("Wednesday"), Some(2));
        assert_eq!(parse_weekday("sun"), Some(6));
        assert_eq!(parse_weekday("t"), None, "ambiguous / too short");
        assert_eq!(parse_weekday("funday"), None);
    }

    #[test]
    fn entry_draft_validates_and_normalizes() {
        let labels = vec![" Work ".to_string(), "work".to_string()];
        let d = EntryDraft::new(
            1,
            Some("09:00"),
            Some("10:15"),
            false,
            "  Standup ",
            Some("  "),
            None,
            &labels,
            None,
        )
        .unwrap();
        assert_eq!((d.start_minute, d.end_minute), (540, 615));
        assert_eq!(d.summary, "Standup");
        assert_eq!(d.location, None);
        assert_eq!(d.labels, vec!["Work".to_string()]);

        let all_day = EntryDraft::new(4, None, None, true, "Off", None, None, &[], None).unwrap();
        assert_eq!((all_day.start_minute, all_day.end_minute), (0, 1440));

        let bad = |start: Option<&str>, end: Option<&str>, summary: &str| {
            EntryDraft::new(0, start, end, false, summary, None, None, &[], None).is_err()
        };
        assert!(bad(Some("10:00"), Some("09:00"), "x"), "end before start");
        assert!(bad(Some("10:00"), Some("10:00"), "x"), "empty slot");
        assert!(bad(None, Some("10:00"), "x"), "missing start");
        assert!(bad(Some("9am"), Some("10:00"), "x"), "not HH:MM");
        assert!(bad(Some("09:00"), Some("10:00"), "  "), "blank summary");
        assert!(EntryDraft::new(9, Some("09:00"), Some("10:00"), false, "x", None, None, &[], None)
            .is_err());
    }

    #[test]
    fn unknown_timezone_is_invalid() {
        assert!(parse_timezone("Mars/Olympus").is_err());
    }
}
