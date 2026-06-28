//! The project-to-graph pipeline (SOUL §6.3/§10/§21): project a note into the
//! derived Neo4j graph as a `:Note` node plus a `:Topic` node per tag, linked by
//! `REFERENCES` edges.
//!
//! [`project_note_to_graph`] is the unit of work, and it is **reconcile-based**
//! (like [`crate::ingest_note`]): a present note is `MERGE`d (idempotent, no
//! dups); a note found deleted has its `:Note` node detach-deleted, so a delete
//! reconciles like any edit (SOUL §3.1). The graph is a **derived** projection —
//! rebuildable from Postgres truth — so a wiped graph costs a reprojection and
//! never data (principle 1).
//!
//! # Tags → Topics (a first projection signal)
//! Until the `entities` table + extraction pipeline lands (§10), the entities a
//! note references come from its **tags**: each tag becomes a `:Topic` node whose
//! id is a stable name-based UUIDv5 of `(workspace, normalized-tag)`, so the same
//! tag across notes is the *same* topic node and "which notes share a topic" is a
//! one-hop query. Richer `:Person`/`:Org`/… references extracted from the note
//! body layer on later, augmenting these tag-topics.
//!
//! # Job contract
//! [`enqueue_project_note`] writes a durable [`JOB_KIND_PROJECT_NOTE`] job whose
//! payload is [`ProjectNotePayload`]; a worker holding a [`GraphContext`] runs it.

use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

use catalerum_core::{Entity, EntityKind, Event, EventId, LinkId, Note, NoteId, WorkspaceId};
use catalerum_graph::{GraphStore, NodeRef};
use catalerum_store::Store;

use crate::error::Result;

/// The `job_queue.kind` token for a graph-projection job.
pub const JOB_KIND_PROJECT_NOTE: &str = "project_note";

/// What one [`project_note_to_graph`] run produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectReport {
    /// How many `:Topic` nodes the note referenced (0 when purged or untagged).
    pub topics: usize,
    /// `true` when the note was found deleted and its node was purged instead.
    pub purged: bool,
}

/// Project (or purge) one note in the derived graph (SOUL §6.3/§21). Idempotent.
pub async fn project_note_to_graph(
    store: &Store,
    graph: &GraphStore,
    workspace_id: WorkspaceId,
    note_id: NoteId,
) -> Result<ProjectReport> {
    match store.notes().get(workspace_id, note_id).await {
        Ok(note) => {
            let topics = tag_topics(&note);
            graph.project_note(&note, &topics).await?;
            debug!(%note_id, topics = topics.len(), "projected note to graph");
            Ok(ProjectReport {
                topics: topics.len(),
                purged: false,
            })
        }
        Err(catalerum_store::StoreError::NotFound) => {
            // The note was deleted: detach-delete its node (its REFERENCES edges
            // go with it; shared Topic nodes are left for other notes, §6.3).
            graph
                .delete_node(workspace_id, &NodeRef::note(note_id))
                .await?;
            debug!(%note_id, "purged deleted note from graph");
            Ok(ProjectReport {
                topics: 0,
                purged: true,
            })
        }
        Err(e) => Err(e.into()),
    }
}

/// Synthesize a `:Topic` [`Entity`] per distinct note tag (case-insensitive).
fn tag_topics(note: &Note) -> Vec<Entity> {
    topics_from(note.workspace_id, &note.tags)
}

/// Synthesize a `:Topic` [`Entity`] per distinct event label (case-insensitive)
/// — the calendar twin of [`tag_topics`], so an event labelled `work` shares the
/// *same* `:Topic` node as a note tagged `work` (SOUL §6.3/§8).
fn label_topics(event: &Event) -> Vec<Entity> {
    topics_from(event.workspace_id, &event.labels)
}

