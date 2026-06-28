//! catalerum-graph — Neo4j derived projection: idempotent `MERGE` writers and
//! parameterized Cypher behind typed helpers. Rebuildable from Postgres
//! (SOUL §6.3).
//!
//! This crate is a thin async client over Neo4j's **HTTP transactional API**
//! (`POST /db/{database}/tx/commit`) — the same shape as
//! [`catalerum-vector`](catalerum_vector)'s Qdrant client, so the projection
//! layer stays dependency-light (no Bolt driver) with one auth/TLS story. The
//! graph is **derived**: every node traces back to a Postgres-truth row, so a
//! cold or wiped graph costs a reprojection and never data (principle 1, §3.1).
//!
//! # Tenancy
//! Every node is keyed on `(workspace_id, id)` and **every** query filters on
//! `workspace_id`, so cross-workspace reach is impossible by construction
//! (SOUL §18).
//!
//! # Injection safety
//! Cypher labels and relationship types can't be parameterized, so both are
//! closed enums ([`NodeLabel`]/[`EdgeType`]) with a fixed `&'static str` table —
//! caller data never reaches a label position. Workspace ids, node ids, and
//! properties all ride as `$parameters`.
//!
//! # Shape
//! - [`GraphStore`] — connect, [`ensure_indexes`](GraphStore::ensure_indexes),
//!   [`project_note`](GraphStore::project_note) /
//!   [`project_entity`](GraphStore::project_entity) (idempotent writers),
//!   [`references_of`](GraphStore::references_of) (typed read),
//!   [`count_nodes`](GraphStore::count_nodes),
//!   [`delete_node`](GraphStore::delete_node) (re-projection basis) /
//!   [`delete_workspace`](GraphStore::delete_workspace) (full rebuild), and the
//!   low-level [`run`](GraphStore::run) for arbitrary [`Statement`]s.
//! - [`NodeLabel`] / [`EdgeType`] / [`NodeRef`] — the §6.3 taxonomy.
//! - [`Statement`] / [`QueryResult`] — what you run and what comes back.
//!
//! ```no_run
//! # async fn demo(note: &catalerum_core::Note, refs: &[catalerum_core::Entity])
//! #   -> catalerum_graph::Result<()> {
//! use catalerum_graph::GraphStore;
//!
//! let graph = GraphStore::new("http://localhost:7474")?.with_auth("neo4j", "catalerum");
//! graph.ensure_indexes().await?;             // one-time, idempotent
//! graph.project_note(note, refs).await?;     // upsert note + entities + REFERENCES edges
//! let entity_ids = graph.references_of(note.workspace_id, note.id).await?;
//! # let _ = entity_ids; Ok(()) }
//! ```

#![forbid(unsafe_code)]

pub mod cypher;
pub mod error;
pub mod model;
pub mod store;

pub use cypher::{QueryResult, Statement};
pub use error::{GraphError, Neo4jError, Result};
pub use model::{EdgeType, NodeLabel, NodeRef, ScopedNode};
pub use store::{
    GraphStore, NoteHit, RelatedNote, WorkspaceFacts, MAX_WORKSPACE_EDGES, MAX_WORKSPACE_NODES,
};

/// Live integration tests against a real Neo4j (compose: `neo4j:5`,
/// `NEO4J_AUTH=neo4j/catalerum`). Gated on `NEO4J_URL` (e.g.
/// `http://localhost:7474`) so the suite **skips and passes** with no server —
/// the same pattern as `catalerum-vector`'s Qdrant test. `NEO4J_USER` /
/// `NEO4J_PASSWORD` default to `neo4j` / `catalerum`.
#[cfg(test)]
mod live {
    use super::*;
    use catalerum_core::{
        Author, Entity, EntityId, EntityKind, EventId, Link, LinkId, Note, NoteId, SourceRef,
        UserId, WorkspaceId,
    };
    use chrono::Utc;

