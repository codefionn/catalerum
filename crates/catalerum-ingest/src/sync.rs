//! Calendar sync orchestration (SOUL §10).
//!
//! [`sync_connection`] is the one entry point: given a connection id, it builds
//! the matching [`CalendarProvider`](catalerum_core::provider::CalendarProvider)
//! from the connection's stored `config`, upserts the provider's calendars,
//! incrementally syncs each calendar's events from its saved cursor, persists
//! everything into Postgres (the source of truth), and saves the returned
//! cursors so the next run resumes where this one stopped.
//!
//! ## Idempotency (SOUL §3.4)
//! Every step is idempotent and keyed on stable provider identity:
//! - calendars upsert by `(connection_id, external_id)`,
//! - events upsert by `(calendar_id, uid)`,
//! - cursors are persisted per calendar and compared on the next run.
//!
//! A second `sync_connection` with no source changes is a **no-op**: the
//! provider returns no upserts (the cursor still matches), nothing is written,
//! and no events are duplicated or lost. When a calendar's cursor *does* change,
//! we reconcile deletions by diffing the freshly-synced UID set against the UIDs
//! already stored for that calendar and removing the leftovers (covering
//! providers — like local `.ics` — that cannot observe deletions across calls
//! and so emit none in [`SyncBatch::deletions`]).

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use catalerum_core::model::{Calendar, Connection, Cursor};
use catalerum_core::{EventId, WorkspaceId};
use catalerum_store::{SecretStore, Store, UpsertEvent};

use crate::error::{IngestError, Result};

/// Best-effort enqueue of an event's §6.3 graph projection (`project_event`). A
/// failed enqueue is logged, never failing the sync: the graph is **derived** +
/// rebuildable from Postgres truth (§3), and the event is already persisted.
async fn enqueue_event_projection(store: &Store, workspace_id: WorkspaceId, event_id: EventId) {
    if let Err(e) = crate::enqueue_project_event(store, workspace_id, event_id).await {
        tracing::warn!(error = %e, %event_id, "failed to enqueue event graph projection");
    }
}

/// What a single [`sync_connection`] run did. Returned for logging / metrics and
/// asserted on in tests (a no-op second run reports zero writes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of calendars upserted (listed) on the connection.
    pub calendars: usize,
    /// Number of events inserted or updated across all calendars.
    pub events_upserted: usize,
    /// Number of events deleted (reconciled removals + provider deletions).
    pub events_deleted: usize,
    /// True if at least one calendar's cursor advanced (the source changed).
    pub changed: bool,
}

impl SyncReport {
    /// True if this run wrote nothing (a no-op): no upserts, no deletions.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.events_upserted == 0 && self.events_deleted == 0
    }
}

/// Sync one connection end-to-end (SOUL §10): build its provider, upsert its
/// calendars, incrementally sync events into Postgres, and persist cursors.
///
/// `connection_id` and `workspace_id` together scope every read and write to the
/// owning workspace (SOUL §14). The connection must be of kind
/// [`ConnectionKind::Calendar`](catalerum_core::model::ConnectionKind::Calendar);
/// other kinds (storage, channel) land in later milestones and return
/// [`IngestError::Provider`] with an `Unsupported`/`Invalid` cause.
///
/// Idempotent: re-running with no source changes is a no-op (see module docs).
///
/// A Google connection needs the OAuth token seam; use
/// [`sync_connection_with`] and pass the secret store. This entry (no secrets)
/// works for local/CalDAV/webcal and errors clearly on a Google connection.
pub async fn sync_connection(
    store: &Store,
    workspace_id: catalerum_core::id::WorkspaceId,
    connection_id: catalerum_core::id::ConnectionId,
) -> Result<SyncReport> {
    sync_connection_with(store, workspace_id, connection_id, None).await
}

