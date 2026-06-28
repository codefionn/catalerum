//! Integration test: the `ObjectLabelRepo` CRUD + workspace-isolation contract
//! (SOUL §9, §6.1/§18). Add / list_for / list_by_store / list_by_label / delete
//! round-trip, idempotent re-labelling, blank rejection, directory vs file
//! labels, path-purge cleanup, and cross-workspace invisibility.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::Author;
use catalerum_core::UserId;
use catalerum_store::{Store, StoreError, DEFAULT_LABEL_LIMIT};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn object_label_crud_round_trips_is_idempotent_and_workspace_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping object_label_crud_round_trips_is_idempotent_and_workspace_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");

    let ws_a = store
        .workspaces()
        .create("labels-a", &format!("labels-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace a");
    let ws_b = store
        .workspaces()
        .create("labels-b", &format!("labels-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace b");

    let author = Author::User { id: UserId::new() };
    let labels = store.object_labels();

    // --- add + list round-trip (a file) -------------------------------------
    let created = labels
        .add(ws_a.id, author, "main", "docs/report.pdf", false, "invoice")
        .await
        .expect("add file label");
    assert_eq!(created.store, "main");
    assert_eq!(created.path, "docs/report.pdf");
    assert!(!created.is_dir);
    assert_eq!(created.label, "invoice");
    assert_eq!(created.author, author);
    assert_eq!(created.workspace_id, ws_a.id);

    let on_path = labels
        .list_for(ws_a.id, "main", "docs/report.pdf")
        .await
        .expect("list_for");
    assert_eq!(on_path.len(), 1);
    assert_eq!(on_path[0], created);

    // --- idempotent: same (store,path,label) keeps the id, no duplicate ------
    let again = labels
        .add(ws_a.id, author, "main", "docs/report.pdf", false, "invoice")
        .await
        .expect("re-add");
    assert_eq!(again.id, created.id, "same label → same row, no dup");
    assert_eq!(
        labels
            .list_for(ws_a.id, "main", "docs/report.pdf")
            .await
            .expect("list_for after re-add")
            .len(),
        1,
        "idempotent add never duplicates"
    );

    // --- a directory label + a second label on the same file ----------------
    let dir_label = labels
        .add(ws_a.id, author, "main", "docs", true, "shared")
        .await
        .expect("add dir label");
    assert!(dir_label.is_dir);
    labels
        .add(ws_a.id, author, "main", "docs/report.pdf", false, "urgent")
        .await
        .expect("second file label");

    // --- blank label / path are rejected ------------------------------------
    assert!(matches!(
        labels
            .add(ws_a.id, author, "main", "docs/report.pdf", false, "   ")
            .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(matches!(
        labels.add(ws_a.id, author, "main", "  ", false, "x").await,
        Err(StoreError::Invalid(_))
    ));

    // --- list_by_store: prefix filter + whole-store ------------------------
    let under_docs = labels
        .list_by_store(ws_a.id, "main", "docs", DEFAULT_LABEL_LIMIT)
        .await
        .expect("list_by_store prefix");
    assert_eq!(
        under_docs.len(),
        3,
        "dir label + two file labels under docs/"
    );
    let all = labels
        .list_by_store(ws_a.id, "main", "", DEFAULT_LABEL_LIMIT)
        .await
        .expect("list_by_store all");
    assert_eq!(all.len(), 3);
    // A different store shares no labels.
    assert!(labels
        .list_by_store(ws_a.id, "other", "", DEFAULT_LABEL_LIMIT)
        .await
        .expect("list_by_store other store")
        .is_empty());

    // --- list_by_label: every path with a given label -----------------------
    assert_eq!(
        labels
            .list_by_label(ws_a.id, "invoice", DEFAULT_LABEL_LIMIT)
            .await
            .expect("by label")
            .len(),
        1
    );

    // --- delete_for_path purges every label on a file (the delete-cleanup) ---
    let purged = labels
        .delete_for_path(ws_a.id, "main", "docs/report.pdf")
        .await
        .expect("delete_for_path");
    assert_eq!(purged, 2, "both file labels removed");
    assert!(labels
        .list_for(ws_a.id, "main", "docs/report.pdf")
        .await
        .expect("list_for after purge")
        .is_empty());
    // The directory label is untouched by a file-path purge.
    assert_eq!(
        labels
            .list_by_store(ws_a.id, "main", "", DEFAULT_LABEL_LIMIT)
            .await
            .expect("remaining")
            .len(),
        1
    );

    // --- workspace isolation -------------------------------------------------
    assert!(labels
        .list_by_store(ws_b.id, "main", "", DEFAULT_LABEL_LIMIT)
        .await
        .expect("list b")
        .is_empty());
    assert!(matches!(
        labels.delete(ws_b.id, dir_label.id).await,
        Err(StoreError::NotFound)
    ));

    // --- delete by id (scoped) ----------------------------------------------
    labels
        .delete(ws_a.id, dir_label.id)
        .await
        .expect("delete dir label");
    assert!(labels
        .list_by_store(ws_a.id, "main", "", DEFAULT_LABEL_LIMIT)
        .await
        .expect("empty after delete")
        .is_empty());
    assert!(matches!(
        labels.delete(ws_a.id, dir_label.id).await,
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
