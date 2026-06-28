//! Integration test: the `LinkRepo` CRUD + workspace-isolation contract
//! (SOUL §5/§6.3, §6.1/§18). Create / get / list / list_for / delete round-trip,
//! idempotent re-linking, self-link rejection, bidirectional `list_for`, and
//! cross-workspace invisibility.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{Author, SourceRef};
use catalerum_core::{EventId, NoteId, ObjectId, UserId};
use catalerum_store::{Store, StoreError, DEFAULT_LINK_LIMIT};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn link_crud_round_trips_is_idempotent_and_workspace_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping link_crud_round_trips_is_idempotent_and_workspace_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");

    let ws_a = store
        .workspaces()
        .create("links-a", &format!("links-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace a");
    let ws_b = store
        .workspaces()
        .create("links-b", &format!("links-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace b");

    let author = Author::User { id: UserId::new() };
    let note = SourceRef::Note { id: NoteId::new() };
    let event = SourceRef::Event { id: EventId::new() };

    // --- create + get round-trip --------------------------------------------
    let created = store
        .links()
        .create(
            ws_a.id,
            author,
            &note,
            &event,
            Some("follow-up"),
            Some("agenda"),
        )
        .await
        .expect("create");
    assert_eq!(created.from, note);
    assert_eq!(created.to, event);
    assert_eq!(created.label.as_deref(), Some("follow-up"));
    assert_eq!(created.note.as_deref(), Some("agenda"));
    assert_eq!(created.author, author);
    assert_eq!(created.workspace_id, ws_a.id);

    let fetched = store.links().get(ws_a.id, created.id).await.expect("get");
    assert_eq!(fetched, created);

    // --- idempotent: same (from,to,label) refreshes the note, keeps the id ---
    let again = store
        .links()
        .create(
            ws_a.id,
            author,
            &note,
            &event,
            Some("follow-up"),
            Some("new note"),
        )
        .await
        .expect("re-create");
    assert_eq!(again.id, created.id, "same relationship → same row, no dup");
    assert_eq!(again.note.as_deref(), Some("new note"));
    assert_eq!(
        store
            .links()
            .list_by_workspace(ws_a.id, DEFAULT_LINK_LIMIT)
            .await
            .expect("list a")
            .len(),
        1,
        "idempotent create never duplicates"
    );

    // --- a different label between the same pair is a distinct link ----------
    let other = store
        .links()
        .create(ws_a.id, author, &note, &event, Some("duplicate-of"), None)
        .await
        .expect("second label");
    assert_ne!(other.id, created.id);

    // --- list_for finds links in *both* directions --------------------------
    // `note` is the `from` end of both links.
    assert_eq!(
        store
            .links()
            .list_for(ws_a.id, &note, DEFAULT_LINK_LIMIT)
            .await
            .expect("for note")
            .len(),
        2
    );
    // `event` is the `to` end — still returned (reverse direction).
    let for_event = store
        .links()
        .list_for(ws_a.id, &event, DEFAULT_LINK_LIMIT)
        .await
        .expect("for event");
    assert_eq!(for_event.len(), 2);
    // An unrelated endpoint has no links.
    assert!(store
        .links()
        .list_for(
            ws_a.id,
            &SourceRef::Object {
                id: ObjectId::new()
            },
            DEFAULT_LINK_LIMIT
        )
        .await
        .expect("for unrelated")
        .is_empty());

    // --- self-link is rejected ----------------------------------------------
    assert!(matches!(
        store
            .links()
            .create(ws_a.id, author, &note, &note, None, None)
            .await,
        Err(StoreError::Invalid(_))
    ));

    // --- workspace isolation -------------------------------------------------
    assert!(store
        .links()
        .list_by_workspace(ws_b.id, DEFAULT_LINK_LIMIT)
        .await
        .expect("list b")
        .is_empty());
    assert!(matches!(
        store.links().get(ws_b.id, created.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.links().delete(ws_b.id, created.id).await,
        Err(StoreError::NotFound)
    ));

    // --- delete (scoped) -----------------------------------------------------
    store
        .links()
        .delete(ws_a.id, created.id)
        .await
        .expect("delete");
    assert!(matches!(
        store.links().get(ws_a.id, created.id).await,
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