/// Synthesize a `:Topic` [`Entity`] per distinct name within a workspace, routed
/// through the shared entity dedup seam ([`crate::entity_dedup`], SOUL §29): tags
/// differing only in case or internal whitespace fold to one `:Topic` on every
/// projection and across notes *and* events (idempotent §3.4), so the graph is
/// never diluted with duplicate topics. Blanks are dropped; the first-seen casing
/// is kept for display.
fn topics_from(workspace_id: WorkspaceId, names: &[String]) -> Vec<Entity> {
    crate::entity_dedup::resolve_entities(
        workspace_id,
        names.iter().map(|n| (EntityKind::Topic, n.as_str())),
    )
}

/// The services a worker needs to run a [`JOB_KIND_PROJECT_NOTE`] job: a
/// [`GraphStore`]. Bundled (like [`crate::EmbedContext`]) so the polling worker
/// holds one optional handle and projects when present.
#[derive(Clone, Debug)]
pub struct GraphContext {
    /// The derived Neo4j graph.
    pub graph: GraphStore,
}

impl GraphContext {
    /// Bundle the graph store for projection.
    #[must_use]
    pub fn new(graph: GraphStore) -> Self {
        Self { graph }
    }

    /// Run [`project_note_to_graph`] for `note_id`.
    pub async fn project_note(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<ProjectReport> {
        project_note_to_graph(store, &self.graph, workspace_id, note_id).await
    }

    /// Run [`project_event_to_graph`] for `event_id` (returns `true` if purged).
    pub async fn project_event(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        event_id: EventId,
    ) -> Result<bool> {
        project_event_to_graph(store, &self.graph, workspace_id, event_id).await
    }

    /// Run [`project_link_to_graph`] for `link_id` (returns `true` if purged).
    pub async fn project_link(
        &self,
        store: &Store,
        workspace_id: WorkspaceId,
        link_id: LinkId,
    ) -> Result<bool> {
        project_link_to_graph(store, &self.graph, workspace_id, link_id).await
    }
}

/// The JSON payload of a [`JOB_KIND_PROJECT_NOTE`] job (same workspace-optional
/// shape as [`crate::IngestNotePayload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectNotePayload {
    /// The workspace that owns the note. Optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The note to project.
    pub note_id: NoteId,
}

