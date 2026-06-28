//! Integration test: end-to-end note → documents/chunks → embed → Qdrant ingest
//! (SOUL §6.4/§10).
//!
//! Proves the M4 ingest contract: `ingest_note` upserts one stable document,
//! replaces the note's chunk set in Postgres (each carrying its Qdrant point
//! id), and upserts one vector per chunk — and that the whole thing is
//! **idempotent**: re-ingesting is a no-op, and editing the note down to fewer
//! chunks deletes the orphaned vectors (no drift, SOUL §3.4). Also checks
//! per-workspace isolation (§18).
//!
//! Requires BOTH a Postgres and a Qdrant. Set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) and `CATALERUM_TEST_QDRANT_URL` (or `QDRANT_URL`); with either
//! unset the test prints a skip note and passes, so `cargo test -p
//! catalerum-ingest` stays green offline. The embedder is a deterministic
//! in-test fake — no llmleaf needed, so the assertions are stable.

mod common;

use std::sync::Arc;
use std::time::Duration;

use catalerum_core::embed::{Embedding, EmbeddingRequest, EmbeddingResponse};
use catalerum_core::model::{Author, MemoryScope};
use catalerum_core::provider::Embedder;
use catalerum_core::{Result as CoreResult, SourceRef, UserId};
use catalerum_ingest::{
    enqueue_ingest_memory, enqueue_ingest_note, ingest_note, ChunkConfig, EmbedContext,
    IngestConfig, SyncWorker,
};
use catalerum_store::{JobStatus, Store};
use catalerum_vector::{SearchFilter, SearchQuery, VectorStore};

const DIM: u64 = 8;

/// A deterministic, provider-free [`Embedder`]: identical text → identical
/// `DIM`-wide vector, different text → (almost surely) different vector. No
/// network, so the test is stable and offline-capable.
struct FakeEmbedder;

fn fake_vector(text: &str) -> Vec<f32> {
    let seed: u64 = text.bytes().fold(1469598103934665603u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(1099511628211)
    });
    (0..DIM)
        .map(|i| (((seed >> (i * 4)) & 0xF) as f32) + 1.0) // always >= 1, non-degenerate
        .collect()
}

#[async_trait::async_trait]
impl Embedder for FakeEmbedder {
    async fn embed(&self, request: EmbeddingRequest) -> CoreResult<EmbeddingResponse> {
        let embeddings = request
            .input
            .iter()
            .enumerate()
            .map(|(i, text)| Embedding {
                index: i as u32,
                vector: fake_vector(text),
            })
            .collect();
        Ok(EmbeddingResponse {
            model: request.model,
            embeddings,
            usage: None,
        })
    }
}

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn test_qdrant_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_QDRANT_URL")
        .or_else(|_| std::env::var("QDRANT_URL"))
        .ok()
}

