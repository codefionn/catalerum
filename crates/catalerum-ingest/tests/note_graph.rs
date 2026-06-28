//! Integration test: note → Neo4j graph projection (SOUL §6.3/§10/§21).
//!
//! Proves the projection contract: a note projects to a `:Note` node plus a
//! `:Topic` node per (deduped) tag, linked by `REFERENCES`; re-projection is
//! idempotent (no dups); and a deleted note's `:Note` node is **purged** (the
//! reconcile path). Also drives the durable worker: an enqueued `project_note`
//! job is claimed and run by a graph-capable [`SyncWorker`].
//!
//! Requires BOTH a Postgres and a Neo4j. Set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) and `NEO4J_URL` (+ optional `NEO4J_USER`/`NEO4J_PASSWORD`,
//! defaulting to `neo4j`/`catalerum`); with either unset the test prints a skip
//! note and passes, so the suite stays green offline.

use std::time::Duration;

use catalerum_core::model::Author;
use catalerum_core::{NoteId, UserId};
use catalerum_graph::{GraphStore, NodeRef};
use catalerum_ingest::{
    enqueue_project_event, enqueue_project_note, project_event_to_graph, project_note_to_graph,
    GraphContext, SyncWorker,
};
use catalerum_store::{JobStatus, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn graph_store() -> Option<GraphStore> {
    let url = std::env::var("NEO4J_URL").ok()?;
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
    let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "catalerum".into());
    Some(
        GraphStore::new(&url)
            .expect("valid NEO4J_URL")
            .with_auth(user, password),
    )
}