impl ProjectNotePayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, note_id: NoteId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            note_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_note(note_id: NoteId) -> Self {
        Self {
            workspace_id: None,
            note_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_PROJECT_NOTE`] job for `note_id` (SOUL §6.2/§10).
pub async fn enqueue_project_note(
    store: &Store,
    workspace_id: WorkspaceId,
    note_id: NoteId,
) -> Result<Uuid> {
    let payload = ProjectNotePayload::new(workspace_id, note_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_PROJECT_NOTE,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %note_id, "enqueued project_note job");
    Ok(job.id)
}

// ---------------------------------------------------------------------------
// Event projection (SOUL §6.3/§8) — the calendar twin of note projection.
// ---------------------------------------------------------------------------

/// The `job_queue.kind` token for an event graph-projection job.
pub const JOB_KIND_PROJECT_EVENT: &str = "project_event";

/// Project (or purge) one calendar event in the derived graph (SOUL §6.3/§8): an
/// `:Event` node + a `SCHEDULED_IN` edge to its `:Calendar` (see
/// [`GraphStore::project_event`](catalerum_graph::GraphStore::project_event)).
/// Reconcile-based like [`project_note_to_graph`]: a present event is `MERGE`d
/// (idempotent), a deleted one has its node detach-deleted. Returns `true` when the
/// event was found deleted and purged instead of projected.
pub async fn project_event_to_graph(
    store: &Store,
    graph: &GraphStore,
    workspace_id: WorkspaceId,
    event_id: EventId,
) -> Result<bool> {
    match store.events().get(workspace_id, event_id).await {
        Ok(event) => {
            let topics = label_topics(&event);
            graph.project_event(&event, &topics).await?;
            debug!(%event_id, topics = topics.len(), "projected event to graph");
            Ok(false)
        }
        Err(catalerum_store::StoreError::NotFound) => {
            graph
                .delete_node(workspace_id, &NodeRef::event(event_id))
                .await?;
            debug!(%event_id, "purged deleted event from graph");
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

/// The JSON payload of a [`JOB_KIND_PROJECT_EVENT`] job (workspace-optional, the
/// same shape as [`ProjectNotePayload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEventPayload {
    /// The workspace that owns the event. Optional on the wire (defers to the job row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The event to project.
    pub event_id: EventId,
}

impl ProjectEventPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, event_id: EventId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            event_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_event(event_id: EventId) -> Self {
        Self {
            workspace_id: None,
            event_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_PROJECT_EVENT`] job for `event_id` (SOUL §6.2/§8).
pub async fn enqueue_project_event(
    store: &Store,
    workspace_id: WorkspaceId,
    event_id: EventId,
) -> Result<Uuid> {
    let payload = ProjectEventPayload::new(workspace_id, event_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_PROJECT_EVENT,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %event_id, "enqueued project_event job");
    Ok(job.id)
}

// ---------------------------------------------------------------------------
// Link projection (SOUL §6.3) — user/agent-authored `RELATES_TO` edges.
// ---------------------------------------------------------------------------

/// The `job_queue.kind` token for a link graph-projection job.
pub const JOB_KIND_PROJECT_LINK: &str = "project_link";

/// Project (or purge) one link in the derived graph (SOUL §6.3): upsert a
/// `RELATES_TO` edge between its endpoints (see
/// [`GraphStore::project_link`](catalerum_graph::GraphStore::project_link)).
/// Reconcile-based like [`project_note_to_graph`]: a present link is `MERGE`d
/// (idempotent), a deleted one has its edge detached. Returns `true` when the link
/// was found deleted and purged instead of projected.
pub async fn project_link_to_graph(
    store: &Store,
    graph: &GraphStore,
    workspace_id: WorkspaceId,
    link_id: LinkId,
) -> Result<bool> {
    match store.links().get(workspace_id, link_id).await {
        Ok(link) => {
            let wrote_edge = graph.project_link(&link).await?;
            debug!(%link_id, wrote_edge, "projected link to graph");
            Ok(false)
        }
        Err(catalerum_store::StoreError::NotFound) => {
            graph.delete_link(workspace_id, link_id).await?;
            debug!(%link_id, "purged deleted link from graph");
            Ok(true)
        }
        Err(e) => Err(e.into()),
    }
}

/// The JSON payload of a [`JOB_KIND_PROJECT_LINK`] job (workspace-optional, the
/// same shape as [`ProjectNotePayload`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLinkPayload {
    /// The workspace that owns the link. Optional on the wire (defers to the job row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The link to project.
    pub link_id: LinkId,
}

impl ProjectLinkPayload {
    /// A payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, link_id: LinkId) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            link_id,
        }
    }

    /// A payload that defers its scope to the job row's `workspace_id` column.
    #[must_use]
    pub fn for_link(link_id: LinkId) -> Self {
        Self {
            workspace_id: None,
            link_id,
        }
    }
}

/// Enqueue a durable [`JOB_KIND_PROJECT_LINK`] job for `link_id` (SOUL §6.2/§6.3).
pub async fn enqueue_project_link(
    store: &Store,
    workspace_id: WorkspaceId,
    link_id: LinkId,
) -> Result<Uuid> {
    let payload = ProjectLinkPayload::new(workspace_id, link_id);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_PROJECT_LINK,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %link_id, "enqueued project_link job");
    Ok(job.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::model::Author;
    use catalerum_core::{EntityId, UserId};
    use chrono::Utc;

    /// The stable topic-node id for a normalized tag — the `EntityKind::Topic`
    /// case of the shared, kind-scoped [`crate::entity_dedup::entity_id`], which the
    /// tag/label `:Topic` synthesis now routes through.
    fn topic_id(ws: WorkspaceId, normalized_tag: &str) -> EntityId {
        crate::entity_dedup::entity_id(ws, EntityKind::Topic, normalized_tag)
    }

    fn note_with_tags(ws: WorkspaceId, tags: &[&str]) -> Note {
        Note {
            id: NoteId::new(),
            workspace_id: ws,
            author: Author::User { id: UserId::new() },
            title: "t".into(),
            markdown: String::new(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn tag_topics_dedups_case_insensitively_and_is_stable() {
        let ws = WorkspaceId::new();
        let note = note_with_tags(ws, &["Work", "work", "  ", "Ideas"]);
        let topics = tag_topics(&note);
        // "Work"/"work" collapse; blank dropped → 2 distinct topics.
        assert_eq!(topics.len(), 2);
        assert!(topics.iter().all(|t| t.kind == EntityKind::Topic));
        assert!(topics.iter().all(|t| t.workspace_id == ws));
        // Display keeps the first-seen casing; ids are case-insensitive-stable.
        assert_eq!(topics[0].display_name, "Work");
        assert_eq!(topics[0].id, topic_id(ws, "work"));
        assert_eq!(topics[1].id, topic_id(ws, "ideas"));
    }

    #[test]
    fn topic_normalization_folds_internal_whitespace_and_case() {
        let ws = WorkspaceId::new();
        // Same topic written three ways (case + internal/edge whitespace) → one node.
        let note = note_with_tags(
            ws,
            &[
                "Machine Learning",
                "machine  learning",
                "  MACHINE LEARNING  ",
            ],
        );
        let topics = tag_topics(&note);
        assert_eq!(topics.len(), 1, "got {topics:?}");
        // Display is the first-seen form, whitespace-collapsed; id is the folded key.
        assert_eq!(topics[0].display_name, "Machine Learning");
        assert_eq!(topics[0].id, topic_id(ws, "machine learning"));
    }

    #[test]
    fn topic_id_is_workspace_scoped_and_deterministic() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert_eq!(topic_id(a, "x"), topic_id(a, "x")); // stable
        assert_ne!(topic_id(a, "x"), topic_id(b, "x")); // per-workspace
        assert_ne!(topic_id(a, "x"), topic_id(a, "y")); // per-tag
    }

    #[test]
    fn payload_round_trips_and_accepts_note_only_shape() {
        let p = ProjectNotePayload::new(WorkspaceId::new(), NoteId::new());
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(
            serde_json::from_value::<ProjectNotePayload>(json).unwrap(),
            p
        );
        let note = NoteId::new();
        let p2: ProjectNotePayload =
            serde_json::from_value(serde_json::json!({ "note_id": note })).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.note_id, note);
        assert!(serde_json::to_value(ProjectNotePayload::for_note(note))
            .unwrap()
            .get("workspace_id")
            .is_none());
    }

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_PROJECT_NOTE, "project_note");
    }

    #[test]
    fn event_payload_round_trips_and_accepts_event_only_shape() {
        let p = ProjectEventPayload::new(WorkspaceId::new(), EventId::new());
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(
            serde_json::from_value::<ProjectEventPayload>(json).unwrap(),
            p
        );
        let ev = EventId::new();
        let p2: ProjectEventPayload =
            serde_json::from_value(serde_json::json!({ "event_id": ev })).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.event_id, ev);
        assert!(serde_json::to_value(ProjectEventPayload::for_event(ev))
            .unwrap()
            .get("workspace_id")
            .is_none());
    }

    #[test]
    fn project_event_job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_PROJECT_EVENT, "project_event");
    }

    #[test]
    fn link_payload_round_trips_and_accepts_link_only_shape() {
        let p = ProjectLinkPayload::new(WorkspaceId::new(), LinkId::new());
        let json = serde_json::to_value(p).unwrap();
        assert_eq!(
            serde_json::from_value::<ProjectLinkPayload>(json).unwrap(),
            p
        );
        let link = LinkId::new();
        let p2: ProjectLinkPayload =
            serde_json::from_value(serde_json::json!({ "link_id": link })).unwrap();
        assert_eq!(p2.workspace_id, None);
        assert_eq!(p2.link_id, link);
        assert!(serde_json::to_value(ProjectLinkPayload::for_link(link))
            .unwrap()
            .get("workspace_id")
            .is_none());
    }

    #[test]
    fn project_link_job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_PROJECT_LINK, "project_link");
    }
}
