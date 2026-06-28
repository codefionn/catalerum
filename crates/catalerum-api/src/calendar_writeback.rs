//! Provider **write-back** seam for calendar events (SOUL §8).
//!
//! Historically every event write surface (REST routes, LLM tools) refused
//! provider-backed calendars — they were sync-managed, read-only substrate.
//! With the providers' write halves implemented (CalDAV `PUT`/`DELETE`, Google
//! `events.insert`/`patch`/`delete`, Outlook Graph `POST`/`PATCH`/`DELETE`),
//! those surfaces now route an edit on a **writable provider calendar through
//! the provider first**, then mirror the provider's canonical result into the
//! local store (`upsert_by_uid` under the provider-issued uid/ETag), so the
//! next sync converges instead of fighting the edit.
//!
//! **Ordering & failure posture.** The provider write happens first: if it
//! fails, the local store is untouched (no divergence, the error surfaces to
//! the caller). If the local mirror fails *after* a successful provider write,
//! the divergence is transient — the next sync/collect pulls the provider's
//! state back in.
//!
//! **What stays out.** The `WriteEvent` automation action (persisting a
//! *collected* provider event) is intentionally not routed here — it mirrors
//! provider → local and writing back would loop. Local (database-native)
//! calendars keep their existing store-only path. Read-only provider calendars
//! (webcal/ics subscriptions, local `.ics` directories) are refused exactly as
//! before.

use std::sync::Arc;

use catalerum_core::error::{Error, Result};
use catalerum_core::model::{Calendar, Connection, Event};
use catalerum_core::provider::{CalendarProvider, NewEvent};
use catalerum_core::WorkspaceId;
use catalerum_store::{SecretStore, Store};

/// Where an event write lands, resolved from the target calendar.
pub enum EventWriteTarget {
    /// A local (database-native) calendar: the store is the only truth; the
    /// caller keeps its existing store-only write path.
    Local(Calendar),
    /// A writable provider-backed calendar: write the provider first, then
    /// mirror its canonical result locally.
    Provider {
        calendar: Calendar,
        provider: Arc<dyn CalendarProvider>,
    },
}

/// Resolve the write target for `calendar`, building the live provider (with
/// its OAuth token seams) for a writable provider-backed one. Read-only
/// calendars — local or provider — are refused with an actionable error.
pub async fn resolve_event_write_target(
    store: &Store,
    secrets: Option<&Arc<SecretStore>>,
    ws: WorkspaceId,
    calendar: Calendar,
) -> Result<EventWriteTarget> {
    if calendar.read_only {
        return Err(Error::invalid(
            "this calendar is read-only (an ics/webcal subscription or a local .ics \
             directory); events cannot be written to it",
        ));
    }
    let Some(connection_id) = calendar.connection_id else {
        return Ok(EventWriteTarget::Local(calendar));
    };
    let row = store
        .connections()
        .get_row(ws, connection_id)
        .await
        .map_err(|e| Error::provider(format!("load the calendar's connection: {e}")))?;
    let connection: Connection = row
        .clone()
        .try_into()
        .map_err(|e| Error::provider(format!("decode the calendar's connection: {e}")))?;
    let google = catalerum_ingest::google_token_store_for(secrets, &connection, row.config());
    let outlook = catalerum_ingest::outlook_token_store_for(secrets, &connection, row.config());
    let provider = catalerum_ingest::provider_from_connection_with(
        &connection,
        row.config(),
        google,
        outlook,
    )?;
    Ok(EventWriteTarget::Provider { calendar, provider })
}

/// Create `event` on the provider, then mirror the provider's canonical event
/// (its uid/ETag/sequence) into the store by `(calendar_id, uid)`.
pub async fn create_on_provider(
    store: &Store,
    calendar: &Calendar,
    provider: &Arc<dyn CalendarProvider>,
    event: NewEvent,
) -> Result<Event> {
    let created = provider.create_event(calendar, event).await?;
    store
        .events()
        .upsert_by_uid(&catalerum_ingest::event_to_upsert(&created))
        .await
        .map_err(|e| {
            Error::provider(format!(
                "the event was created on the provider but the local mirror failed \
                 (the next sync heals it): {e}"
            ))
        })
}

/// Push `updated` (the caller-merged full event state, same uid/ids) to the
/// provider, then mirror the provider's canonical result into the store.
pub async fn update_on_provider(
    store: &Store,
    provider: &Arc<dyn CalendarProvider>,
    updated: &Event,
) -> Result<Event> {
    let result = provider.update_event(updated).await?;
    store
        .events()
        .upsert_by_uid(&catalerum_ingest::event_to_upsert(&result))
        .await
        .map_err(|e| {
            Error::provider(format!(
                "the event was updated on the provider but the local mirror failed \
                 (the next sync heals it): {e}"
            ))
        })
}

/// Delete `event` on the provider (idempotent there on already-gone), then
/// drop the local row.
pub async fn delete_on_provider(
    store: &Store,
    ws: WorkspaceId,
    provider: &Arc<dyn CalendarProvider>,
    event: &Event,
) -> Result<()> {
    provider.delete_event(event).await?;
    store.events().delete(ws, event.id).await.map_err(|e| {
        Error::provider(format!(
            "the event was deleted on the provider but the local mirror failed \
             (the next sync heals it): {e}"
        ))
    })?;
    Ok(())
}