    fn store() -> Option<GraphStore> {
        let url = std::env::var("NEO4J_URL").ok()?;
        let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
        let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "catalerum".into());
        Some(
            GraphStore::new(&url)
                .expect("valid NEO4J_URL")
                .with_auth(user, password),
        )
    }

    fn note(ws: WorkspaceId) -> Note {
        Note {
            id: NoteId::new(),
            workspace_id: ws,
            author: Author::User { id: UserId::new() },
            title: "Project kickoff".into(),
            markdown: "# kickoff".into(),
            tags: vec!["work".into()],
            updated_at: Utc::now(),
        }
    }

    fn entity(ws: WorkspaceId, kind: EntityKind, name: &str) -> Entity {
        Entity {
            id: EntityId::new(),
            workspace_id: ws,
            kind,
            display_name: name.into(),
            aliases: vec![],
        }
    }

    #[tokio::test]
    async fn full_projection_round_trip_and_isolation() {
        let Some(graph) = store() else {
            eprintln!("NEO4J_URL unset; skipping live graph test");
            return;
        };

        graph.healthz().await.expect("neo4j reachable");
        graph.ensure_indexes().await.expect("indexes");

        // Two isolated workspaces (random ids so reruns never collide, §18).
        let ws_a = WorkspaceId::new();
        let ws_b = WorkspaceId::new();
        // Clean slate.
        graph.delete_workspace(ws_a).await.unwrap();
        graph.delete_workspace(ws_b).await.unwrap();

        let n = note(ws_a);
        let ada = entity(ws_a, EntityKind::Person, "Ada");
        let topic = entity(ws_a, EntityKind::Topic, "Scheduling");
        graph
            .project_note(&n, &[ada.clone(), topic.clone()])
            .await
            .expect("project note");

        // Idempotent: re-projecting the same note adds nothing.
        graph
            .project_note(&n, &[ada.clone(), topic.clone()])
            .await
            .unwrap();

        // Note + 2 entities = 3 nodes, no duplicates.
        assert_eq!(graph.count_nodes(ws_a).await.unwrap(), 3);

        // The two REFERENCES targets come back.
        let mut refs = graph.references_of(ws_a, n.id).await.unwrap();
        refs.sort();
        let mut want = vec![ada.id.to_string(), topic.id.to_string()];
        want.sort();
        assert_eq!(refs, want);

        // Cross-workspace isolation: ws_b sees nothing of ws_a (§18).
        assert_eq!(graph.count_nodes(ws_b).await.unwrap(), 0);
        assert!(graph.references_of(ws_b, n.id).await.unwrap().is_empty());

        // Cross-workspace edge is refused before any write.
        let foreign = entity(ws_b, EntityKind::Person, "Grace");
        assert!(graph.project_note(&n, &[foreign]).await.is_err());

        // Single-node delete (re-projection basis) drops the note, keeps entities.
        graph.delete_node(ws_a, &NodeRef::note(n.id)).await.unwrap();
        assert_eq!(graph.count_nodes(ws_a).await.unwrap(), 2);
        assert!(graph.references_of(ws_a, n.id).await.unwrap().is_empty());

        // Full per-workspace rebuild basis: drop everything.
        graph.delete_workspace(ws_a).await.unwrap();
        assert_eq!(graph.count_nodes(ws_a).await.unwrap(), 0);
    }

    fn titled_note(ws: WorkspaceId, title: &str) -> Note {
        Note {
            id: NoteId::new(),
            workspace_id: ws,
            author: Author::User { id: UserId::new() },
            title: title.into(),
            markdown: String::new(),
            tags: vec![],
            updated_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn graph_queries_find_related_notes_and_notes_by_topic() {
        let Some(graph) = store() else {
            eprintln!("NEO4J_URL unset; skipping live graph query test");
            return;
        };
        graph.ensure_indexes().await.expect("indexes");
        let ws = WorkspaceId::new();
        graph.delete_workspace(ws).await.unwrap();

        // Two notes share Topic "Scheduling"; n1 also references "Budget".
        let scheduling = entity(ws, EntityKind::Topic, "Scheduling");
        let budget = entity(ws, EntityKind::Topic, "Budget");
        let n1 = titled_note(ws, "Sprint plan");
        let n2 = titled_note(ws, "Roadmap");
        graph
            .project_note(&n1, &[scheduling.clone(), budget.clone()])
            .await
            .unwrap();
        graph
            .project_note(&n2, std::slice::from_ref(&scheduling))
            .await
            .unwrap();

        // related_notes(n1) → n2, sharing exactly one topic.
        let related = graph.related_notes(ws, n1.id, 10).await.unwrap();
        assert_eq!(related.len(), 1);
        assert_eq!(related[0].note_id, n2.id.to_string());
        assert_eq!(related[0].shared_topics, 1);
        assert_eq!(related[0].title.as_deref(), Some("Roadmap"));

        // notes_by_topic is case-insensitive on display_name.
        let by_topic = graph.notes_by_topic(ws, "scheduling", 10).await.unwrap();
        let ids: std::collections::HashSet<String> =
            by_topic.iter().map(|h| h.note_id.clone()).collect();
        assert_eq!(by_topic.len(), 2);
        assert!(ids.contains(&n1.id.to_string()) && ids.contains(&n2.id.to_string()));

        // "Budget" only n1; unknown topic → empty.
        let budget_notes = graph.notes_by_topic(ws, "BUDGET", 10).await.unwrap();
        assert_eq!(budget_notes.len(), 1);
        assert_eq!(budget_notes[0].note_id, n1.id.to_string());
        assert!(graph
            .notes_by_topic(ws, "nope", 10)
            .await
            .unwrap()
            .is_empty());

        // Workspace isolation: another workspace sees none of it.
        let other = WorkspaceId::new();
        assert!(graph
            .related_notes(other, n1.id, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(graph
            .notes_by_topic(other, "scheduling", 10)
            .await
            .unwrap()
            .is_empty());

        graph.delete_workspace(ws).await.unwrap();
    }

    fn link(ws: WorkspaceId, from: SourceRef, to: SourceRef, label: Option<&str>) -> Link {
        Link {
            id: LinkId::new(),
            workspace_id: ws,
            from,
            to,
            label: label.map(str::to_owned),
            note: None,
            author: Author::User { id: UserId::new() },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// The `link_id`s of every `RELATES_TO` edge in a workspace — the typed
    /// verification read for the link projection.
    async fn link_ids(graph: &GraphStore, ws: WorkspaceId) -> Vec<String> {
        graph
            .run_one(crate::cypher::relates_to_link_ids(ws))
            .await
            .unwrap()
            .string_column("id")
    }

    #[tokio::test]
    async fn project_link_round_trips_and_purges() {
        let Some(graph) = store() else {
            eprintln!("NEO4J_URL unset; skipping live link projection test");
            return;
        };
        graph.ensure_indexes().await.expect("indexes");
        let ws = WorkspaceId::new();
        graph.delete_workspace(ws).await.unwrap();

        let note = SourceRef::Note { id: NoteId::new() };
        let event = SourceRef::Event { id: EventId::new() };
        let l = link(ws, note.clone(), event.clone(), Some("follow-up"));

        // Projects an edge; idempotent (a second projection adds nothing).
        assert!(graph.project_link(&l).await.expect("project"));
        assert!(graph.project_link(&l).await.expect("re-project"));
        assert_eq!(link_ids(&graph, ws).await, vec![l.id.to_string()]);

        // A second link (different label, same pair) is a *distinct* edge.
        let l2 = link(ws, note, event, Some("mentions"));
        assert!(graph.project_link(&l2).await.unwrap());
        assert_eq!(link_ids(&graph, ws).await.len(), 2);

        // An External endpoint has no graph node → no edge written, no error.
        let ext = link(
            ws,
            SourceRef::Note { id: NoteId::new() },
            SourceRef::External {
                uri: "https://example.com".into(),
            },
            None,
        );
        assert!(!graph.project_link(&ext).await.unwrap());
        assert_eq!(link_ids(&graph, ws).await.len(), 2, "External adds no edge");

        // Purge one link; its edge goes, the other stays.
        graph.delete_link(ws, l.id).await.unwrap();
        assert_eq!(link_ids(&graph, ws).await, vec![l2.id.to_string()]);

        graph.delete_workspace(ws).await.unwrap();
    }

    #[tokio::test]
    async fn project_entity_reporting_distinguishes_created_from_deduplicated() {
        let Some(graph) = store() else {
            eprintln!("NEO4J_URL unset; skipping live entity-dedup report test");
            return;
        };
        graph.ensure_indexes().await.expect("indexes");
        let ws = WorkspaceId::new();
        graph.delete_workspace(ws).await.unwrap();

        let ada = entity(ws, EntityKind::Person, "Ada Lovelace");

        // First projection creates the node → not previously present.
        assert!(!graph.project_entity_reporting(&ada).await.unwrap());
        // A re-projection of the same id reports it already existed (deduplicated),
        // and no second node is created.
        assert!(graph.project_entity_reporting(&ada).await.unwrap());
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 1);

        // A same-id re-projection carrying a fresher display_name updates in place —
        // still one node, still reported as pre-existing.
        let mut renamed = ada.clone();
        renamed.display_name = "Augusta Ada King".into();
        assert!(graph.project_entity_reporting(&renamed).await.unwrap());
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 1);

        // A different id (e.g. a distinct kind's node) is its own creation.
        let place = entity(ws, EntityKind::Place, "London");
        assert!(!graph.project_entity_reporting(&place).await.unwrap());
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 2);

        graph.delete_workspace(ws).await.unwrap();
    }
}