#[tokio::test]
async fn note_ingest_round_trips_and_is_idempotent_and_isolated() {
    let (Some(db), Some(qdrant)) = (test_db_url(), test_qdrant_url()) else {
        eprintln!(
            "skipping note_ingest_round_trips_and_is_idempotent_and_isolated: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and \
             CATALERUM_TEST_QDRANT_URL/QDRANT_URL to run it"
        );
        return;
    };

    let store = common::isolated_store(&db).await;
    let vector = VectorStore::new(&qdrant).expect("qdrant client");
    let embedder = FakeEmbedder;
    // Small chunks so the multi-paragraph note splits into several. The vector
    // width is discovered from the (fake) embedder — DIM — not configured.
    let cfg = IngestConfig::new("fake").with_chunk(ChunkConfig::sized(40));

    let ws = store
        .workspaces()
        .create("ingest", &format!("ingest-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let ws_other = store
        .workspaces()
        .create("other", &format!("other-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other workspace");

    // Clean any prior collections for these (random) workspaces.
    let _ = vector.delete_collection(ws.id).await;
    let _ = vector.delete_collection(ws_other.id).await;

    // A note whose title + body span several 40-char chunks.
    let note = store
        .notes()
        .create(
            ws.id,
            Author::User { id: UserId::new() },
            "Quarterly planning",
            "First we align on the roadmap themes.\n\n\
             Then each team commits to two measurable goals.\n\n\
             Finally we schedule the mid-quarter review.",
            &["planning".to_string()],
        )
        .await
        .expect("create note");

    // --- first ingest -------------------------------------------------------
    let report = ingest_note(&store, &embedder, &vector, &cfg, ws.id, note.id)
        .await
        .expect("ingest note");
    assert!(
        report.chunks >= 2,
        "multi-paragraph note splits: {report:?}"
    );
    let doc_id = report
        .document_id
        .expect("a present note projects to a document");

    // Chunks persisted in Postgres, each with a point id, dense 0-based ordinals.
    let chunks = store
        .chunks()
        .list_by_document(ws.id, doc_id)
        .await
        .expect("list chunks");
    assert_eq!(chunks.len(), report.chunks);
    for (i, c) in chunks.iter().enumerate() {
        assert_eq!(c.ordinal, i as i32, "dense ordinals");
        assert!(c.qdrant_point_id.is_some(), "chunk carries its point id");
        assert_eq!(c.document_id, doc_id);
    }

    // One Qdrant vector per chunk.
    let count = vector
        .count(ws.id, &SearchFilter::default())
        .await
        .expect("count");
    assert_eq!(count, report.chunks as u64, "one vector per chunk");

    // A search by a chunk's own embedding returns that chunk, traced to the note.
    let probe = fake_vector(&chunks[0].text);
    let hits = vector
        .search(ws.id, &SearchQuery::new(probe, 1))
        .await
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].payload.text, chunks[0].text);
    assert_eq!(hits[0].payload.source, SourceRef::Note { id: note.id });

    // --- idempotent re-ingest: same counts, no duplicates -------------------
    let report2 = ingest_note(&store, &embedder, &vector, &cfg, ws.id, note.id)
        .await
        .expect("re-ingest");
    assert_eq!(
        report2.document_id, report.document_id,
        "stable document id"
    );
    assert_eq!(report2.chunks, report.chunks);
    assert_eq!(
        vector.count(ws.id, &SearchFilter::default()).await.unwrap(),
        report.chunks as u64,
        "re-ingest does not duplicate vectors"
    );

    // --- edit down to one short chunk: orphan vectors are cleaned -----------
    store
        .notes()
        .update(ws.id, note.id, "Done", "Shipped.", &[])
        .await
        .expect("shrink note");
    let report3 = ingest_note(&store, &embedder, &vector, &cfg, ws.id, note.id)
        .await
        .expect("re-ingest shrunk");
    assert_eq!(report3.chunks, 1, "short note → one chunk");
    assert_eq!(
        store
            .chunks()
            .list_by_document(ws.id, doc_id)
            .await
            .unwrap()
            .len(),
        1,
        "chunk set replaced wholesale"
    );
    assert_eq!(
        vector.count(ws.id, &SearchFilter::default()).await.unwrap(),
        1,
        "orphaned vectors from the larger note are gone"
    );

    // --- per-workspace isolation -------------------------------------------
    assert_eq!(
        vector
            .count(ws_other.id, &SearchFilter::default())
            .await
            .unwrap(),
        0,
        "another workspace sees none of this note's vectors"
    );
    assert_eq!(
        store
            .chunks()
            .count_by_workspace(ws_other.id)
            .await
            .unwrap(),
        0
    );

    // --- cleanup ------------------------------------------------------------
    let _ = vector.delete_collection(ws.id).await;
    let _ = vector.delete_collection(ws_other.id).await;
}

#[tokio::test]
async fn worker_dispatches_ingest_note_job_to_the_embed_context() {
    let (Some(db), Some(qdrant)) = (test_db_url(), test_qdrant_url()) else {
        eprintln!(
            "skipping worker_dispatches_ingest_note_job_to_the_embed_context: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and \
             CATALERUM_TEST_QDRANT_URL/QDRANT_URL to run it"
        );
        return;
    };

    let store = common::isolated_store(&db).await;
    let vector = VectorStore::new(&qdrant).expect("qdrant");
    let ws = store
        .workspaces()
        .create("wd", &format!("wd-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let _ = vector.delete_collection(ws.id).await;

    let note = store
        .notes()
        .create(
            ws.id,
            Author::User { id: UserId::new() },
            "Standup",
            "We shipped the ingest worker.\n\nNext up is the search tool.",
            &[],
        )
        .await
        .expect("note");

    // A worker WITH an embed context (the binary's wiring, here with a fake
    // embedder + live Qdrant) — so it dispatches `ingest_note` jobs (§6.4/§10).
    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder);
    let ctx = EmbedContext::new(embedder, vector.clone(), IngestConfig::new("fake"));
    let worker = SyncWorker::new(store.clone()).with_embed_context(ctx);

    // Enqueue an `ingest_note` job exactly as the note-write path will, then let
    // the worker drain it. Scope the wait to our own job id (shared queue).
    let job_id = enqueue_ingest_note(&store, ws.id, note.id)
        .await
        .expect("enqueue ingest_note");

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
    let (status, last_error) = terminal.expect("ingest_note job observed terminal");
    assert_eq!(
        status,
        JobStatus::Done,
        "the worker dispatched ingest_note to the embed context; last_error = {last_error:?}"
    );

    // The note's chunks landed in Qdrant via the worker path.
    let count = vector
        .count(ws.id, &SearchFilter::default())
        .await
        .expect("count");
    assert!(count >= 1, "the worker embedded the note's chunks");
    // And persisted in Postgres.
    let doc = store
        .documents()
        .get_by_source(ws.id, &SourceRef::Note { id: note.id })
        .await
        .expect("doc")
        .expect("document exists");
    assert!(
        !store
            .chunks()
            .list_by_document(ws.id, doc.id)
            .await
            .unwrap()
            .is_empty(),
        "chunks persisted"
    );

    // --- delete reconciles: a deleted note's projection is purged ------------
    store
        .notes()
        .delete(ws.id, note.id)
        .await
        .expect("delete note");
    // The note-write path enqueues an ingest_note on delete too; the worker
    // finds it gone and purges the vectors + document + chunks (SOUL §3.1/§10).
    let del_job = enqueue_ingest_note(&store, ws.id, note.id)
        .await
        .expect("enqueue delete reconcile");
    let mut del_terminal = None;
    for _ in 0..40 {
        let row = store.job_queue().get(del_job).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            del_terminal = Some(row.status().unwrap());
            break;
        }
        if !worker.poll_once().await.expect("poll_once") {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    assert_eq!(
        del_terminal,
        Some(JobStatus::Done),
        "purge job runs to completion"
    );
    assert_eq!(
        vector.count(ws.id, &SearchFilter::default()).await.unwrap(),
        0,
        "the deleted note's vectors are purged"
    );
    assert!(
        store
            .documents()
            .get_by_source(ws.id, &SourceRef::Note { id: note.id })
            .await
            .unwrap()
            .is_none(),
        "the deleted note's document (and cascaded chunks) are gone"
    );

    let _ = vector.delete_collection(ws.id).await;
}

#[tokio::test]
async fn worker_dispatches_ingest_memory_job_and_purges_on_delete() {
    let (Some(db), Some(qdrant)) = (test_db_url(), test_qdrant_url()) else {
        eprintln!("skipping ingest_memory worker test: set DB + QDRANT urls");
        return;
    };
    let store = common::isolated_store(&db).await;
    let vector = VectorStore::new(&qdrant).expect("qdrant");
    let ws = store
        .workspaces()
        .create("mem", &format!("mem-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let _ = vector.delete_collection(ws.id).await;

    let memory = store
        .memories()
        .create(
            ws.id,
            MemoryScope::User,
            Some(UserId::new()),
            "prefers morning standups",
            None,
        )
        .await
        .expect("memory");

    let embedder: Arc<dyn Embedder> = Arc::new(FakeEmbedder);
    let ctx = EmbedContext::new(embedder, vector.clone(), IngestConfig::new("fake"));
    let worker = SyncWorker::new(store.clone()).with_embed_context(ctx);

    // Enqueue + drain the ingest_memory job.
    let job = enqueue_ingest_memory(&store, ws.id, memory.id)
        .await
        .expect("enqueue");
    drain_job(&store, &worker, job).await;
    // The memory's chunk landed in Qdrant, traced to SourceRef::Memory.
    assert_eq!(
        vector.count(ws.id, &SearchFilter::default()).await.unwrap(),
        1
    );
    let probe = fake_vector("prefers morning standups");
    let hits = vector
        .search(ws.id, &SearchQuery::new(probe, 3))
        .await
        .unwrap();
    assert_eq!(hits[0].payload.source, SourceRef::Memory { id: memory.id });

    // Delete the memory, re-enqueue → the worker purges its vectors.
    store.memories().delete(ws.id, memory.id).await.unwrap();
    let job2 = enqueue_ingest_memory(&store, ws.id, memory.id)
        .await
        .unwrap();
    drain_job(&store, &worker, job2).await;
    assert_eq!(
        vector.count(ws.id, &SearchFilter::default()).await.unwrap(),
        0
    );

    let _ = vector.delete_collection(ws.id).await;
}

/// Drive `worker.poll_once()` until `job` is terminal (scoped to the job id).
async fn drain_job(store: &Store, worker: &SyncWorker, job: uuid::Uuid) {
    for _ in 0..40 {
        let row = store.job_queue().get(job).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            assert_eq!(
                row.status().unwrap(),
                JobStatus::Done,
                "last_error={:?}",
                row.last_error
            );
            return;
        }
        if !worker.poll_once().await.expect("poll") {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    panic!("job {job} did not reach terminal state");
}