/// Like [`sync_connection`], but threads the encrypted secret store so a Google
/// calendar connection can build its OAuth-backed provider (SOUL §13/§16 M7).
pub async fn sync_connection_with(
    store: &Store,
    workspace_id: catalerum_core::id::WorkspaceId,
    connection_id: catalerum_core::id::ConnectionId,
    secrets: Option<&Arc<SecretStore>>,
) -> Result<SyncReport> {
    // 1. Load the connection (with its config blob) and build the provider.
    let row = store
        .connections()
        .get_row(workspace_id, connection_id)
        .await?;
    let connection: Connection = row.clone().try_into().map_err(IngestError::Store)?;
    let google_tokens =
        crate::google_tokens::google_token_store_for(secrets, &connection, row.config());
    let outlook_tokens =
        crate::outlook_tokens::outlook_token_store_for(secrets, &connection, row.config());
    let provider = catalerum_calendar::provider_from_connection_with(
        &connection,
        row.config(),
        google_tokens,
        outlook_tokens,
    )?;

    // The connection's single `sync_token` column holds a per-calendar cursor
    // map (keyed by external_id) encoded as JSON, so one connection can carry N
    // calendars each with its own incremental position.
    let mut cursors = CursorMap::decode(connection.cursor.as_ref());

    // 2. List + upsert calendars. The store's returned CalendarId is the source
    //    of truth; we sync against it (the provider's id is stable but we never
    //    assume it equals the stored one — SOUL §6.1).
    let provider_calendars = provider.list_calendars().await?;
    // Provider calendars the user has deleted locally (`DELETE /calendars/{id}`):
    // skip re-`upsert`ing them so the deletion sticks across syncs (SOUL §8/§11).
    let excluded: std::collections::HashSet<String> = store
        .calendars()
        .excluded_external_ids(workspace_id, connection_id)
        .await?
        .into_iter()
        .collect();
    let mut report = SyncReport {
        calendars: provider_calendars
            .iter()
            .filter(|pc| !excluded.contains(&pc.external_id))
            .count(),
        ..SyncReport::default()
    };

    for pc in &provider_calendars {
        if excluded.contains(&pc.external_id) {
            continue;
        }
        let stored = store
            .calendars()
            .upsert(
                workspace_id,
                connection_id,
                &pc.external_id,
                &pc.name,
                pc.read_only,
            )
            .await?;

        let prior = cursors.get(&pc.external_id);
        let outcome = sync_calendar(store, &provider, &stored, prior.clone()).await?;

        report.events_upserted += outcome.upserted;
        report.events_deleted += outcome.deleted;

        if outcome.next_cursor.as_ref() != prior.as_ref() {
            report.changed = true;
        }
        cursors.set(&pc.external_id, outcome.next_cursor);
    }

    // 3. Persist the (possibly advanced) per-calendar cursor map back onto the
    //    connection so the next run resumes incrementally.
    let encoded = cursors.encode();
    store
        .connections()
        .update_cursor(workspace_id, connection_id, encoded.as_ref())
        .await?;

    Ok(report)
}

/// The result of syncing a single calendar.
struct CalendarSync {
    upserted: usize,
    deleted: usize,
    next_cursor: Option<Cursor>,
}

