//! Integration test: the `NoteRepo` CRUD + workspace-isolation contract
//! (SOUL §21, §6.1/§18). Create / get / list / update / delete round-trip, and
//! a note in one workspace is invisible (and immutable / undeletable) from
//! another — cross-workspace access is impossible by construction.
//!
//! Same DB gating as the ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes so the suite stays
//! green offline.

use catalerum_core::model::Author;
use catalerum_core::UserId;
use catalerum_store::{Store, StoreError, DEFAULT_NOTE_LIMIT};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn note_crud_round_trips_and_is_workspace_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping note_crud_round_trips_and_is_workspace_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");

    // Two distinct workspaces to prove cross-workspace isolation.
    let ws_a = store
        .workspaces()
        .create("notes-a", &format!("notes-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace a");
    let ws_b = store
        .workspaces()
        .create("notes-b", &format!("notes-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace b");

    let author = Author::User { id: UserId::new() };

    // --- create + get round-trip --------------------------------------------
    let created = store
        .notes()
        .create(
            ws_a.id,
            author,
            "Groceries",
            "- milk\n- eggs",
            &["home".to_string(), "shopping".to_string()],
        )
        .await
        .expect("create");
    assert_eq!(created.title, "Groceries");
    assert_eq!(created.markdown, "- milk\n- eggs");
    assert_eq!(
        created.tags,
        vec!["home".to_string(), "shopping".to_string()]
    );
    assert_eq!(created.author, author);
    assert_eq!(created.workspace_id, ws_a.id);

    let fetched = store.notes().get(ws_a.id, created.id).await.expect("get");
    assert_eq!(fetched, created);

    // --- list scoped to the workspace ---------------------------------------
    let listed = store
        .notes()
        .list_by_workspace(ws_a.id, DEFAULT_NOTE_LIMIT)
        .await
        .expect("list a");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.id);
    // Workspace B sees nothing.
    assert!(store
        .notes()
        .list_by_workspace(ws_b.id, DEFAULT_NOTE_LIMIT)
        .await
        .expect("list b")
        .is_empty());

    // --- update bumps fields + updated_at, preserves author -----------------
    let updated = store
        .notes()
        .update(
            ws_a.id,
            created.id,
            "Groceries (wk2)",
            "- bread",
            &["home".to_string()],
        )
        .await
        .expect("update");
    assert_eq!(updated.title, "Groceries (wk2)");
    assert_eq!(updated.markdown, "- bread");
    assert_eq!(updated.tags, vec!["home".to_string()]);
    assert_eq!(updated.author, author, "author is immutable");
    assert!(
        updated.updated_at >= created.updated_at,
        "updated_at is bumped"
    );

    // --- cross-workspace access is impossible by construction ----------------
    // Workspace B cannot get / update / delete workspace A's note.
    assert!(matches!(
        store.notes().get(ws_b.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store
            .notes()
            .update(ws_b.id, created.id, "hijack", "x", &[])
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.notes().delete(ws_b.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    // ...and A's note is untouched after the failed cross-workspace writes.
    let still_there = store.notes().get(ws_a.id, created.id).await.expect("get a");
    assert_eq!(still_there.title, "Groceries (wk2)");

    // --- delete (scoped) -----------------------------------------------------
    store
        .notes()
        .delete(ws_a.id, created.id)
        .await
        .expect("delete");
    assert!(matches!(
        store.notes().get(ws_a.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    // Deleting a second time is a NotFound (idempotent at the API's 404 level).
    assert!(matches!(
        store.notes().delete(ws_a.id, created.id).await,
        Err(StoreError::NotFound)
    ));

    // cleanup
    store
        .workspaces()
        .delete(ws_a.id)
        .await
        .expect("delete ws a");
    store
        .workspaces()
        .delete(ws_b.id)
        .await
        .expect("delete ws b");
}

#[tokio::test]
async fn list_by_workspace_is_bounded_and_newest_first() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping list_by_workspace_is_bounded_and_newest_first: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "notesbound",
            &format!("notesbound-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");

    // Five notes, created in sequence (increasing updated_at).
    for i in 0..5 {
        store
            .notes()
            .create(
                ws.id,
                Author::User { id: UserId::new() },
                &format!("note {i}"),
                "",
                &[],
            )
            .await
            .expect("create");
    }

    // §18: a small limit returns exactly that many, most-recently-edited first.
    let listed = store
        .notes()
        .list_by_workspace(ws.id, 2)
        .await
        .expect("list bounded");
    assert_eq!(listed.len(), 2, "the result is bounded to the limit");
    assert!(
        listed[0].updated_at >= listed[1].updated_at,
        "ordered newest-first (updated_at DESC)"
    );
    // A generous limit returns them all; a 0 limit is floored to 1 (never empty-via-0).
    assert_eq!(
        store
            .notes()
            .list_by_workspace(ws.id, DEFAULT_NOTE_LIMIT)
            .await
            .expect("list all")
            .len(),
        5
    );
    assert_eq!(
        store
            .notes()
            .list_by_workspace(ws.id, 0)
            .await
            .expect("list floored")
            .len(),
        1,
        "a limit of 0 is floored to 1"
    );

    store.workspaces().delete(ws.id).await.expect("cleanup");
}