async fn make_note(store: &Store, tags: &[&str]) -> (catalerum_core::WorkspaceId, NoteId) {
    let ws = store
        .workspaces()
        .create("graph", &format!("graph-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let note = store
        .notes()
        .create(
            ws.id,
            Author::User { id: UserId::new() },
            "Roadmap sync",
            "Align on Q3 themes.",
            &tags.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .await
        .expect("note");
    (ws.id, note.id)
}

#[tokio::test]
async fn project_note_round_trips_idempotently_and_purges_on_delete() {
    let (Some(db), Some(graph)) = (test_db_url(), graph_store()) else {
        eprintln!(
            "skipping project_note_round_trips_idempotently_and_purges_on_delete: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and NEO4J_URL"
        );
        return;
    };
    let store = Store::connect(&db).await.expect("store");
    graph.ensure_indexes().await.expect("indexes");

    // Two tags (+ a duplicate-by-case + blank) → 2 distinct Topic nodes.
    let (ws, note_id) = make_note(&store, &["Planning", "planning", "Q3"]).await;
    graph.delete_workspace(ws).await.unwrap(); // clean slate

    let report = project_note_to_graph(&store, &graph, ws, note_id)
        .await
        .expect("project");
    assert!(!report.purged);
    assert_eq!(report.topics, 2, "deduped tags → 2 topics");

    // :Note + 2 :Topic = 3 nodes; the note REFERENCES both topics.
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 3);
    assert_eq!(graph.references_of(ws, note_id).await.unwrap().len(), 2);

    // Idempotent: re-projecting adds nothing.
    project_note_to_graph(&store, &graph, ws, note_id)
        .await
        .unwrap();
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 3);

    // Delete the note, then reconcile: the :Note node is purged, topics remain.
    store
        .notes()
        .delete(ws, note_id)
        .await
        .expect("delete note");
    let purge = project_note_to_graph(&store, &graph, ws, note_id)
        .await
        .expect("purge");
    assert!(purge.purged);
    assert!(graph.references_of(ws, note_id).await.unwrap().is_empty());
    assert_eq!(
        graph.count_nodes(ws).await.unwrap(),
        2,
        "topics survive the note delete"
    );

    graph.delete_workspace(ws).await.unwrap();
}

#[tokio::test]
async fn worker_dispatches_project_note_job_to_the_graph_context() {
    let (Some(db), Some(graph)) = (test_db_url(), graph_store()) else {
        eprintln!(
            "skipping worker_dispatches_project_note_job_to_the_graph_context: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and NEO4J_URL"
        );
        return;
    };
    let store = Store::connect(&db).await.expect("store");
    graph.ensure_indexes().await.expect("indexes");

    let (ws, note_id) = make_note(&store, &["ops"]).await;
    graph.delete_workspace(ws).await.unwrap();

    // A worker WITH a graph context (the binary's wiring).
    let worker =
        SyncWorker::new(store.clone()).with_graph_context(GraphContext::new(graph.clone()));

    let job_id = enqueue_project_note(&store, ws, note_id)
        .await
        .expect("enqueue project_note");

    let mut terminal = None;
    for _ in 0..40 {
        let row = store.job_queue().get(job_id).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            terminal = Some((row.status().unwrap(), row.last_error.clone()));
            break;
        }
        if !worker.poll_once().await.expect("poll_once") {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let (status, last_error) = terminal.expect("project_note job observed terminal");
    assert_eq!(
        status,
        JobStatus::Done,
        "the worker dispatched project_note to the graph context; last_error = {last_error:?}"
    );

    // :Note + 1 :Topic landed in the graph via the worker.
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 2);
    assert_eq!(graph.references_of(ws, note_id).await.unwrap().len(), 1);
    // The note node exists and is addressable.
    let _ = NodeRef::note(note_id);

    graph.delete_workspace(ws).await.unwrap();
}

/// Seed a calendar event (its own connection + calendar) and return the workspace
/// and the stored event, for the event-projection tests.
async fn make_event(store: &Store) -> (catalerum_core::WorkspaceId, catalerum_core::Event) {
    use catalerum_core::model::ConnectionKind;
    use catalerum_store::UpsertEvent;
    let ws = store
        .workspaces()
        .create("evgraph", &format!("evgraph-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let conn = store
        .connections()
        .ensure(ws.id, ConnectionKind::Calendar, "cal", None, None)
        .await
        .expect("calendar connection");
    let cal = store
        .calendars()
        .upsert(ws.id, conn.id, "ext-cal", "Work", false)
        .await
        .expect("calendar");
    let now = chrono::Utc::now();
    let event = store
        .events()
        .upsert_by_uid(&UpsertEvent {
            workspace_id: ws.id,
            calendar_id: cal.id,
            uid: "evt-graph-1",
            starts_at: now,
            ends_at: now + chrono::Duration::hours(1),
            all_day: false,
            rrule: None,
            summary: "Quarterly review",
            location: Some("Room 5"),
            body: None,
            attendees: &[],
            // One label → one `:Topic` node + `ABOUT` edge on projection.
            labels: &["Quarterly".to_string()],
            attachments: &[],
            etag: None,
            sequence: 0,
        })
        .await
        .expect("event");
    (ws.id, event)
}

#[tokio::test]
async fn project_event_round_trips_idempotently_and_purges_on_delete() {
    let (Some(db), Some(graph)) = (test_db_url(), graph_store()) else {
        eprintln!(
            "skipping project_event_round_trips_idempotently_and_purges_on_delete: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and NEO4J_URL"
        );
        return;
    };
    let store = Store::connect(&db).await.expect("store");
    graph.ensure_indexes().await.expect("indexes");

    let (ws, event) = make_event(&store).await;
    graph.delete_workspace(ws).await.unwrap(); // clean slate

    let purged = project_event_to_graph(&store, &graph, ws, event.id)
        .await
        .expect("project");
    assert!(!purged);
    // :Event + its :Calendar (the SCHEDULED_IN endpoint) + its one label :Topic
    // (the ABOUT endpoint) = 3 nodes.
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 3);
    // The distinctive output: the event is SCHEDULED_IN exactly its own calendar
    // (proves the edge's type, direction, and endpoint — not just "3 nodes exist").
    assert_eq!(
        graph.scheduled_in(ws, event.id).await.unwrap(),
        vec![event.calendar_id.to_string()],
        "the :Event is SCHEDULED_IN its :Calendar"
    );
    // …and ABOUT exactly one :Topic (its single label).
    assert_eq!(
        graph.about_of(ws, event.id).await.unwrap().len(),
        1,
        "the :Event is ABOUT its one label :Topic"
    );

    // Idempotent: re-projecting adds nothing (still one edge to the one calendar
    // and one to the one topic).
    project_event_to_graph(&store, &graph, ws, event.id)
        .await
        .unwrap();
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 3);
    assert_eq!(graph.scheduled_in(ws, event.id).await.unwrap().len(), 1);
    assert_eq!(graph.about_of(ws, event.id).await.unwrap().len(), 1);

    // Delete the event, then reconcile: the :Event node is purged; the :Calendar
    // and shared :Topic nodes survive (other events may schedule into / be about them).
    store
        .events()
        .delete_by_uid(ws, event.calendar_id, &event.uid)
        .await
        .expect("delete event");
    let purge = project_event_to_graph(&store, &graph, ws, event.id)
        .await
        .expect("purge");
    assert!(purge, "a deleted event reports purged");
    assert_eq!(
        graph.count_nodes(ws).await.unwrap(),
        2,
        "the calendar + topic nodes survive the event delete"
    );
    assert!(
        graph.scheduled_in(ws, event.id).await.unwrap().is_empty(),
        "the purged event has no remaining SCHEDULED_IN edge"
    );
    assert!(
        graph.about_of(ws, event.id).await.unwrap().is_empty(),
        "the purged event has no remaining ABOUT edge"
    );

    graph.delete_workspace(ws).await.unwrap();
}

#[tokio::test]
async fn worker_dispatches_project_event_job_to_the_graph_context() {
    let (Some(db), Some(graph)) = (test_db_url(), graph_store()) else {
        eprintln!(
            "skipping worker_dispatches_project_event_job_to_the_graph_context: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and NEO4J_URL"
        );
        return;
    };
    let store = Store::connect(&db).await.expect("store");
    graph.ensure_indexes().await.expect("indexes");

    let (ws, event) = make_event(&store).await;
    graph.delete_workspace(ws).await.unwrap();

    let worker =
        SyncWorker::new(store.clone()).with_graph_context(GraphContext::new(graph.clone()));
    let job_id = enqueue_project_event(&store, ws, event.id)
        .await
        .expect("enqueue project_event");

    let mut terminal = None;
    for _ in 0..40 {
        let row = store.job_queue().get(job_id).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            terminal = Some((row.status().unwrap(), row.last_error.clone()));
            break;
        }
        if !worker.poll_once().await.expect("poll_once") {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let (status, last_error) = terminal.expect("project_event job observed terminal");
    assert_eq!(
        status,
        JobStatus::Done,
        "the worker dispatched project_event to the graph context; last_error = {last_error:?}"
    );
    // :Event + :Calendar + label :Topic landed via the worker, with the
    // SCHEDULED_IN and ABOUT edges.
    assert_eq!(graph.count_nodes(ws).await.unwrap(), 3);
    assert_eq!(
        graph.scheduled_in(ws, event.id).await.unwrap(),
        vec![event.calendar_id.to_string()]
    );
    assert_eq!(graph.about_of(ws, event.id).await.unwrap().len(), 1);

    graph.delete_workspace(ws).await.unwrap();
}
