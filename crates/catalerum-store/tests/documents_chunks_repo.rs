//! Integration test: the `DocumentRepo` + `ChunkRepo` contract
//! (SOUL §5/§6.4/§10/§18). Covers what the ingest-pipeline test does not exercise
//! directly: `upsert_by_source` idempotency (a stable document id across
//! re-upserts), `get_by_source` workspace isolation, `replace_for_document`'s
//! wholesale replacement + its **tenancy guard** (a document_id from another
//! workspace is rejected with `NotFound`), and `delete_by_source` cascading to
//! chunks.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes so the suite
//! stays green offline.

use catalerum_core::{NoteId, SourceRef};
use catalerum_store::{NewChunk, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn documents_and_chunks_round_trip_with_tenancy_guards() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping documents_and_chunks_round_trip_with_tenancy_guards: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");

    let ws_a = store
        .workspaces()
        .create("docs-a", &format!("docs-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace a");
    let ws_b = store
        .workspaces()
        .create("docs-b", &format!("docs-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace b");

    let source = SourceRef::Note { id: NoteId::new() };

    // --- upsert_by_source is idempotent on the source: stable id ------------
    let doc1 = store
        .documents()
        .upsert_by_source(ws_a.id, &source, "first text", None)
        .await
        .expect("first upsert");
    let doc2 = store
        .documents()
        .upsert_by_source(ws_a.id, &source, "second text", Some("a summary"))
        .await
        .expect("second upsert");
    assert_eq!(doc1.id, doc2.id, "same source upserts one stable document");
    assert_eq!(doc2.text, "second text");
    assert_eq!(doc2.summary.as_deref(), Some("a summary"));
    assert_eq!(doc2.source, source);

    // get_by_source reads it back in-workspace; another workspace sees nothing.
    assert_eq!(
        store
            .documents()
            .get_by_source(ws_a.id, &source)
            .await
            .unwrap()
            .map(|d| d.id),
        Some(doc1.id)
    );
    assert!(store
        .documents()
        .get_by_source(ws_b.id, &source)
        .await
        .unwrap()
        .is_none());

    // --- replace_for_document writes a dense chunk set ----------------------
    let chunks = [
        NewChunk::new(0, "alpha", Some(uuid::Uuid::new_v4())),
        NewChunk::new(1, "bravo", Some(uuid::Uuid::new_v4())),
        NewChunk::new(2, "charlie", None),
    ];
    let stored = store
        .chunks()
        .replace_for_document(ws_a.id, doc1.id, &chunks)
        .await
        .expect("replace chunks");
    assert_eq!(stored.len(), 3);

    let listed = store
        .chunks()
        .list_by_document(ws_a.id, doc1.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(listed[0].text, "alpha");
    assert!(listed[0].qdrant_point_id.is_some());
    assert!(listed[2].qdrant_point_id.is_none());
    assert_eq!(store.chunks().count_by_workspace(ws_a.id).await.unwrap(), 3);

    // Replacing wholesale shrinks the set (no drift).
    let stored2 = store
        .chunks()
        .replace_for_document(ws_a.id, doc1.id, &[NewChunk::new(0, "only", None)])
        .await
        .expect("replace again");
    assert_eq!(stored2.len(), 1);
    assert_eq!(store.chunks().count_by_workspace(ws_a.id).await.unwrap(), 1);

    // --- tenancy guard: ws_b cannot write chunks into ws_a's document -------
    let guarded = store
        .chunks()
        .replace_for_document(ws_b.id, doc1.id, &[NewChunk::new(0, "intruder", None)])
        .await;
    assert!(
        matches!(guarded, Err(StoreError::NotFound)),
        "cross-workspace chunk write must be rejected, got {guarded:?}"
    );
    // ws_a's chunks are untouched by the rejected cross-workspace write.
    assert_eq!(store.chunks().count_by_workspace(ws_a.id).await.unwrap(), 1);

    // --- delete_by_source cascades to chunks --------------------------------
    assert!(store
        .documents()
        .delete_by_source(ws_a.id, &source)
        .await
        .unwrap());
    assert!(store
        .documents()
        .get_by_source(ws_a.id, &source)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.chunks().count_by_workspace(ws_a.id).await.unwrap(),
        0,
        "ON DELETE CASCADE drops the document's chunks"
    );
    // A second delete is a no-op (returns false).
    assert!(!store
        .documents()
        .delete_by_source(ws_a.id, &source)
        .await
        .unwrap());
}
