//! Integration test: `ObjectRepo::search_text_in_workspace` (SOUL §9/§10/§18) —
//! search objects by their §10 extracted-text content, with a match-windowed
//! excerpt, workspace-scoped, only matching ingested objects, blank query → none.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{ConnectionKind, SourceRef};
use catalerum_store::{Store, UpsertObject};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn object_text_search_is_scoped_excerpted_and_ingest_only() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping object_text_search_is_scoped_excerpted_and_ingest_only: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("obj", &format!("obj-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("obj-b", &format!("obj-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // A bucket per workspace (backed by a storage connection).
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "s", None, None)
        .await
        .expect("conn");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "default", None)
        .await
        .expect("bucket");
    let other_conn = store
        .connections()
        .create(other.id, ConnectionKind::Storage, "s", None, None)
        .await
        .expect("other conn");
    let other_bucket = store
        .buckets()
        .ensure(other.id, other_conn.id, "default", None)
        .await
        .expect("other bucket");

    let now = chrono::Utc::now();
    let mk = |ws_id, bucket_id, key: &'static str| UpsertObject {
        workspace_id: ws_id,
        bucket_id,
        key,
        size: 0,
        content_type: Some("text/plain"),
        etag: None,
        last_modified: now,
        sha256: None,
    };

    // An ingested object: catalogue it, store its extracted text, link them.
    let report = store
        .objects()
        .upsert(&mk(ws.id, bucket.id, "report.txt"))
        .await
        .expect("o1");
    let long = format!("{} PINEAPPLE harvest {}", "x".repeat(200), "y".repeat(200));
    let doc = store
        .documents()
        .upsert_by_source(ws.id, &SourceRef::Object { id: report.id }, &long, None)
        .await
        .expect("doc");
    store
        .objects()
        .set_extracted_text(ws.id, report.id, Some(doc.id))
        .await
        .expect("link text");

    // A non-ingested object (no extracted text) — must never match.
    store
        .objects()
        .upsert(&mk(ws.id, bucket.id, "raw.bin"))
        .await
        .expect("o2");

    // Another workspace's ingested object that DOES contain the term — must not leak.
    let leak = store
        .objects()
        .upsert(&mk(other.id, other_bucket.id, "leak.txt"))
        .await
        .expect("o3");
    let leak_doc = store
        .documents()
        .upsert_by_source(
            other.id,
            &SourceRef::Object { id: leak.id },
            "secret pineapple",
            None,
        )
        .await
        .expect("leak doc");
    store
        .objects()
        .set_extracted_text(other.id, leak.id, Some(leak_doc.id))
        .await
        .expect("link leak");

    // Case-insensitive content match, scoped to `ws`, with a windowed excerpt.
    let hits = store
        .objects()
        .search_text_in_workspace(ws.id, "pineapple", 50)
        .await
        .expect("search");
    assert_eq!(hits.len(), 1, "only the in-workspace ingested object");
    assert_eq!(hits[0].key, "report.txt");
    assert_eq!(
        hits[0].bucket_id, bucket.id,
        "the hit carries its bucket so a caller can resolve its store"
    );
    assert!(
        hits[0].excerpt.to_lowercase().contains("pineapple"),
        "excerpt is windowed on the match: {:?}",
        hits[0].excerpt
    );
    assert!(
        hits[0].excerpt.len() < long.len(),
        "excerpt is bounded, not the whole document"
    );

    // Blank query → nothing; a literal `%` matches nothing (no LIKE wildcard).
    assert!(store
        .objects()
        .search_text_in_workspace(ws.id, "   ", 50)
        .await
        .expect("blank")
        .is_empty());
    assert!(store
        .objects()
        .search_text_in_workspace(ws.id, "%", 50)
        .await
        .expect("pct")
        .is_empty());
}
