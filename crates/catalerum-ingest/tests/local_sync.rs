//! Integration test: end-to-end local `.ics` sync against a fixture directory.
//!
//! Proves the M2 contract (SOUL §10): `sync_connection` builds the local
//! provider from the connection's `config`, upserts calendars, upserts events
//! into Postgres, persists cursors, and — critically — a **second run with no
//! source changes is a no-op** (SOUL §3.4 idempotency).
//!
//! Requires a Postgres (the source of truth). Set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to a database the test may migrate + write to; when
//! neither is set the test prints a skip note and passes, so
//! `cargo test -p catalerum-ingest` stays green offline. `just test` provides an
//! ephemeral Postgres.

use catalerum_core::model::ConnectionKind;
use catalerum_store::{DateRange, Store};

/// A minimal two-event VCALENDAR fixture.
const ICS: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//catalerum//test//EN\r
BEGIN:VEVENT\r
UID:standup@catalerum\r
DTSTART:20260613T090000Z\r
DTEND:20260613T093000Z\r
SUMMARY:Daily standup\r
LOCATION:Room 1\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:review@catalerum\r
DTSTART:20260613T140000Z\r
DTEND:20260613T150000Z\r
SUMMARY:Design review\r
END:VEVENT\r
END:VCALENDAR\r
";

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn local_ics_sync_is_idempotent_against_fixture_dir() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping local_ics_sync_is_idempotent_against_fixture_dir: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    // --- fixture directory with one .ics calendar ---------------------------
    let dir = tempfile::tempdir().expect("tempdir");
    let cal_path = dir.path().join("work.ics");
    std::fs::write(&cal_path, ICS).expect("write fixture");

    // --- connect + migrate (store is the only migrator) ---------------------
    let store = Store::connect(&url).await.expect("connect+migrate store");

    // A workspace to own the connection (FK target).
    let ws = store
        .workspaces()
        .create(
            "ingest-test",
            &format!("ingest-test-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("create workspace");

    // A local-ics calendar connection pointing at the fixture dir.
    let config = serde_json::json!({
        "provider": "local",
        "path": dir.path().to_string_lossy(),
    });
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "fixture",
            None,
            Some(config),
        )
        .await
        .expect("create connection");

    // --- first sync: discovers the calendar + both events -------------------
    let first = catalerum_ingest::sync_connection(&store, ws.id, conn.id)
        .await
        .expect("first sync");
    assert_eq!(first.calendars, 1, "one .ics file → one calendar");
    assert_eq!(first.events_upserted, 2, "both VEVENTs upserted");
    assert_eq!(first.events_deleted, 0);
    assert!(first.changed, "first sync advances the cursor");

    // The events are persisted and queryable.
    let events = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            DateRange::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .expect("list events");
    assert_eq!(events.len(), 2);
    let mut summaries: Vec<_> = events.iter().map(|e| e.summary.clone()).collect();
    summaries.sort();
    assert_eq!(summaries, vec!["Daily standup", "Design review"]);

    // --- second sync, no source change: a no-op (SOUL §3.4) -----------------
    let second = catalerum_ingest::sync_connection(&store, ws.id, conn.id)
        .await
        .expect("second sync");
    assert_eq!(second.calendars, 1);
    assert_eq!(
        second.events_upserted, 0,
        "re-sync of unchanged source writes nothing"
    );
    assert_eq!(second.events_deleted, 0, "and deletes nothing");
    assert!(!second.changed, "cursor unchanged → no-op");
    assert!(second.is_noop());

    // Still exactly two events: re-running never duplicated.
    let again = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            DateRange::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .expect("list events again");
    assert_eq!(again.len(), 2, "idempotent: no duplicates");

    // --- change the source: an event is removed, another edited -------------
    let edited = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//catalerum//test//EN\r
BEGIN:VEVENT\r
UID:standup@catalerum\r
DTSTART:20260613T090000Z\r
DTEND:20260613T093000Z\r
SUMMARY:Daily standup (moved)\r
END:VEVENT\r
END:VCALENDAR\r
";
    std::fs::write(&cal_path, edited).expect("rewrite fixture");

    let third = catalerum_ingest::sync_connection(&store, ws.id, conn.id)
        .await
        .expect("third sync");
    assert!(third.changed, "edited source advances the cursor");
    assert_eq!(third.events_upserted, 1, "the surviving event re-upserts");
    assert_eq!(
        third.events_deleted, 1,
        "the removed event is reconciled away"
    );

    let final_events = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            DateRange::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .expect("list final events");
    assert_eq!(final_events.len(), 1);
    assert_eq!(final_events[0].summary, "Daily standup (moved)");

    // --- cleanup ------------------------------------------------------------
    store
        .connections()
        .delete(ws.id, conn.id)
        .await
        .expect("delete connection (cascades calendars + events)");
    store
        .workspaces()
        .delete(ws.id)
        .await
        .expect("delete workspace");
}
