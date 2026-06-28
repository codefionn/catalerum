//! Integration test: local (database-native) calendars + the event write path
//! (`CalendarRepo::upsert_local` / `EventRepo::create`/`update`/`delete`, SOUL
//! §8/§11). A local calendar has no provider connection (`connection_id IS
//! NULL`), is read-write, and is the substrate automations target. Covers
//! get-or-create idempotency by `external_id`, the `is_local` flag, direct event
//! create/update (sequence bump)/delete by id, listing, and cross-workspace
//! isolation (§18).
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{Attachment, ConnectionKind};
use catalerum_store::{EventPatch, Store, StoreError, UpsertEvent};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn local_calendar_and_event_write_path() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping local_calendar_and_event_write_path: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cal", &format!("cal-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("cal-b", &format!("cal-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // upsert_local is get-or-create by (workspace, external_id): two calls for
    // the same external_id return the SAME id with the name refreshed — never a
    // duplicate (§3.4). And it is local + read-write.
    let c1 = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .expect("upsert_local");
    let c2 = store
        .calendars()
        .upsert_local(ws.id, "default", "Personal")
        .await
        .expect("upsert_local again");
    assert_eq!(c1.id, c2.id, "idempotent on (workspace, external_id)");
    assert_eq!(c2.name, "Personal", "name refreshed");
    assert!(c2.connection_id.is_none() && c2.is_local(), "no connection");
    assert!(!c2.read_only, "local calendars are writable");
    assert_eq!(
        store
            .calendars()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1,
        "no duplicate calendar row"
    );

    // A distinct external_id is a distinct local calendar.
    let work = store
        .calendars()
        .upsert_local(ws.id, "work", "Work")
        .await
        .expect("second local calendar");
    assert_ne!(work.id, c1.id);

    // Create an event directly (a fresh uid → a pure insert).
    let start = chrono::DateTime::parse_from_rfc3339("2026-06-18T09:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let end = chrono::DateTime::parse_from_rfc3339("2026-06-18T09:30:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let uid = uuid::Uuid::new_v4().to_string();
    let labels = vec!["Work".to_string(), "Standups".to_string()];
    let attachments = vec![Attachment {
        url: "https://example.com/agenda.pdf".to_string(),
        filename: Some("agenda.pdf".to_string()),
        content_type: Some("application/pdf".to_string()),
        size: Some(2048),
    }];
    let ev = store
        .events()
        .create(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: c1.id,
            uid: &uid,
            starts_at: start,
            ends_at: end,
            all_day: false,
            rrule: None,
            summary: "Standup",
            location: Some("Room 2"),
            body: None,
            attendees: &[],
            labels: &labels,
            attachments: &attachments,
            etag: None,
            sequence: 0,
        })
        .await
        .expect("create event");
    assert_eq!(ev.summary, "Standup");
    assert_eq!(ev.sequence, 0);
    // Labels + attachments round-trip through the JSONB columns.
    assert_eq!(ev.labels, labels);
    assert_eq!(ev.attachments, attachments);

    // Update by id: fields land, SEQUENCE bumps, uid is preserved.
    let new_end = chrono::DateTime::parse_from_rfc3339("2026-06-18T10:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let updated = store
        .events()
        .update(
            ws.id,
            ev.id,
            &EventPatch {
                starts_at: start,
                ends_at: new_end,
                all_day: false,
                summary: "Standup (extended)",
                location: None,
                body: Some("agenda"),
                labels: &["Planning".to_string()],
                attachments: &[],
                rrule: None,
            },
        )
        .await
        .expect("update event");
    assert_eq!(updated.id, ev.id);
    assert_eq!(updated.uid, uid, "uid is immutable");
    assert_eq!(updated.summary, "Standup (extended)");
    assert_eq!(updated.end, new_end);
    assert_eq!(updated.location, None, "location cleared");
    assert_eq!(updated.sequence, 1, "SEQUENCE bumped on edit");
    // The patch replaces labels and clears attachments.
    assert_eq!(updated.labels, vec!["Planning".to_string()]);
    assert!(
        updated.attachments.is_empty(),
        "attachments cleared by patch"
    );

    // The event shows up in the workspace listing (the path the CalendarEvent
    // automation trigger polls).
    let listed = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            Default::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, ev.id);

    // §18: another workspace can neither see nor touch the event/calendar.
    assert!(store
        .calendars()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        store.events().get(other.id, ev.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.events().delete(other.id, ev.id).await,
        Err(StoreError::NotFound)
    ));

    // Delete by id removes it; a second delete is NotFound.
    store.events().delete(ws.id, ev.id).await.expect("delete");
    assert!(matches!(
        store.events().delete(ws.id, ev.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            Default::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT
        )
        .await
        .unwrap()
        .is_empty());
}

/// `list_by_workspace`'s `limit` bounds the result to the first `limit` events
/// by start (ascending), floored at 1. A limit at/above the row count returns
/// everything — the bound is a no-op.
#[tokio::test]
async fn event_list_limit_bounds_first_ascending() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping event_list_limit_bounds_first_ascending: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cap", &format!("cap-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .expect("upsert_local");

    // Five events on consecutive days, inserted out of order to prove ordering
    // comes from the query, not insertion.
    let days = [16u32, 14, 18, 15, 17];
    for d in days {
        let start = chrono::DateTime::parse_from_rfc3339(&format!("2026-06-{d:02}T09:00:00Z"))
            .unwrap()
            .with_timezone(&chrono::Utc);
        let end = start + chrono::Duration::minutes(30);
        store
            .events()
            .create(&UpsertEvent {
                workspace_id: ws.id,
                calendar_id: cal.id,
                uid: &uuid::Uuid::new_v4().to_string(),
                starts_at: start,
                ends_at: end,
                all_day: false,
                rrule: None,
                summary: &format!("Day {d}"),
                location: None,
                body: None,
                attendees: &[],
                labels: &[],
                attachments: &[],
                etag: None,
                sequence: 0,
            })
            .await
            .expect("create event");
    }

    // limit = 2 → the first two starts ascending (06-14, 06-15).
    let capped = store
        .events()
        .list_by_workspace(ws.id, None, Default::default(), 2)
        .await
        .expect("list capped");
    let starts: Vec<String> = capped
        .iter()
        .map(|e| e.start.format("%Y-%m-%d").to_string())
        .collect();
    assert_eq!(
        starts,
        vec!["2026-06-14", "2026-06-15"],
        "first two, ascending"
    );

    // A floor of 1 protects against a zero/negative limit.
    let one = store
        .events()
        .list_by_workspace(ws.id, None, Default::default(), 0)
        .await
        .expect("list floored");
    assert_eq!(one.len(), 1);
    assert_eq!(
        one[0].start.format("%Y-%m-%d").to_string(),
        "2026-06-14",
        "the single earliest event"
    );

    // limit ≥ count → every event, ascending (the window is a no-op).
    let all = store
        .events()
        .list_by_workspace(ws.id, None, Default::default(), 50)
        .await
        .expect("list all");
    let all_starts: Vec<String> = all
        .iter()
        .map(|e| e.start.format("%Y-%m-%d").to_string())
        .collect();
    assert_eq!(
        all_starts,
        vec![
            "2026-06-14",
            "2026-06-15",
            "2026-06-16",
            "2026-06-17",
            "2026-06-18",
        ],
        "all five, ascending"
    );
}

/// Deleting a *synced* calendar records a persistent exclusion so a re-sync
/// won't resurrect it (migration `0057`); the exclusion is keyed on
/// `(connection_id, external_id)`, idempotent, and cleared when its connection
/// is dropped (FK cascade) — mirroring "remove + re-add the source re-syncs it".
#[tokio::test]
async fn provider_calendar_exclusion_survives_resync_and_cascades() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping provider_calendar_exclusion_survives_resync_and_cascades: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("excl", &format!("excl-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Calendar, "dav", None, None)
        .await
        .expect("conn");

    // A provider calendar (as a sync would upsert it).
    let cal = store
        .calendars()
        .upsert(ws.id, conn.id, "ext-work", "Work", true)
        .await
        .expect("upsert provider calendar");
    assert!(!cal.is_local(), "provider-backed");
    assert!(
        store
            .calendars()
            .excluded_external_ids(ws.id, conn.id)
            .await
            .unwrap()
            .is_empty(),
        "no exclusions yet"
    );

    // Delete it the way the API does: record the exclusion, then drop the row.
    store
        .calendars()
        .exclude(ws.id, conn.id, "ext-work")
        .await
        .expect("exclude");
    store
        .calendars()
        .delete(ws.id, cal.id)
        .await
        .expect("delete");
    // Idempotent: a repeated exclude is a no-op.
    store
        .calendars()
        .exclude(ws.id, conn.id, "ext-work")
        .await
        .expect("exclude again");
    assert_eq!(
        store
            .calendars()
            .excluded_external_ids(ws.id, conn.id)
            .await
            .unwrap(),
        vec!["ext-work".to_string()],
        "exclusion recorded once"
    );

    // The exclusion is per (connection, external_id): a *different* external_id
    // on the same connection is unaffected, so a sync still upserts it.
    store
        .calendars()
        .upsert(ws.id, conn.id, "ext-home", "Home", true)
        .await
        .expect("other calendar upserts");
    assert_eq!(
        store
            .calendars()
            .excluded_external_ids(ws.id, conn.id)
            .await
            .unwrap(),
        vec!["ext-work".to_string()],
        "only the deleted calendar is excluded"
    );

    // Removing the connection cascades the exclusion away, so re-adding the
    // source (a fresh connection) would re-sync every calendar again.
    store
        .connections()
        .delete(ws.id, conn.id)
        .await
        .expect("delete conn");
    assert!(
        store
            .calendars()
            .excluded_external_ids(ws.id, conn.id)
            .await
            .unwrap()
            .is_empty(),
        "exclusion cascaded on connection delete"
    );
}
