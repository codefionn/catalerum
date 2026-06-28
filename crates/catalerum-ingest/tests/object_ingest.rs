//! Integration test: object ingestion (SOUL §9/§10) — a stored object's bytes
//! become a catalogued `documents` row linked from `objects.extracted_text_id`.
//! Drives `ObjectIngestContext::ingest` directly (the worker arm's body) over a
//! real `LocalFsBackend` — deterministic and free of the shared `job_queue`,
//! which parallel sibling test binaries contend on — plus the binary-object skip
//! and the deleted-object purge.
//!
//! Same DB gating as the other ingest tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use std::sync::Arc;

use catalerum_core::model::{ConnectionKind, SourceRef};
use catalerum_core::provider::{workspace_object_key, PutMeta, StorageBackend};
use catalerum_ingest::ObjectIngestContext;
use catalerum_storage::LocalFsBackend;
use catalerum_store::Store;
use futures::stream::{self, StreamExt};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

async fn put_file(backend: &LocalFsBackend, key: &str, bytes: &[u8], content_type: &str) {
    let owned = bytes.to_vec();
    let data = stream::once(async move { Ok(owned) }).boxed();
    backend
        .put(
            key,
            data,
            PutMeta {
                content_type: Some(content_type.to_string()),
                content_length: Some(bytes.len() as u64),
            },
        )
        .await
        .expect("put file");
}

#[tokio::test]
async fn object_ingest_extracts_links_skips_binary_and_purges() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping object_ingest_extracts_links_skips_binary_and_purges: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("oing", &format!("oing-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "local-storage", None, None)
        .await
        .expect("connection");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "files", None)
        .await
        .expect("bucket");

    let dir = tempfile::tempdir().expect("tempdir");
    let backend = LocalFsBackend::new(dir.path().to_path_buf());

    // --- A text object: ingest extracts its text + links the document ---------
    const BODY: &str = "# Readme\n\nCatalogue the world.";
    // The worker reads from the workspace-namespaced physical key (SOUL §18), so
    // write the blob there — exactly where the storage route would have put it.
    put_file(
        &backend,
        &workspace_object_key(ws.id, "docs/readme.md"),
        BODY.as_bytes(),
        "text/markdown",
    )
    .await;
    let now = chrono::Utc::now();
    let text_obj = store
        .objects()
        .upsert(&catalerum_store::UpsertObject {
            workspace_id: ws.id,
            bucket_id: bucket.id,
            key: "docs/readme.md",
            size: BODY.len() as u64,
            content_type: Some("text/markdown"),
            etag: Some("e1"),
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("catalogue text object");

    // Drive the ingest directly (the worker arm's body is just this call) —
    // deterministic and free of the shared job_queue, which parallel sibling test
    // binaries otherwise contend on (a single `poll_once` could claim their job).
    let ctx = ObjectIngestContext::single(Arc::new(backend) as Arc<dyn StorageBackend>);
    ctx.ingest(&store, None, ws.id, text_obj.id)
        .await
        .expect("ingest object");

    // The document holds the object's text, linked from the object row.
    let source = SourceRef::Object { id: text_obj.id };
    let doc = store
        .documents()
        .get_by_source(ws.id, &source)
        .await
        .expect("doc query")
        .expect("document exists");
    assert_eq!(doc.text, BODY, "extracted text matches the file bytes");
    let linked = store
        .objects()
        .get(ws.id, text_obj.id)
        .await
        .expect("get obj");
    assert_eq!(
        linked.extracted_text_id,
        Some(doc.id),
        "object links its extracted-text document"
    );

    // The document existing + linked above already proves the worker dispatched
    // and completed this test's `ingest_object` job. (We intentionally do NOT
    // assert a global `count_by_status(Pending) == 0` — the ingest tests share one
    // ephemeral job_queue, so a sibling test's in-flight job would make that
    // global count fragile; the per-job effect, asserted above, is the contract.)

    // --- text→binary overwrite PURGES the prior document (Fix: §3.1/§10) ------
    // Re-catalogue the SAME key (id preserved) with a binary content type, as a
    // re-upload would; re-ingest must purge the previously-extracted document so
    // no orphan (document or, with an embed ctx, vectors) survives.
    let overwrite = store
        .objects()
        .upsert(&catalerum_store::UpsertObject {
            workspace_id: ws.id,
            bucket_id: bucket.id,
            key: "docs/readme.md",
            size: 8,
            content_type: Some("application/octet-stream"),
            etag: Some("e2"),
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("overwrite to binary");
    assert_eq!(overwrite.id, text_obj.id, "same object id across overwrite");
    let ctx =
        ObjectIngestContext::single(Arc::new(backend_reopen(&dir)) as Arc<dyn StorageBackend>);
    let ow_report = ctx
        .ingest(&store, None, ws.id, text_obj.id)
        .await
        .expect("ingest overwrite");
    assert_eq!(
        ow_report.document_id, None,
        "overwrite-to-binary yields no document"
    );
    assert!(
        store
            .documents()
            .get_by_source(ws.id, &source)
            .await
            .unwrap()
            .is_none(),
        "the prior text document was purged on text→binary overwrite"
    );
    assert_eq!(
        store
            .objects()
            .get(ws.id, text_obj.id)
            .await
            .unwrap()
            .extracted_text_id,
        None,
        "the extracted-text link was cleared"
    );

    // --- A binary object: no text extracted, no document linked ---------------
    put_file(
        &backend_reopen(&dir),
        "img/logo.png",
        &[0x89, 0x50, 0x4e, 0x47],
        "image/png",
    )
    .await;
    let bin_obj = store
        .objects()
        .upsert(&catalerum_store::UpsertObject {
            workspace_id: ws.id,
            bucket_id: bucket.id,
            key: "img/logo.png",
            size: 4,
            content_type: Some("image/png"),
            etag: None,
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("catalogue binary object");
    let ctx =
        ObjectIngestContext::single(Arc::new(backend_reopen(&dir)) as Arc<dyn StorageBackend>);
    let report = ctx
        .ingest(&store, None, ws.id, bin_obj.id)
        .await
        .expect("ingest binary");
    assert_eq!(report.document_id, None, "binary object yields no document");
    assert_eq!(report.text_bytes, 0);
    let bin_linked = store.objects().get(ws.id, bin_obj.id).await.unwrap();
    assert_eq!(bin_linked.extracted_text_id, None);
    assert!(store
        .documents()
        .get_by_source(ws.id, &SourceRef::Object { id: bin_obj.id })
        .await
        .unwrap()
        .is_none());

    // --- Deleted object: ingest PURGES its document (reconcile, §3.1/§10) ------
    store
        .objects()
        .delete_by_key(ws.id, bucket.id, "docs/readme.md")
        .await
        .expect("delete object row");
    let purge = ctx
        .ingest(&store, None, ws.id, text_obj.id)
        .await
        .expect("ingest purge");
    assert_eq!(purge.document_id, None, "purge reports no document");
    assert!(
        store
            .documents()
            .get_by_source(ws.id, &source)
            .await
            .unwrap()
            .is_none(),
        "the extracted-text document was purged"
    );
}

/// Re-open the bucket dir as a fresh backend handle (cheap; the backend is a thin
/// path wrapper) so each use owns its `Arc<dyn StorageBackend>`.
fn backend_reopen(dir: &tempfile::TempDir) -> LocalFsBackend {
    LocalFsBackend::new(dir.path().to_path_buf())
}
