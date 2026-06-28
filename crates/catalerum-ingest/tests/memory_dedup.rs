//! Integration test: the memory-store dedup seam's **embedding-similarity** layer
//! (SOUL §22/§29).
//!
//! The pure heuristic (exact / whole-word-superset) layer is unit-tested in
//! `dedup.rs`; this proves the end-to-end similarity path: with a candidate that
//! is *not* a textual duplicate of a stored memory but embeds to (nearly) the same
//! vector, the seam still recognises the duplicate — while a genuinely unrelated
//! fact is stored (the high threshold must never swallow new facts).
//!
//! Requires BOTH a Postgres and a Qdrant. Set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) and `CATALERUM_TEST_QDRANT_URL` (or `QDRANT_URL`); with either
//! unset the test prints a skip note and passes. The embedder is a deterministic
//! in-test fake that groups near-paraphrases onto one vector, so assertions are
//! stable and offline-capable.

mod common;

use std::sync::Arc;

use catalerum_core::embed::{Embedding, EmbeddingRequest, EmbeddingResponse};
use catalerum_core::model::MemoryScope;
use catalerum_core::provider::Embedder;
use catalerum_core::Result as CoreResult;
use catalerum_ingest::{
    ingest_memory, store_memory_deduped, IngestConfig, MemoryDedupIndex, MemoryStoreStatus,
};
use catalerum_vector::VectorStore;

const DIM: u64 = 8;

/// A deterministic, provider-free [`Embedder`] that maps every text to a one-hot
/// `DIM`-vector chosen by a coarse **topic** — so two near-paraphrases sharing a
/// topic embed to the *same* vector (cosine 1.0), while different topics are
/// orthogonal (cosine 0.0, far below the dedup threshold). This isolates the
/// similarity layer: the paired texts are not textual duplicates/supersets, so
/// only the embedding match can catch them.
struct TopicEmbedder;

fn topic_slot(text: &str) -> usize {
    let t = text.to_lowercase();
    if t.contains("espresso") {
        1
    } else if t.contains("berlin") {
        2
    } else if t.contains("guitar") {
        3
    } else {
        // Anything else → its own slot from a cheap hash, so unrelated fixtures
        // don't collide by accident.
        (text.bytes().fold(0u64, |h, b| h.wrapping_add(u64::from(b))) as usize % (DIM as usize - 4))
            + 4
    }
}

fn topic_vector(text: &str) -> Vec<f32> {
    let slot = topic_slot(text);
    (0..DIM as usize)
        .map(|i| if i == slot { 1.0 } else { 0.0 })
        .collect()
}

#[async_trait::async_trait]
impl Embedder for TopicEmbedder {
    async fn embed(&self, request: EmbeddingRequest) -> CoreResult<EmbeddingResponse> {
        let embeddings = request
            .input
            .iter()
            .enumerate()
            .map(|(i, text)| Embedding {
                index: i as u32,
                vector: topic_vector(text),
            })
            .collect();
        Ok(EmbeddingResponse {
            model: request.model,
            embeddings,
            usage: None,
        })
    }
}

/// An [`Embedder`] that always fails, standing in for a down/misconfigured
/// provider (e.g. dev llmleaf with no route for the embedding model). The
/// similarity layer is best-effort, so a store through it must still succeed.
struct BrokenEmbedder;

