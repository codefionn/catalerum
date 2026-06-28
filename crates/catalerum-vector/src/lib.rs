//! catalerum-vector — Qdrant derived vector index: per-workspace collections,
//! embeddings upsert, and filtered ANN search. Rebuildable from Postgres
//! `chunks`/`memories`/`notes` (SOUL §6.4).
//!
//! This crate is a thin async REST client over Qdrant. It owns nothing durable:
//! every point traces back to a Postgres-truth row via a
//! [`SourceRef`](catalerum_core::SourceRef), so a cold or wiped index costs a
//! re-embed and never data (principle 1, §3.1).
//!
//! # Tenancy
//! Each workspace gets its **own collection** (`catalerum_ws_<uuid>`), and every
//! point *also* carries a `workspace_id` payload that searches **always** filter
//! on. Per-workspace collections satisfy the §18 partitioning invariant; the
//! redundant payload filter is defense-in-depth and keeps a future switch to a
//! single shared collection (the open question in §29) a cheap migration rather
//! than a rewrite.
//!
//! # Shape
//! - [`VectorStore`] — connect, [`ensure_collection`](VectorStore::ensure_collection)
//!   (idempotent, width-checked), [`upsert`](VectorStore::upsert),
//!   [`search`](VectorStore::search) (filtered ANN),
//!   [`count`](VectorStore::count), [`delete_points`](VectorStore::delete_points),
//!   [`delete_by_source`](VectorStore::delete_by_source) (re-projection),
//!   [`delete_collection`](VectorStore::delete_collection) (full rebuild).
//! - [`VectorPoint`] / [`PointPayload`] — what you upsert; payload carries source
//!   ref, kind, entity ids, and a timestamp so search is filterable (§6.4).
//! - [`SearchQuery`] / [`SearchFilter`] — vector + limit + narrowing (kinds,
//!   entities, time window) + optional score threshold.
//!
//! ```no_run
//! # async fn demo(ws: catalerum_core::WorkspaceId, src: catalerum_core::SourceRef)
//! #   -> catalerum_vector::Result<()> {
//! use catalerum_vector::{PointPayload, SearchQuery, VectorPoint, VectorStore};
//!
//! let store = VectorStore::new("http://localhost:6333")?;
//! store.ensure_collection(ws, 1536).await?;          // width = embedding dim
//! let point = VectorPoint::new(vec![0.0; 1536], PointPayload::new(ws, src, "hello"));
//! store.upsert(ws, &[point]).await?;
//! let hits = store.search(ws, &SearchQuery::new(vec![0.0; 1536], 5)).await?;
//! # let _ = hits; Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod error;
pub mod payload;
pub mod store;