/// Normalize a client-supplied event `(start, end)` to the instants the store
/// persists.
///
/// An all-day event marks calendar *dates*, not instants: the calendar renderer
/// (which buckets an all-day block by the date substring of its stored stamp and
/// never timezone-shifts it) and every provider's date-valued write-back
/// (`VALUE=DATE` / `{date}` / `isAllDay`) both assume an all-day stamp is
/// **midnight UTC** of its date. So pin each all-day endpoint to midnight UTC of
/// the wall-clock date the caller wrote — read in the input's *own* offset, so an
/// all-day event sent as `2026-07-07T00:00:00+02:00` stays on the 7th instead of
/// collapsing to `2026-07-06T22:00:00Z` and rendering a day early (the off-by-one
/// that dropped all-day events onto the previous day in the week/month grids). A
/// timed event keeps its exact instant, offset collapsed to UTC.
#[must_use]
pub fn normalize_event_span(
    start: chrono::DateTime<chrono::FixedOffset>,
    end: chrono::DateTime<chrono::FixedOffset>,
    all_day: bool,
) -> (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) {
    if all_day {
        (all_day_midnight_utc(start), all_day_midnight_utc(end))
    } else {
        (
            start.with_timezone(&chrono::Utc),
            end.with_timezone(&chrono::Utc),
        )
    }
}

/// Midnight UTC of a timestamp's wall-clock date in its own offset — the
/// canonical stored instant for an all-day endpoint.
fn all_day_midnight_utc(
    dt: chrono::DateTime<chrono::FixedOffset>,
) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    chrono::Utc.from_utc_datetime(&dt.date_naive().and_time(chrono::NaiveTime::MIN))
}

/// Merge an edit surface's full replacement fields onto an existing event —
/// the value [`update_on_provider`] pushes. Identity (`id`, `uid`,
/// `workspace_id`, `calendar_id`, `etag`) rides from `existing`; `SEQUENCE` is
/// bumped (RFC 5545: a change increments it), matching the store's local
/// `update`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn merge_event_update(
    existing: &Event,
    summary: &str,
    start: chrono::DateTime<chrono::Utc>,
    end: chrono::DateTime<chrono::Utc>,
    all_day: bool,
    location: Option<&str>,
    body: Option<&str>,
    rrule: Option<&str>,
    labels: Vec<String>,
    attachments: Vec<catalerum_core::model::Attachment>,
) -> Event {
    Event {
        summary: summary.to_string(),
        start,
        end,
        all_day,
        location: location.map(str::to_string),
        body: body.map(str::to_string),
        rrule: rrule.map(str::to_string),
        labels,
        attachments,
        sequence: existing.sequence + 1,
        ..existing.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::id::{CalendarId, EventId};
    use chrono::{TimeZone, Utc};

    #[test]
    fn merge_keeps_identity_and_bumps_sequence() {
        let existing = Event {
            id: EventId::new(),
            workspace_id: WorkspaceId::new(),
            calendar_id: CalendarId::new(),
            uid: "prov-uid".into(),
            start: Utc.with_ymd_and_hms(2026, 7, 1, 9, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 7, 1, 10, 0, 0).unwrap(),
            all_day: false,
            rrule: Some("FREQ=DAILY".into()),
            summary: "old".into(),
            location: Some("old room".into()),
            attendees: Vec::new(),
            body: None,
            labels: vec!["a".into()],
            attachments: Vec::new(),
            etag: Some("\"e1\"".into()),
            sequence: 3,
        };
        let merged = merge_event_update(
            &existing,
            "new",
            Utc.with_ymd_and_hms(2026, 7, 2, 9, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 7, 2, 10, 0, 0).unwrap(),
            true,
            None,
            Some("notes"),
            None,
            vec!["b".into()],
            Vec::new(),
        );
        // Identity + concurrency token ride along untouched…
        assert_eq!(merged.id, existing.id);
        assert_eq!(merged.uid, "prov-uid");
        assert_eq!(merged.calendar_id, existing.calendar_id);
        assert_eq!(merged.etag.as_deref(), Some("\"e1\""));
        // …the replacement fields land (omitted optionals clear)…
        assert_eq!(merged.summary, "new");
        assert!(merged.all_day);
        assert!(merged.location.is_none());
        assert_eq!(merged.body.as_deref(), Some("notes"));
        assert!(merged.rrule.is_none());
        assert_eq!(merged.labels, vec!["b".to_string()]);
        // …and the edit bumps SEQUENCE like the store's local update.
        assert_eq!(merged.sequence, 4);
    }

    fn dt(s: &str) -> chrono::DateTime<chrono::FixedOffset> {
        chrono::DateTime::parse_from_rfc3339(s).unwrap()
    }

    #[test]
    fn all_day_span_pins_written_date_to_midnight_utc() {
        // A positive-offset all-day start must stay on the date the caller wrote
        // (the 7th), not slip to the previous UTC day — the week/month off-by-one.
        let (s, e) = normalize_event_span(
            dt("2026-07-07T00:00:00+02:00"),
            dt("2026-07-08T00:00:00+02:00"),
            true,
        );
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 7, 8, 0, 0, 0).unwrap());

        // A non-midnight UTC all-day stamp is floored to that date's midnight.
        let (s, e) =
            normalize_event_span(dt("2026-07-07T14:30:00Z"), dt("2026-07-07T15:00:00Z"), true);
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0).unwrap());
    }

    #[test]
    fn timed_span_keeps_exact_instant_as_utc() {
        let (s, e) = normalize_event_span(
            dt("2026-07-07T09:00:00+02:00"),
            dt("2026-07-07T10:00:00+02:00"),
            false,
        );
        assert_eq!(s, Utc.with_ymd_and_hms(2026, 7, 7, 7, 0, 0).unwrap());
        assert_eq!(e, Utc.with_ymd_and_hms(2026, 7, 7, 8, 0, 0).unwrap());
    }
}
