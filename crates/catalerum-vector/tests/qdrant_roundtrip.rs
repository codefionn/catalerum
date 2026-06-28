//! Integration test: the `VectorStore` contract against a live Qdrant
//! (SOUL §6.4, §18). Ensure-collection (idempotent + width-checked), upsert,
//! filtered ANN search (kind / entity / time window), source-scoped + id-scoped
//! deletion, count, full-collection rebuild, and per-workspace isolation.
//!
//! Same gating as the store/ingest integration tests: set
//! `CATALERUM_TEST_QDRANT_URL` (or `QDRANT_URL`) to run it; otherwise it skips
//! and passes so the suite stays green offline.

use catalerum_core::{EntityId, MemoryId, NoteId, SourceRef, WorkspaceId};
use catalerum_vector::{
    PointPayload, SearchFilter, SearchQuery, VectorError, VectorPoint, VectorStore,
};
use chrono::DateTime;

fn test_qdrant_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_QDRANT_URL")
        .or_else(|_| std::env::var("QDRANT_URL"))
        .ok()
}

const DIM: u64 = 4;

#[tokio::test]
async fn vector_store_round_trips_and_is_workspace_isolated() {
    let Some(url) = test_qdrant_url() else {
        eprintln!(
            "skipping vector_store_round_trips_and_is_workspace_isolated: \
             set CATALERUM_TEST_QDRANT_URL or QDRANT_URL to run it"
        );
        return;
    };

    let store = VectorStore::new(&url).expect("valid url");
    store.healthz().await.expect("qdrant healthy");

    // Fresh, unique workspaces so reruns never collide.
    let ws_a = WorkspaceId::new();
    let ws_b = WorkspaceId::new();

    // --- ensure_collection: create, then idempotent re-ensure ---------------
    store.ensure_collection(ws_a, DIM).await.expect("create a");
    store
        .ensure_collection(ws_a, DIM)
        .await
        .expect("re-ensure a is a no-op");
    assert_eq!(store.collection_dim(ws_a).await.unwrap(), Some(DIM));

    // Re-ensuring at a different width must refuse (would drop data).
    match store.ensure_collection(ws_a, DIM + 1).await {
        Err(VectorError::DimensionMismatch {
            found, expected, ..
        }) => {
            assert_eq!(found, DIM);
            assert_eq!(expected, DIM + 1);
        }
        other => panic!("expected DimensionMismatch, got {other:?}"),
    }

    // --- upsert -------------------------------------------------------------
    let entity_1 = EntityId::new();
    let entity_2 = EntityId::new();
    let t_old = DateTime::from_timestamp(1_000, 0).unwrap();
    let t_new = DateTime::from_timestamp(9_000, 0).unwrap();

    let note_1 = SourceRef::Note { id: NoteId::new() };
    let note_2 = SourceRef::Note { id: NoteId::new() };
    let mem = SourceRef::Memory {
        id: MemoryId::new(),
    };

    let p1 = VectorPoint::new(
        vec![1.0, 0.0, 0.0, 0.0],
        PointPayload::new(ws_a, note_1.clone(), "alpha about entity one")
            .with_entities(vec![entity_1])
            .with_created_at(t_new),
    );
    let p2 = VectorPoint::new(
        vec![0.0, 1.0, 0.0, 0.0],
        PointPayload::new(ws_a, note_2.clone(), "beta about entity two")
            .with_entities(vec![entity_2])
            .with_created_at(t_new),
    );
    let p3 = VectorPoint::new(
        vec![0.0, 0.0, 1.0, 0.0],
        PointPayload::new(ws_a, mem.clone(), "gamma memory").with_created_at(t_old),
    );
    store
        .upsert(ws_a, &[p1.clone(), p2.clone(), p3.clone()])
        .await
        .expect("upsert a");

    // Empty upsert is a no-op (does not error).
    store.upsert(ws_a, &[]).await.expect("empty upsert");

    // --- count --------------------------------------------------------------
    assert_eq!(
        store.count(ws_a, &SearchFilter::default()).await.unwrap(),
        3
    );

    // --- search: nearest neighbour ------------------------------------------
    let hits = store
        .search(ws_a, &SearchQuery::new(vec![1.0, 0.0, 0.0, 0.0], 3))
        .await
        .expect("search a");
    assert_eq!(hits.len(), 3);
    assert_eq!(hits[0].id, p1.id, "closest to [1,0,0,0] is p1");
    assert_eq!(hits[0].payload.text, "alpha about entity one");
    assert_eq!(hits[0].payload.workspace_id, ws_a);

    // --- search: kind filter ------------------------------------------------
    let mem_only = store
        .search(
            ws_a,
            &SearchQuery::new(vec![0.0, 0.0, 1.0, 0.0], 5).with_filter(SearchFilter {
                kinds: vec!["memory".into()],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(mem_only.len(), 1);
    assert_eq!(mem_only[0].id, p3.id);

    // --- search: entity filter ----------------------------------------------
    let by_entity = store
        .search(
            ws_a,
            &SearchQuery::new(vec![1.0, 0.0, 0.0, 0.0], 5).with_filter(SearchFilter {
                entity_ids: vec![entity_1],
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert_eq!(by_entity.len(), 1);
    assert_eq!(by_entity[0].id, p1.id);

    // --- search: time-window filter (excludes the old memory) ---------------
    let recent = store
        .search(
            ws_a,
            &SearchQuery::new(vec![0.0, 0.0, 1.0, 0.0], 5).with_filter(SearchFilter {
                created_after: Some(DateTime::from_timestamp(5_000, 0).unwrap()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    assert!(
        recent.iter().all(|h| h.id != p3.id),
        "old memory filtered out"
    );

    // --- per-workspace isolation: ws_b is a separate collection -------------
    store.ensure_collection(ws_b, DIM).await.expect("create b");
    store
        .upsert(
            ws_b,
            &[VectorPoint::new(
                vec![1.0, 0.0, 0.0, 0.0],
                PointPayload::new(ws_b, SourceRef::Note { id: NoteId::new() }, "other tenant"),
            )],
        )
        .await
        .expect("upsert b");
    // ws_a is unaffected by ws_b's write, and a ws_a search never sees ws_b.
    assert_eq!(
        store.count(ws_a, &SearchFilter::default()).await.unwrap(),
        3
    );
    let a_hits = store
        .search(ws_a, &SearchQuery::new(vec![1.0, 0.0, 0.0, 0.0], 10))
        .await
        .unwrap();
    assert!(a_hits.iter().all(|h| h.payload.workspace_id == ws_a));

    // --- delete_by_source (re-projection basis) -----------------------------
    store
        .delete_by_source(ws_a, &note_1)
        .await
        .expect("delete by source");
    assert_eq!(
        store.count(ws_a, &SearchFilter::default()).await.unwrap(),
        2
    );
    let after = store
        .search(ws_a, &SearchQuery::new(vec![1.0, 0.0, 0.0, 0.0], 10))
        .await
        .unwrap();
    assert!(after.iter().all(|h| h.id != p1.id), "p1's points are gone");

    // --- delete_points by id ------------------------------------------------
    store
        .delete_points(ws_a, &[p2.id])
        .await
        .expect("delete points");
    assert_eq!(
        store.count(ws_a, &SearchFilter::default()).await.unwrap(),
        1
    );

    // --- delete_collection (full rebuild) -----------------------------------
    store.delete_collection(ws_a).await.expect("drop a");
    assert_eq!(store.collection_dim(ws_a).await.unwrap(), None);
    // Search/count on a missing collection are lenient: empty / zero.
    assert_eq!(
        store.count(ws_a, &SearchFilter::default()).await.unwrap(),
        0
    );
    assert!(store
        .search(ws_a, &SearchQuery::new(vec![1.0, 0.0, 0.0, 0.0], 5))
        .await
        .unwrap()
        .is_empty());

    // Cleanup ws_b.
    store.delete_collection(ws_b).await.expect("drop b");
}