pub use error::{Result, VectorError};
pub use payload::{
    source_id_string, source_kind, Distance, PointPayload, ScoredPoint, SearchFilter, SearchQuery,
    VectorPoint,
};
pub use store::VectorStore;

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{EntityId, NoteId, SourceRef, WorkspaceId};
    use chrono::{DateTime, Utc};

    fn ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::from_u128(1))
    }

    #[test]
    fn collection_name_is_stable_and_url_safe() {
        let name = VectorStore::collection_name(ws());
        assert_eq!(name, "catalerum_ws_00000000000000000000000000000001");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn distance_maps_to_qdrant_strings() {
        assert_eq!(Distance::default().as_qdrant(), "Cosine");
        assert_eq!(Distance::Dot.as_qdrant(), "Dot");
        assert_eq!(Distance::Euclid.as_qdrant(), "Euclid");
    }

    #[test]
    fn source_kind_and_id_cover_every_variant() {
        let note = NoteId::new();
        let cases = [
            (SourceRef::Note { id: note }, "note", note.to_string()),
            (
                SourceRef::External {
                    uri: "https://x".into(),
                },
                "external",
                "https://x".to_owned(),
            ),
        ];
        for (src, kind, id) in cases {
            assert_eq!(source_kind(&src), kind);
            assert_eq!(source_id_string(&src), id);
        }
    }

    #[test]
    fn payload_round_trips_through_qdrant_json() {
        let src = SourceRef::Note { id: NoteId::new() };
        let ts: DateTime<Utc> = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let payload = PointPayload::new(ws(), src, "the quick brown fox")
            .with_entities(vec![EntityId::new(), EntityId::new()])
            .with_created_at(ts);

        let json = payload.to_qdrant();
        // Denormalized flat fields for cheap filtering.
        assert_eq!(json["kind"], "note");
        assert_eq!(json["workspace_id"], ws().to_string());
        assert_eq!(json["created_at"], 1_700_000_000);
        assert_eq!(json["entity_ids"].as_array().unwrap().len(), 2);

        let back = PointPayload::from_qdrant(&json).expect("round-trip");
        assert_eq!(back, payload);
    }

    #[test]
    fn payload_without_timestamp_omits_field() {
        let src = SourceRef::Note { id: NoteId::new() };
        let payload = PointPayload::new(ws(), src, "no ts");
        let json = payload.to_qdrant();
        assert!(json.get("created_at").is_none());
        assert_eq!(PointPayload::from_qdrant(&json).unwrap(), payload);
    }

    #[test]
    fn empty_filter_is_just_the_workspace_match() {
        let f = SearchFilter::default();
        assert!(f.is_empty());
        let q = f.to_qdrant(ws());
        let must = q["must"].as_array().unwrap();
        assert_eq!(must.len(), 1);
        assert_eq!(must[0]["key"], "workspace_id");
        assert_eq!(must[0]["match"]["value"], ws().to_string());
    }

    #[test]
    fn full_filter_builds_all_clauses_with_workspace_always_first() {
        let entity = EntityId::new();
        let after: DateTime<Utc> = DateTime::from_timestamp(100, 0).unwrap();
        let before: DateTime<Utc> = DateTime::from_timestamp(200, 0).unwrap();
        let f = SearchFilter {
            kinds: vec!["note".into(), "memory".into()],
            entity_ids: vec![entity],
            bucket_name: Some("wiki".into()),
            key_prefix: Some("acme".into()),
            created_after: Some(after),
            created_before: Some(before),
        };
        assert!(!f.is_empty());
        let q = f.to_qdrant(ws());
        let must = q["must"].as_array().unwrap();

        // workspace is always the first clause (never droppable).
        assert_eq!(must[0]["key"], "workspace_id");
        let keys: Vec<&str> = must.iter().map(|c| c["key"].as_str().unwrap()).collect();
        assert!(keys.contains(&"kind"));
        assert!(keys.contains(&"entity_ids"));
        assert!(keys.contains(&"created_at"));
        assert!(keys.contains(&"bucket_name"));
        assert!(keys.contains(&"key_prefixes"));

        let kind_clause = must.iter().find(|c| c["key"] == "kind").unwrap();
        assert_eq!(kind_clause["match"]["any"].as_array().unwrap().len(), 2);

        let range = must.iter().find(|c| c["key"] == "created_at").unwrap();
        assert_eq!(range["range"]["gte"], 100);
        assert_eq!(range["range"]["lte"], 200);

        // The subdir prefix is normalized to a directory boundary + matched
        // exactly against the denormalized ancestor-prefix array.
        let prefix_clause = must.iter().find(|c| c["key"] == "key_prefixes").unwrap();
        assert_eq!(prefix_clause["match"]["value"], "acme/");
    }

    #[test]
    fn search_query_builder_sets_filter_and_threshold() {
        let q = SearchQuery::new(vec![0.1, 0.2], 7)
            .with_score_threshold(0.5)
            .with_filter(SearchFilter {
                kinds: vec!["note".into()],
                ..Default::default()
            });
        assert_eq!(q.limit, 7);
        assert_eq!(q.score_threshold, Some(0.5));
        assert_eq!(q.filter.kinds, vec!["note".to_string()]);
    }

    #[test]
    fn vector_point_to_qdrant_carries_id_vector_payload() {
        let id = uuid::Uuid::from_u128(42);
        let src = SourceRef::Note { id: NoteId::new() };
        let p = VectorPoint::with_id(id, vec![1.0, 2.0], PointPayload::new(ws(), src, "x"));
        let json = p.to_qdrant();
        assert_eq!(json["id"], id.to_string());
        assert_eq!(json["vector"], serde_json::json!([1.0, 2.0]));
        assert_eq!(json["payload"]["text"], "x");
    }

    #[test]
    fn store_rejects_bad_base_url() {
        assert!(VectorStore::new("not a url").is_err());
        assert!(VectorStore::new("http://localhost:6333").is_ok());
    }
}