/// Incrementally sync one calendar's events from `cursor` into Postgres.
///
/// Upserts every returned event by `(calendar_id, uid)`, applies provider-
/// reported deletions, and — when the cursor advanced — reconciles removals by
/// diffing the freshly-synced UID set against the calendar's stored UIDs.
async fn sync_calendar(
    store: &Store,
    provider: &std::sync::Arc<dyn catalerum_core::provider::CalendarProvider>,
    calendar: &Calendar,
    cursor: Option<Cursor>,
) -> Result<CalendarSync> {
    let mut upserted = 0usize;
    let mut deleted = 0usize;
    let mut any_deletions = false;
    let mut synced_uids: HashSet<String> = HashSet::new();

    // Drain every page: a paged provider (`SyncBatch::has_more`) must have all
    // pages fetched this run, not just the first — otherwise its backlog is
    // silently dropped until the next scheduled sync. Bounded by `next_sync_page`
    // (page cap + a cursor-advance guard) so a misbehaving provider can't loop.
    // `synced_uids` accumulates across pages so the reconcile below sees the full
    // snapshot, not just the last page.
    let mut page_cursor = cursor.clone();
    let mut pages = 0usize;
    let (final_cursor, drained_fully) = loop {
        let batch = provider.sync(calendar, page_cursor.clone()).await?;
        pages += 1;

        for event in &batch.upserts {
            let upsert = event_to_upsert(event);
            let stored = store.events().upsert_by_uid(&upsert).await?;
            synced_uids.insert(event.uid.clone());
            upserted += 1;
            // §6.3: project the new/changed event into the derived graph (an `:Event`
            // node + `SCHEDULED_IN` edge to its `:Calendar`). For an incremental
            // provider `batch.upserts` is the delta (only changed events project);
            // for a full-snapshot provider it re-projects the window (idempotent).
            // Deleted events' nodes are reconciled on the next full graph rebuild —
            // a documented follow-up (the survey scoped this slice to upserts).
            enqueue_event_projection(store, calendar.workspace_id, stored.id).await;
        }

        // Provider-reported deletions (CalDAV 404s, etc.) apply directly by UID.
        for uid in &batch.deletions {
            if store
                .events()
                .delete_by_uid(calendar.workspace_id, calendar.id, uid)
                .await?
            {
                deleted += 1;
            }
            any_deletions = true;
        }

        let has_more = batch.has_more;
        let next = batch.next_cursor;
        match crate::next_sync_page(
            has_more,
            page_cursor.as_ref(),
            &next,
            pages,
            crate::MAX_SYNC_PAGES,
        ) {
            Some(c) => page_cursor = Some(c),
            // `drained_fully` is true ONLY when the provider reported no more data —
            // not on an early stop (page cap / non-advancing cursor). A partial drain
            // must NOT trigger the full-snapshot reconcile below (it would prune
            // events that live on the un-fetched pages).
            None => break (next, !has_more),
        }
    };

    // Reconcile removals for providers that return the full set without
    // emitting deletions (e.g. local `.ics`). Only do this when the cursor
    // advanced, the provider returned a full snapshot this run (non-empty upserts,
    // across all pages), AND the drain completed (`drained_fully`) — a cap-/stall-
    // truncated drain is a partial snapshot, so skip it rather than prune events on
    // the un-fetched pages. Thus an incremental delta batch is never mistaken for
    // "the calendar is now empty".
    let advanced = Some(&final_cursor) != cursor.as_ref();
    if drained_fully && advanced && upserted > 0 && !any_deletions {
        // Reconciliation must see *every* stored event for this calendar to find
        // the orphans the snapshot dropped — an unbounded list (i64::MAX), not
        // the agenda's recency cap, or events past the cap would never be pruned.
        let stored = store
            .events()
            .list_by_workspace(
                calendar.workspace_id,
                Some(calendar.id),
                catalerum_store::DateRange::default(),
                i64::MAX,
            )
            .await?;
        for ev in stored {
            if !synced_uids.contains(&ev.uid)
                && store
                    .events()
                    .delete_by_uid(calendar.workspace_id, calendar.id, &ev.uid)
                    .await?
            {
                deleted += 1;
            }
        }
    }

    Ok(CalendarSync {
        upserted,
        deleted,
        next_cursor: Some(final_cursor),
    })
}

