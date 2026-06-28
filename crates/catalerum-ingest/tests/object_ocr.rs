//! Integration test: OCR at object ingest (SOUL §7/§10) — an image object's
//! bytes run through the `OcrContext` engine and become a catalogued
//! `documents` row, exactly like a text object. Drives
//! `ObjectIngestContext::ingest` directly (the worker arm's body) over a real
//! `LocalFsBackend` with a scriptable fake engine — the trait-object seam means
//! no gateway/binary is needed. Covers the retry contract (permanent rejection
//! → clean skip; transient error → `Err`), the skip-never-truncate size cap,
//! the text-free image, and the purge when OCR is unconfigured again.
//!
//! Same DB gating as the other ingest tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use catalerum_core::error::{Error, Result as CoreResult};
use catalerum_core::model::{ConnectionKind, SourceRef};
use catalerum_core::ocr::{OcrRequest, OcrResponse};
use catalerum_core::provider::{workspace_object_key, OcrEngine, PutMeta, StorageBackend};
use catalerum_ingest::{ObjectIngestContext, OcrContext};
use catalerum_storage::LocalFsBackend;
use catalerum_store::Store;
use futures::stream::{self, StreamExt};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// What the fake engine answers.
enum Script {
    Text(&'static str),
    Unsupported,
    Provider,
}

/// A scriptable [`OcrEngine`]: supports `image/*`, counts calls, answers per
/// its [`Script`].
struct FakeOcr {
    script: Script,
    calls: AtomicUsize,
}

impl FakeOcr {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl OcrEngine for FakeOcr {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn supports(&self, content_type: &str) -> bool {
        content_type.starts_with("image/")
    }
    async fn ocr(&self, _request: OcrRequest) -> CoreResult<OcrResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.script {
            Script::Text(t) => Ok(OcrResponse {
                text: t.to_string(),
                engine: "fake".to_string(),
            }),
            Script::Unsupported => Err(Error::Unsupported("no image input".into())),
            Script::Provider => Err(Error::Provider("upstream 502".into())),
        }
    }
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

/// Re-open the bucket dir as a fresh backend handle (cheap; the backend is a
/// thin path wrapper) so each context owns its `Arc<dyn StorageBackend>`.
fn backend_reopen(dir: &tempfile::TempDir) -> Arc<dyn StorageBackend> {
    Arc::new(LocalFsBackend::new(dir.path().to_path_buf()))
}

#[tokio::test]
async fn object_ocr_extracts_skips_and_retries_per_contract() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping object_ocr_extracts_skips_and_retries_per_contract: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ocri", &format!("ocri-{}", uuid::Uuid::new_v4()))
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

    // A fake PNG at the workspace-namespaced physical key (SOUL §18).
    let png: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    put_file(
        &backend,
        &workspace_object_key(ws.id, "img/scan.png"),
        png,
        "image/png",
    )
    .await;
    let now = chrono::Utc::now();
    let obj = store
        .objects()
        .upsert(&catalerum_store::UpsertObject {
            workspace_id: ws.id,
            bucket_id: bucket.id,
            key: "img/scan.png",
            size: png.len() as u64,
            content_type: Some("image/png"),
            etag: Some("o1"),
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("catalogue image object");
    let source = SourceRef::Object { id: obj.id };

    // --- OCR succeeds: the image catalogues a document, like a text file ------
    const TEXT: &str = "hello from the scanner";
    let engine = FakeOcr::new(Script::Text(TEXT));
    let ctx =
        ObjectIngestContext::single(backend_reopen(&dir)).with_ocr(OcrContext::new(engine.clone()));
    let report = ctx
        .ingest(&store, None, ws.id, obj.id)
        .await
        .expect("ingest OCRs the image");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.text_bytes, TEXT.len());
    let doc = store
        .documents()
        .get_by_source(ws.id, &source)
        .await
        .expect("doc query")
        .expect("OCR text catalogued as a document");
    assert_eq!(doc.text, TEXT);
    assert_eq!(
        store
            .objects()
            .get(ws.id, obj.id)
            .await
            .unwrap()
            .extracted_text_id,
        Some(doc.id),
        "object links its OCR'd document"
    );

    // --- Permanent rejection (Unsupported/Invalid): clean skip, job succeeds --
    // Must NOT burn worker retries — and, reconciling to "no text", must purge
    // the prior projection.
    let engine = FakeOcr::new(Script::Unsupported);
    let ctx =
        ObjectIngestContext::single(backend_reopen(&dir)).with_ocr(OcrContext::new(engine.clone()));
    let report = ctx
        .ingest(&store, None, ws.id, obj.id)
        .await
        .expect("a permanent rejection is a clean skip, not a job failure");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.document_id, None);
    assert!(
        store
            .documents()
            .get_by_source(ws.id, &source)
            .await
            .unwrap()
            .is_none(),
        "the prior OCR document was purged on rejection"
    );

    // --- Transient error (Provider): the job fails → worker retry/backoff -----
    let engine = FakeOcr::new(Script::Provider);
    let ctx =
        ObjectIngestContext::single(backend_reopen(&dir)).with_ocr(OcrContext::new(engine.clone()));
    ctx.ingest(&store, None, ws.id, obj.id)
        .await
        .expect_err("a transient provider error must propagate for retry");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);

    // --- Oversized image: skipped, the engine is never called -----------------
    let engine = FakeOcr::new(Script::Text("never seen"));
    let ctx = ObjectIngestContext::single(backend_reopen(&dir))
        .with_ocr(OcrContext::new(engine.clone()).with_limits(4, 8));
    let report = ctx
        .ingest(&store, None, ws.id, obj.id)
        .await
        .expect("an oversized image skips cleanly");
    assert_eq!(
        engine.calls.load(Ordering::SeqCst),
        0,
        "skip-never-truncate: the engine never sees an oversized document"
    );
    assert_eq!(report.document_id, None);

    // --- A text-free image catalogues no document ------------------------------
    let engine = FakeOcr::new(Script::Text("   \n  "));
    let ctx =
        ObjectIngestContext::single(backend_reopen(&dir)).with_ocr(OcrContext::new(engine.clone()));
    let report = ctx
        .ingest(&store, None, ws.id, obj.id)
        .await
        .expect("a text-free image skips cleanly");
    assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
    assert_eq!(report.document_id, None);

    // --- OCR context removed: re-ingest reconciles the projection away --------
    // First re-establish a document…
    let engine = FakeOcr::new(Script::Text(TEXT));
    let ctx =
        ObjectIngestContext::single(backend_reopen(&dir)).with_ocr(OcrContext::new(engine.clone()));
    ctx.ingest(&store, None, ws.id, obj.id)
        .await
        .expect("re-ingest with OCR");
    assert!(store
        .documents()
        .get_by_source(ws.id, &source)
        .await
        .unwrap()
        .is_some());
    // …then ingest without OCR: the binary-skip path must purge it (the same
    // text→binary reconcile contract as before OCR existed).
    let ctx = ObjectIngestContext::single(backend_reopen(&dir));
    let report = ctx
        .ingest(&store, None, ws.id, obj.id)
        .await
        .expect("ingest without OCR context");
    assert_eq!(report.document_id, None);
    assert!(
        store
            .documents()
            .get_by_source(ws.id, &source)
            .await
            .unwrap()
            .is_none(),
        "unconfiguring OCR purges the stale projection on the next ingest"
    );
    assert_eq!(
        store
            .objects()
            .get(ws.id, obj.id)
            .await
            .unwrap()
            .extracted_text_id,
        None
    );
}