#[async_trait::async_trait]
impl Embedder for BrokenEmbedder {
    async fn embed(&self, _request: EmbeddingRequest) -> CoreResult<EmbeddingResponse> {
        Err(catalerum_core::Error::Provider(
            "no route for model 'fake'".to_string(),
        ))
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
async fn similarity_layer_dedups_near_paraphrases_but_stores_new_facts() {
    let (Some(db), Some(qdrant)) = (test_db_url(), test_qdrant_url()) else {
        eprintln!(
            "skipping similarity_layer_dedups_near_paraphrases_but_stores_new_facts: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and \
             CATALERUM_TEST_QDRANT_URL/QDRANT_URL to run it"
        );
        return;
    };

    let store = common::isolated_store(&db).await;
    let vector = VectorStore::new(&qdrant).expect("qdrant client");
    let embedder: Arc<dyn Embedder> = Arc::new(TopicEmbedder);
    let cfg = IngestConfig::new("fake");

    let ws = store
        .workspaces()
        .create("dedup", &format!("dedup-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let _ = vector.delete_collection(ws.id).await;

    let index = MemoryDedupIndex {
        embedder: &*embedder,
        vector: &vector,
        embed_model: "fake",
    };

    // 1) A first fact is stored (and would be enqueued for embedding).
    let a = store_memory_deduped(
        &store,
        Some(&index),
        ws.id,
        MemoryScope::Workspace,
        None,
        "likes espresso",
        None,
    )
    .await
    .expect("store a");
    assert_eq!(a.status, MemoryStoreStatus::Stored);

    // Embed it into Qdrant now (the seam enqueues a durable job; here we run the
    // embed synchronously so the similarity search below has a vector to find).
    ingest_memory(&store, &*embedder, &vector, &cfg, ws.id, a.memory.id)
        .await
        .expect("embed a");

    // 2) A near-paraphrase — NOT a textual duplicate/superset, but same topic, so
    //    it embeds to the same vector → the similarity layer dedups it.
    let b = store_memory_deduped(
        &store,
        Some(&index),
        ws.id,
        MemoryScope::Workspace,
        None,
        "enjoys a strong espresso each afternoon",
        None,
    )
    .await
    .expect("store b");
    assert_eq!(
        b.status,
        MemoryStoreStatus::Deduplicated,
        "a near-paraphrase above the similarity threshold must dedup"
    );
    assert_eq!(
        b.memory.id, a.memory.id,
        "dedup returns the existing memory"
    );
    assert_eq!(
        store
            .memories()
            .list_visible(ws.id, None, 50)
            .await
            .unwrap()
            .len(),
        1,
        "the near-duplicate must not add a row"
    );

    // 3) A genuinely unrelated fact embeds to an orthogonal vector (cosine 0 << the
    //    threshold) → it is stored, proving the high threshold never swallows new
    //    facts.
    let c = store_memory_deduped(
        &store,
        Some(&index),
        ws.id,
        MemoryScope::Workspace,
        None,
        "office is in berlin",
        None,
    )
    .await
    .expect("store c");
    assert_eq!(
        c.status,
        MemoryStoreStatus::Stored,
        "an unrelated fact must be stored, not deduped"
    );
    assert_eq!(
        store
            .memories()
            .list_visible(ws.id, None, 50)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn similarity_layer_failure_degrades_to_store_not_error() {
    let (Some(db), Some(qdrant)) = (test_db_url(), test_qdrant_url()) else {
        eprintln!(
            "skipping similarity_layer_failure_degrades_to_store_not_error: \
             set CATALERUM_TEST_DATABASE_URL/DATABASE_URL and \
             CATALERUM_TEST_QDRANT_URL/QDRANT_URL to run it"
        );
        return;
    };

    let store = common::isolated_store(&db).await;
    let vector = VectorStore::new(&qdrant).expect("qdrant client");
    let embedder: Arc<dyn Embedder> = Arc::new(BrokenEmbedder);

    let ws = store
        .workspaces()
        .create(
            "dedup-broken",
            &format!("dedup-broken-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("workspace");

    let index = MemoryDedupIndex {
        embedder: &*embedder,
        vector: &vector,
        embed_model: "fake",
    };

    // The embedder errors on every call, but the similarity layer is best-effort:
    // the fact must be stored (a dropped new fact is worse than a near-dup).
    let a = store_memory_deduped(
        &store,
        Some(&index),
        ws.id,
        MemoryScope::Workspace,
        None,
        "prefers oat milk",
        None,
    )
    .await
    .expect("a broken embedder must not block the store");
    assert_eq!(a.status, MemoryStoreStatus::Stored);

    // The heuristic layer still runs without the embedder: an exact re-remember
    // dedups against the row stored above.
    let b = store_memory_deduped(
        &store,
        Some(&index),
        ws.id,
        MemoryScope::Workspace,
        None,
        "Prefers  OAT milk",
        None,
    )
    .await
    .expect("heuristic dedup works without the embedder");
    assert_eq!(b.status, MemoryStoreStatus::Deduplicated);
    assert_eq!(b.memory.id, a.memory.id);
}