/// Map a synced core [`Event`](catalerum_core::model::Event) to a store
/// [`UpsertEvent`]. Every field — including `all_day` — maps straight through.
/// Shared with the `WriteEvent` automation action (SOUL §8/§10), which persists
/// a *collected* event the same way the calendar sync path does.
pub fn event_to_upsert(event: &catalerum_core::model::Event) -> UpsertEvent<'_> {
    UpsertEvent {
        workspace_id: event.workspace_id,
        calendar_id: event.calendar_id,
        uid: &event.uid,
        starts_at: event.start,
        ends_at: event.end,
        all_day: event.all_day,
        rrule: event.rrule.as_deref(),
        summary: &event.summary,
        location: event.location.as_deref(),
        body: event.body.as_deref(),
        attendees: &event.attendees,
        labels: &event.labels,
        attachments: &event.attachments,
        etag: event.etag.as_deref(),
        sequence: i32::try_from(event.sequence).unwrap_or(i32::MAX),
    }
}

/// A per-calendar cursor map persisted into the connection's single
/// `sync_token` column as a JSON object `{ external_id: cursor_string }`.
///
/// Encoding many calendars' cursors in one column keeps the store API unchanged
/// (one `update_cursor` per connection) while letting a multi-calendar
/// connection (a local `.ics` directory of files) resume each calendar
/// incrementally.
pub(crate) struct CursorMap(BTreeMap<String, String>);

impl CursorMap {
    /// Decode the map from a connection cursor. A legacy/opaque non-JSON cursor
    /// (or `None`) decodes to an empty map, so the first run is treated as a
    /// full sync — safe, because upserts are idempotent.
    pub(crate) fn decode(cursor: Option<&Cursor>) -> Self {
        let map = cursor
            .and_then(|c| serde_json::from_str::<BTreeMap<String, String>>(&c.0).ok())
            .unwrap_or_default();
        Self(map)
    }

    pub(crate) fn get(&self, external_id: &str) -> Option<Cursor> {
        self.0.get(external_id).map(|s| Cursor::new(s.clone()))
    }

    pub(crate) fn set(&mut self, external_id: &str, cursor: Option<Cursor>) {
        match cursor {
            Some(c) => {
                self.0.insert(external_id.to_string(), c.0);
            }
            None => {
                self.0.remove(external_id);
            }
        }
    }

    /// Encode the map back into a single connection [`Cursor`], or `None` when
    /// empty (so an all-unsynced connection clears its `sync_token`).
    pub(crate) fn encode(&self) -> Option<Cursor> {
        if self.0.is_empty() {
            return None;
        }
        serde_json::to_string(&self.0).ok().map(Cursor::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_map_round_trips() {
        let mut m = CursorMap(BTreeMap::new());
        m.set("work.ics", Some(Cursor::new("sha256:abc")));
        m.set("home.ics", Some(Cursor::new("sha256:def")));
        let encoded = m.encode().unwrap();

        let back = CursorMap::decode(Some(&encoded));
        assert_eq!(back.get("work.ics"), Some(Cursor::new("sha256:abc")));
        assert_eq!(back.get("home.ics"), Some(Cursor::new("sha256:def")));
        assert_eq!(back.get("missing.ics"), None);
    }

    #[test]
    fn cursor_map_empty_encodes_to_none() {
        let m = CursorMap(BTreeMap::new());
        assert!(m.encode().is_none());
    }

    #[test]
    fn cursor_map_decodes_legacy_opaque_cursor_as_empty() {
        // A non-JSON token (e.g. an old single-calendar sync-token) must not
        // panic; it decodes to empty so the next run re-syncs idempotently.
        let m = CursorMap::decode(Some(&Cursor::new("sync:opaque-server-token")));
        assert!(m.0.is_empty());
    }

    #[test]
    fn cursor_map_clears_on_none() {
        let mut m = CursorMap(BTreeMap::new());
        m.set("a.ics", Some(Cursor::new("x")));
        m.set("a.ics", None);
        assert!(m.0.is_empty());
    }

    #[test]
    fn report_noop_detection() {
        let noop = SyncReport {
            calendars: 1,
            events_upserted: 0,
            events_deleted: 0,
            changed: false,
        };
        assert!(noop.is_noop());

        let wrote = SyncReport {
            events_upserted: 3,
            ..noop.clone()
        };
        assert!(!wrote.is_noop());
    }
}
