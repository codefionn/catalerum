//! The Neo4j HTTP-transactional Cypher client (SOUL §6.3).
//!
//! Neo4j is a **derived** projection: everything here is rebuildable from
//! Postgres truth, so a wiped graph costs a reprojection and never data
//! (principle 1, §3.1). The client drives Neo4j's HTTP transactional endpoint
//! (`POST {base}/db/{database}/tx/commit`) rather than Bolt — a thin `reqwest`
//! client, mirroring [`catalerum-vector`](catalerum_vector)'s Qdrant client, so
//! the crate stays dependency-light and the same auth/TLS story applies.
//!
//! Every node is keyed on `(workspace_id, id)` and every query filters on
//! `workspace_id`, so cross-workspace reach is impossible by construction
//! (SOUL §18). Writers use idempotent `MERGE`, so a re-projection of the same
//! Postgres rows is a no-op — never a duplicate (SOUL §6.3).

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use catalerum_core::{Entity, Event, EventId, Link, LinkId, Note, NoteId, WorkspaceId};

use crate::cypher::{
    self, count_nodes, delete_link_edge, delete_node, delete_workspace, ensure_index, load_edges,
    load_nodes, merge_edge, merge_link_edge, merge_node, node_exists, notes_by_topic,
    out_neighbor_ids, related_notes, QueryResult, Statement,
};
use crate::error::{GraphError, Result};
use crate::model::{EdgeType, NodeLabel, NodeRef};

/// Every node label catalerum projects (SOUL §6.3) — the set we build the
/// `(workspace_id, id)` MERGE-key index for at startup.
const ALL_LABELS: [NodeLabel; 16] = [
    NodeLabel::Person,
    NodeLabel::Org,
    NodeLabel::Topic,
    NodeLabel::Project,
    NodeLabel::Place,
    NodeLabel::Event,
    NodeLabel::File,
    NodeLabel::Note,
    NodeLabel::Task,
    NodeLabel::Conversation,
    NodeLabel::Calendar,
    NodeLabel::Bucket,
    NodeLabel::Email,
    NodeLabel::Memory,
    NodeLabel::Document,
    NodeLabel::Message,
];

/// Every relationship type catalerum projects (SOUL §6.3) — used to filter the
/// edges the Datalog fact loader hands to the evaluator to the closed taxonomy.
const ALL_EDGES: [EdgeType; 9] = [
    EdgeType::Attends,
    EdgeType::About,
    EdgeType::Mentions,
    EdgeType::StoredIn,
    EdgeType::ScheduledIn,
    EdgeType::Follows,
    EdgeType::RelatesTo,
    EdgeType::DerivedFrom,
    EdgeType::References,
];

/// Default per-workspace caps on the nodes/edges the Datalog fact loader
/// materializes ([`GraphStore::load_workspace_facts`]). A workspace larger than a
/// cap is loaded up to it and flagged [`WorkspaceFacts::truncated`], so a partial
/// load can never masquerade as complete. Callers may pass tighter caps.
pub const MAX_WORKSPACE_NODES: i64 = 50_000;
/// Companion of [`MAX_WORKSPACE_NODES`] for edges.
pub const MAX_WORKSPACE_EDGES: i64 = 50_000;

/// A thin async client over Neo4j's HTTP transactional API.
#[derive(Clone, Debug)]
pub struct GraphStore {
    http: reqwest::Client,
    /// Base URL with no trailing slash, e.g. `http://localhost:7474`.
    base: String,
    /// The Neo4j database name (default `neo4j`).
    database: String,
    /// Optional HTTP-basic credentials (`NEO4J_AUTH=user/password`).
    auth: Option<(String, String)>,
}

/// The default HTTP client for talking to Neo4j: a short **connect timeout**
/// (fail fast when the host is unreachable instead of hanging) plus a generous
/// overall **request timeout** as a backstop so a server that stalls mid-response
/// can never block a projection/query worker indefinitely. Cypher commits are
/// normally sub-second, so the 60 s cap is slack. Callers that need different
/// behaviour use [`GraphStore::with_client`].
fn default_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

impl GraphStore {
    /// Connect to the Neo4j at `base_url` (e.g. `http://localhost:7474`) with no
    /// auth and the default `neo4j` database. The URL is validated but no
    /// request is made. Add credentials with [`with_auth`](Self::with_auth).
    pub fn new(base_url: &str) -> Result<Self> {
        Self::with_client(default_http_client()?, base_url)
    }

    /// Build a store over an existing [`reqwest::Client`] (share a connection
    /// pool, configure timeouts/proxies upstream).
    pub fn with_client(http: reqwest::Client, base_url: &str) -> Result<Self> {
        let parsed = url::Url::parse(base_url)?;
        let base = parsed.as_str().trim_end_matches('/').to_owned();
        Ok(Self {
            http,
            base,
            database: "neo4j".to_owned(),
            auth: None,
        })
    }

    /// Set HTTP-basic credentials (the `neo4j/<password>` of `NEO4J_AUTH`).
    #[must_use]
    pub fn with_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Some((user.into(), password.into()));
        self
    }

    /// Override the target database (default `neo4j`).
    #[must_use]
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    fn commit_url(&self) -> String {
        format!("{}/db/{}/tx/commit", self.base, self.database)
    }

    /// Run one or more statements as a single committed transaction, returning a
    /// [`QueryResult`] per statement (in order). An empty input is a no-op that
    /// returns no results. Either every statement commits or the whole
    /// transaction rolls back (Neo4j `/tx/commit` semantics).
    pub async fn run(&self, statements: &[Statement]) -> Result<Vec<QueryResult>> {
        if statements.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({ "statements": statements });
        let mut req = self.http.post(self.commit_url()).json(&body);
        if let Some((user, password)) = &self.auth {
            req = req.basic_auth(user, Some(password));
        }
        let resp = req.send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(GraphError::Http { status, body });
        }

        let value: Value = resp.json().await?;
        cypher::parse_commit_response(&value)
    }

    /// Run a single statement, returning its one [`QueryResult`].
    pub async fn run_one(&self, statement: Statement) -> Result<QueryResult> {
        let mut results = self.run(std::slice::from_ref(&statement)).await?;
        results
            .pop()
            .ok_or_else(|| GraphError::Malformed("expected one statement result, got none".into()))
    }

    /// Liveness probe — `GET {base}/`. Cheap; use it to fail fast at startup.
    pub async fn healthz(&self) -> Result<()> {
        let resp = self.http.get(format!("{}/", self.base)).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(GraphError::Http { status, body })
        }
    }

    /// Idempotently create the `(workspace_id, id)` range index for every node
    /// label (SOUL §6.3 — the MERGE key). Safe to call on every boot; each
    /// `CREATE INDEX … IF NOT EXISTS` is a no-op once present.
    pub async fn ensure_indexes(&self) -> Result<()> {
        let statements: Vec<Statement> = ALL_LABELS.iter().map(|&l| ensure_index(l)).collect();
        self.run(&statements).await.map(|_| ())
    }

    /// Upsert a single [`Entity`] as a node, labelled by its kind (SOUL §5/§6.3).
    pub async fn project_entity(&self, entity: &Entity) -> Result<()> {
        self.run_one(entity_merge(entity)).await.map(|_| ())
    }

    /// Upsert an [`Entity`] node like [`project_entity`], but report whether a node
    /// with that `(workspace_id, id)` **already existed** — the created-vs-
    /// deduplicated signal the entity dedup seam surfaces (SOUL §29). Runs the
    /// existence probe and the idempotent `MERGE` as one transaction, in order, so
    /// the probe reads the pre-`MERGE` state: a first projection returns `false`
    /// (created), a re-projection of the same id returns `true` (deduplicated). The
    /// `MERGE` runs either way, so `display_name`/`aliases` stay current and any
    /// edges a caller then draws attach to the single surviving node.
    ///
    /// Best-effort observability, not a lock: two racing first-projections of the
    /// same id may *both* report `false`, but `MERGE` still yields one node — the
    /// dedup invariant holds regardless of the reported status.
    pub async fn project_entity_reporting(&self, entity: &Entity) -> Result<bool> {
        let node = NodeRef::entity(entity.kind, entity.id);
        let results = self
            .run(&[
                node_exists(entity.workspace_id, &node),
                entity_merge(entity),
            ])
            .await?;
        let existed = results
            .first()
            .and_then(QueryResult::scalar_i64)
            .unwrap_or(0)
            > 0;
        Ok(existed)
    }

    /// Project a [`Note`] and its references in one transaction: upsert the
    /// `:Note` node, upsert each referenced [`Entity`] node, and `MERGE` a
    /// `REFERENCES` edge from the note to each (SOUL §6.3/§21). Idempotent: a
    /// re-projection of the same note never duplicates nodes or edges.
    ///
    /// All `references` must belong to the note's workspace; any that do not are
    /// rejected (cross-workspace edges are impossible by construction, §18).
    pub async fn project_note(&self, note: &Note, references: &[Entity]) -> Result<()> {
        if let Some(bad) = references
            .iter()
            .find(|e| e.workspace_id != note.workspace_id)
        {
            return Err(GraphError::Malformed(format!(
                "entity {} is in workspace {}, not the note's workspace {}",
                bad.id, bad.workspace_id, note.workspace_id
            )));
        }

        let note_ref = NodeRef::note(note.id);
        let mut statements = Vec::with_capacity(1 + references.len() * 2);
        statements.push(merge_node(note.workspace_id, &note_ref, note_props(note)));
        for entity in references {
            statements.push(entity_merge(entity));
            let entity_ref = NodeRef::entity(entity.kind, entity.id);
            statements.push(merge_edge(
                note.workspace_id,
                &note_ref,
                EdgeType::References,
                &entity_ref,
            ));
        }
        self.run(&statements).await.map(|_| ())
    }

    /// Project an [`Event`](catalerum_core::Event) and its label-topics into the
    /// derived graph (SOUL §6.3/§8): upsert the `:Event` node, `MERGE` a
    /// `SCHEDULED_IN` edge to its `:Calendar` node (a thin node keyed by
    /// `calendar_id`, enriched later by a calendar projection — [`merge_edge`]
    /// creates the endpoint), and upsert each label `:Topic` node with an `ABOUT`
    /// edge from the event to it (the calendar twin of a note's tag-topics). The
    /// label topics come from [`Event::labels`](catalerum_core::Event), resolved
    /// to `:Topic` entities by the ingest pipeline. Idempotent: a re-projection
    /// of the same event never duplicates nodes or edges.
    ///
    /// All `topics` must belong to the event's workspace; any that do not are
    /// rejected (cross-workspace edges are impossible by construction, §18).
    pub async fn project_event(&self, event: &Event, topics: &[Entity]) -> Result<()> {
        if let Some(bad) = topics.iter().find(|e| e.workspace_id != event.workspace_id) {
            return Err(GraphError::Malformed(format!(
                "entity {} is in workspace {}, not the event's workspace {}",
                bad.id, bad.workspace_id, event.workspace_id
            )));
        }

        let event_ref = NodeRef::event(event.id);
        let calendar_ref = NodeRef::calendar(event.calendar_id);
        let mut statements = Vec::with_capacity(2 + topics.len() * 2);
        statements.push(merge_node(
            event.workspace_id,
            &event_ref,
            event_props(event),
        ));
        statements.push(merge_edge(
            event.workspace_id,
            &event_ref,
            EdgeType::ScheduledIn,
            &calendar_ref,
        ));
        for topic in topics {
            statements.push(entity_merge(topic));
            let topic_ref = NodeRef::entity(topic.kind, topic.id);
            statements.push(merge_edge(
                event.workspace_id,
                &event_ref,
                EdgeType::About,
                &topic_ref,
            ));
        }
        self.run(&statements).await.map(|_| ())
    }

    /// Project a [`Link`] into the derived graph as a `RELATES_TO` edge
    /// (SOUL §6.3): `MERGE` both endpoint nodes (thin placeholders a later
    /// projection enriches) and the `(workspace_id, link_id)`-keyed relationship,
    /// overlaying `label`/`note`/`updated_at`. Idempotent: a re-projection of the
    /// same link never duplicates the edge.
    ///
    /// Returns `true` when an edge was written, `false` when an endpoint is a
    /// [`SourceRef::External`](catalerum_core::SourceRef) (no graph node) — the
    /// link still lives in Postgres, it just isn't graph-traversable.
    pub async fn project_link(&self, link: &Link) -> Result<bool> {
        let (Some(from), Some(to)) = (
            NodeRef::from_source(&link.from),
            NodeRef::from_source(&link.to),
        ) else {
            return Ok(false);
        };
        let mut props = Map::new();
        // Always set label/note (null when absent) so a re-projection of a link
        // whose label/note was cleared overwrites the stale value (as with
        // `event_props`' location/labels: `+=` never removes absent keys).
        props.insert("label".into(), json!(link.label));
        props.insert("note".into(), json!(link.note));
        props.insert("updated_at".into(), json!(rfc3339(link.updated_at)));
        self.run_one(merge_link_edge(
            link.workspace_id,
            &link.id.to_string(),
            &from,
            &to,
            props,
        ))
        .await
        .map(|_| true)
    }

    /// Detach the `RELATES_TO` edge for a deleted [`Link`] (SOUL §6.3) — the purge
    /// basis for a removed `links` row. A no-op if absent; endpoint nodes are left
    /// in place. Mirrors [`delete_node`](Self::delete_node).
    pub async fn delete_link(&self, workspace_id: WorkspaceId, link_id: LinkId) -> Result<()> {
        self.run_one(delete_link_edge(workspace_id, &link_id.to_string()))
            .await
            .map(|_| ())
    }

    /// The stable ids of the entities a note `REFERENCES` (SOUL §6.3/§21),
    /// scoped to `workspace_id`.
    pub async fn references_of(
        &self,
        workspace_id: WorkspaceId,
        note_id: NoteId,
    ) -> Result<Vec<String>> {
        let note_ref = NodeRef::note(note_id);
        let result = self
            .run_one(out_neighbor_ids(
                workspace_id,
                &note_ref,
                EdgeType::References,
            ))
            .await?;
        Ok(result.string_column("id"))
    }

    /// The stable ids of the `:Calendar` nodes an event is `SCHEDULED_IN` (SOUL
    /// §6.3/§8), scoped to `workspace_id` — the calendar twin of [`references_of`].
    /// Normally one (the event's calendar); used to verify/traverse the projection.
    pub async fn scheduled_in(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
    ) -> Result<Vec<String>> {
        let event_ref = NodeRef::event(event_id);
        let result = self
            .run_one(out_neighbor_ids(
                workspace_id,
                &event_ref,
                EdgeType::ScheduledIn,
            ))
            .await?;
        Ok(result.string_column("id"))
    }

    /// The stable ids of the `:Topic` nodes an event is `ABOUT` — its label
    /// topics (SOUL §6.3/§8), scoped to `workspace_id`. The label twin of
    /// [`references_of`]; used to verify/traverse the label projection.
    pub async fn about_of(
        &self,
        workspace_id: WorkspaceId,
        event_id: EventId,
    ) -> Result<Vec<String>> {
        let event_ref = NodeRef::event(event_id);
        let result = self
            .run_one(out_neighbor_ids(workspace_id, &event_ref, EdgeType::About))
            .await?;
        Ok(result.string_column("id"))
    }

    /// Notes that share at least one `:Topic` with `note_id`, ranked by the
    /// number of shared topics (SOUL §6.3/§6.5). A typed, read-only, workspace-
    /// scoped graph query — the basis for the `query_graph` retrieval tool.
    pub async fn related_notes(
        &self,
        workspace_id: WorkspaceId,
        note_id: NoteId,
        limit: i64,
    ) -> Result<Vec<RelatedNote>> {
        let note_ref = NodeRef::note(note_id);
        let result = self
            .run_one(related_notes(workspace_id, &note_ref, limit.max(1)))
            .await?;
        Ok(rows_to(&result, |get| {
            Some(RelatedNote {
                note_id: get.str("id")?.to_owned(),
                title: get.str("title").map(str::to_owned),
                shared_topics: get.i64("shared").unwrap_or(0),
            })
        }))
    }

    /// Notes referencing a `:Topic` whose display name matches `topic`
    /// (case-insensitive), workspace-scoped (SOUL §6.3/§6.5). Read-only.
    pub async fn notes_by_topic(
        &self,
        workspace_id: WorkspaceId,
        topic: &str,
        limit: i64,
    ) -> Result<Vec<NoteHit>> {
        let result = self
            .run_one(notes_by_topic(workspace_id, topic, limit.max(1)))
            .await?;
        Ok(rows_to(&result, |get| {
            Some(NoteHit {
                note_id: get.str("id")?.to_owned(),
                title: get.str("title").map(str::to_owned),
            })
        }))
    }

    /// Materialize one workspace's nodes, properties, and edges into a
    /// [`WorkspaceFacts`] set — the extensional facts the in-process Datalog
    /// evaluator ([`catalerum-logic`](https://docs.rs/catalerum-logic)) runs over
    /// (SOUL §6.3/§18). Runs two fixed, structurally-scoped Cypher reads in one
    /// transaction; no user query text reaches Neo4j, so cross-workspace reach is
    /// impossible by construction. `node_cap`/`edge_cap` bound the rows Neo4j
    /// materializes (a workspace larger than a cap is flagged
    /// [`truncated`](WorkspaceFacts::truncated)). Each cap is floored at 1.
    pub async fn load_workspace_facts(
        &self,
        workspace_id: WorkspaceId,
        node_cap: i64,
        edge_cap: i64,
    ) -> Result<WorkspaceFacts> {
        let node_cap = node_cap.max(1);
        let edge_cap = edge_cap.max(1);
        let results = self
            .run(&[
                load_nodes(workspace_id, node_cap),
                load_edges(workspace_id, edge_cap),
            ])
            .await?;
        let mut it = results.into_iter();
        let nodes = it.next().unwrap_or_default();
        let edges = it.next().unwrap_or_default();
        Ok(parse_workspace_facts(&nodes, &edges, node_cap, edge_cap))
    }

    /// Count all nodes in a workspace.
    pub async fn count_nodes(&self, workspace_id: WorkspaceId) -> Result<i64> {
        let result = self.run_one(count_nodes(workspace_id)).await?;
        Ok(result.scalar_i64().unwrap_or(0))
    }

    /// Detach-delete one node (and its relationships) in a workspace. A no-op if
    /// it is absent. The re-projection basis for a single source: delete then
    /// re-`project_*` (SOUL §6.3, like [`catalerum_vector`]'s `delete_by_source`).
    pub async fn delete_node(&self, workspace_id: WorkspaceId, node: &NodeRef) -> Result<()> {
        self.run_one(delete_node(workspace_id, node))
            .await
            .map(|_| ())
    }

    /// Detach-delete **every** node in a workspace — the per-workspace full
    /// rebuild basis (SOUL §3.1/§6.3).
    pub async fn delete_workspace(&self, workspace_id: WorkspaceId) -> Result<()> {
        self.run_one(delete_workspace(workspace_id))
            .await
            .map(|_| ())
    }
}

/// A note related to another via shared `:Topic` nodes (SOUL §6.5). Ids are the
/// stable external ids (uuid strings); resolve back to Postgres rows as needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelatedNote {
    /// The related note's id (uuid string).
    pub note_id: String,
    /// Its title, if the node carries one.
    pub title: Option<String>,
    /// How many topics it shares with the query note.
    pub shared_topics: i64,
}

/// A note matched by a graph query — its id and (optional) title.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteHit {
    /// The note's id (uuid string).
    pub note_id: String,
    /// Its title, if the node carries one.
    pub title: Option<String>,
}

/// One workspace's extensional facts for the Datalog evaluator (SOUL §6.3): the
/// `node`/`edge`/`prop` triples, filtered to the closed §6.3 taxonomy. Deliberately
/// graph-native (plain string triples) so this crate stays Neo4j-only; the async
/// callers convert it into a `catalerum_logic::Facts` (which they already depend
/// on). `workspace_id`/`id` are never emitted as props — the language cannot name a
/// workspace, so `prop(X, "workspace_id", _)` matches nothing (§18).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceFacts {
    /// `(id, label)` — one per node whose label is in the taxonomy.
    pub nodes: Vec<(String, String)>,
    /// `(from_id, edge_type, to_id)` — one per edge whose type is in the taxonomy.
    pub edges: Vec<(String, String, String)>,
    /// `(id, key, value)` — one per scalar prop; list props (`tags`/`aliases`/
    /// `labels`) are expanded to one fact per element.
    pub props: Vec<(String, String, String)>,
    /// Whether a `node_cap`/`edge_cap` was hit — the fact set is partial.
    pub truncated: bool,
}

/// The column index of `name` in a result, if present.
fn col(result: &QueryResult, name: &str) -> Option<usize> {
    result.columns.iter().position(|c| c == name)
}

/// Turn the two loader [`QueryResult`]s (`load_nodes` / `load_edges`) into
/// [`WorkspaceFacts`]: keep only taxonomy labels/edge-types, drop the
/// `workspace_id`/`id` props, stringify scalar prop values, and expand array props
/// to one fact per element. Pure, so it is unit-testable without a live Neo4j.
fn parse_workspace_facts(
    nodes: &QueryResult,
    edges: &QueryResult,
    node_cap: i64,
    edge_cap: i64,
) -> WorkspaceFacts {
    use std::collections::HashSet;
    let known_labels: HashSet<&str> = ALL_LABELS.iter().map(|l| l.as_cypher()).collect();
    let known_edges: HashSet<&str> = ALL_EDGES.iter().map(|e| e.as_cypher()).collect();

    let (id_i, lab_i, props_i) = (col(nodes, "id"), col(nodes, "labels"), col(nodes, "props"));
    let mut out_nodes = Vec::new();
    let mut out_props = Vec::new();
    for row in &nodes.rows {
        let Some(id) = id_i.and_then(|i| row.get(i)).and_then(Value::as_str) else {
            continue;
        };
        // A projection node carries exactly one taxonomy label; take it (skip a
        // node with none, e.g. a stray or mislabelled row).
        let label = lab_i
            .and_then(|i| row.get(i))
            .and_then(Value::as_array)
            .and_then(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .find(|l| known_labels.contains(l))
            });
        let Some(label) = label else { continue };
        out_nodes.push((id.to_owned(), label.to_owned()));
        if let Some(props) = props_i.and_then(|i| row.get(i)).and_then(Value::as_object) {
            for (key, value) in props {
                if key == "workspace_id" || key == "id" {
                    continue;
                }
                push_prop_facts(id, key, value, &mut out_props);
            }
        }
    }

    let (from_i, type_i, to_i) = (col(edges, "from"), col(edges, "type"), col(edges, "to"));
    let mut out_edges = Vec::new();
    for row in &edges.rows {
        let (Some(from), Some(etype), Some(to)) = (
            from_i.and_then(|i| row.get(i)).and_then(Value::as_str),
            type_i.and_then(|i| row.get(i)).and_then(Value::as_str),
            to_i.and_then(|i| row.get(i)).and_then(Value::as_str),
        ) else {
            continue;
        };
        if known_edges.contains(etype) {
            out_edges.push((from.to_owned(), etype.to_owned(), to.to_owned()));
        }
    }

    // Conservative: rows at the cap are treated as possibly-clipped.
    let truncated = nodes.rows.len() as i64 >= node_cap || edges.rows.len() as i64 >= edge_cap;
    WorkspaceFacts {
        nodes: out_nodes,
        edges: out_edges,
        props: out_props,
        truncated,
    }
}

/// Push `prop(id, key, …)` facts for a JSON property value: a scalar yields one
/// fact (stringified), an array yields one per scalar element; objects/nulls are
/// skipped (not modelled in the graph relations).
fn push_prop_facts(id: &str, key: &str, value: &Value, out: &mut Vec<(String, String, String)>) {
    fn scalar(id: &str, key: &str, v: &Value, out: &mut Vec<(String, String, String)>) {
        match v {
            Value::String(s) => out.push((id.to_owned(), key.to_owned(), s.clone())),
            Value::Bool(b) => out.push((id.to_owned(), key.to_owned(), b.to_string())),
            Value::Number(n) => out.push((id.to_owned(), key.to_owned(), n.to_string())),
            _ => {}
        }
    }
    match value {
        Value::Array(items) => {
            for v in items {
                scalar(id, key, v, out);
            }
        }
        other => scalar(id, key, other, out),
    }
}

/// Map each row of a [`QueryResult`] through `f` (which reads cells by column
/// name via a [`RowGet`]); rows for which `f` yields `None` are dropped.
fn rows_to<T>(result: &QueryResult, f: impl Fn(RowGet<'_>) -> Option<T>) -> Vec<T> {
    result
        .rows
        .iter()
        .filter_map(|row| {
            f(RowGet {
                columns: &result.columns,
                row,
            })
        })
        .collect()
}

/// Reads a result row's cells by column name (order-independent).
struct RowGet<'a> {
    columns: &'a [String],
    row: &'a [Value],
}

impl RowGet<'_> {
    fn cell(&self, name: &str) -> Option<&Value> {
        let i = self.columns.iter().position(|c| c == name)?;
        self.row.get(i)
    }
    fn str(&self, name: &str) -> Option<&str> {
        self.cell(name).and_then(Value::as_str)
    }
    fn i64(&self, name: &str) -> Option<i64> {
        self.cell(name).and_then(Value::as_i64)
    }
}

/// The `MERGE` statement for an entity node (labelled by kind).
fn entity_merge(entity: &Entity) -> Statement {
    let node = NodeRef::entity(entity.kind, entity.id);
    merge_node(entity.workspace_id, &node, entity_props(entity))
}

/// Graph props for a note node — relationship-relevant metadata, not the body
/// (the markdown is embedded into Qdrant, not stored in the graph).
fn note_props(note: &Note) -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("title".into(), json!(note.title));
    props.insert("updated_at".into(), json!(rfc3339(note.updated_at)));
    props.insert("tags".into(), json!(note.tags));
    props
}

/// Graph props for an event node — schedule metadata for relationship queries,
/// not the full body (which is embedded into Qdrant, not stored in the graph).
fn event_props(event: &Event) -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("summary".into(), json!(event.summary));
    props.insert("starts_at".into(), json!(rfc3339(event.start)));
    props.insert("ends_at".into(), json!(rfc3339(event.end)));
    // Always set `location` (null when absent) so a re-projection of an event whose
    // location was *cleared* upstream overwrites the stale value — Neo4j's
    // `SET n += {location: null}` removes the property. (Omitting the key would
    // leave the old location on the node, since `+=` never removes absent keys.)
    props.insert("location".into(), json!(event.location));
    // Always set `labels` (the verbatim category strings) so a re-projection of
    // an event whose labels changed overwrites the stale value, as with `location`.
    props.insert("labels".into(), json!(event.labels));
    props
}

/// Graph props for an entity node.
fn entity_props(entity: &Entity) -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("display_name".into(), json!(entity.display_name));
    props.insert(
        "kind".into(),
        json!(NodeLabel::from_entity_kind(entity.kind).as_cypher()),
    );
    if !entity.aliases.is_empty() {
        props.insert("aliases".into(), json!(entity.aliases));
    }
    props
}

fn rfc3339(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{Author, EntityId, EntityKind, UserId};

    fn ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::from_u128(3))
    }

    fn sample_note() -> Note {
        Note {
            id: NoteId::from_uuid(uuid::Uuid::from_u128(10)),
            workspace_id: ws(),
            author: Author::User {
                id: UserId::from_uuid(uuid::Uuid::from_u128(99)),
            },
            title: "Weekly review".into(),
            markdown: "# notes\n- one".into(),
            tags: vec!["work".into(), "review".into()],
            updated_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    fn sample_entity(kind: EntityKind, n: u128) -> Entity {
        Entity {
            id: EntityId::from_uuid(uuid::Uuid::from_u128(n)),
            workspace_id: ws(),
            kind,
            display_name: "Ada".into(),
            aliases: vec!["A. Lovelace".into()],
        }
    }

    #[test]
    fn rejects_bad_base_url() {
        assert!(GraphStore::new("not a url").is_err());
        assert!(GraphStore::new("http://localhost:7474").is_ok());
    }

    #[test]
    fn commit_url_includes_database() {
        let store = GraphStore::new("http://localhost:7474/").unwrap();
        assert_eq!(
            store.commit_url(),
            "http://localhost:7474/db/neo4j/tx/commit"
        );
        let store = store.with_database("catalerum");
        assert_eq!(
            store.commit_url(),
            "http://localhost:7474/db/catalerum/tx/commit"
        );
    }

    #[test]
    fn with_auth_is_stored() {
        let store = GraphStore::new("http://localhost:7474")
            .unwrap()
            .with_auth("neo4j", "secret");
        assert_eq!(store.auth, Some(("neo4j".into(), "secret".into())));
    }

    #[test]
    fn note_props_carry_metadata_not_body() {
        let props = note_props(&sample_note());
        assert_eq!(props["title"], json!("Weekly review"));
        assert_eq!(props["tags"], json!(["work", "review"]));
        // updated_at is a stable RFC3339 string (UTC, second precision).
        assert_eq!(props["updated_at"], json!("2023-11-14T22:13:20Z"));
        assert!(props.get("markdown").is_none());
    }

    #[test]
    fn entity_props_carry_kind_label_and_aliases() {
        let props = entity_props(&sample_entity(EntityKind::Person, 1));
        assert_eq!(props["display_name"], json!("Ada"));
        assert_eq!(props["kind"], json!("Person"));
        assert_eq!(props["aliases"], json!(["A. Lovelace"]));
    }

    #[test]
    fn entity_props_omit_empty_aliases() {
        let mut e = sample_entity(EntityKind::Topic, 2);
        e.aliases.clear();
        let props = entity_props(&e);
        assert!(props.get("aliases").is_none());
        assert_eq!(props["kind"], json!("Topic"));
    }

    #[test]
    fn all_labels_covers_every_variant() {
        // A compile-time-ish guard: every label maps to a distinct index name.
        let names: std::collections::BTreeSet<_> = ALL_LABELS
            .iter()
            .map(|&l| ensure_index(l).statement)
            .collect();
        assert_eq!(names.len(), ALL_LABELS.len());
    }

    fn qr(columns: &[&str], rows: &[serde_json::Value]) -> QueryResult {
        QueryResult {
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            rows: rows.iter().map(|r| r.as_array().unwrap().clone()).collect(),
        }
    }

    #[test]
    fn parse_workspace_facts_filters_taxonomy_and_expands_props() {
        let nodes = qr(
            &["id", "labels", "props"],
            &[
                json!(["n1", ["Note"], {"title": "Hi", "tags": ["a", "b"], "workspace_id": "ws", "id": "n1"}]),
                json!(["t1", ["Topic"], {"display_name": "Planning"}]),
                json!(["x1", ["Weird"], {"k": "v"}]), // unknown label → dropped
            ],
        );
        let edges = qr(
            &["from", "type", "to"],
            &[
                json!(["n1", "REFERENCES", "t1"]),
                json!(["n1", "BOGUS", "t1"]), // unknown edge type → dropped
            ],
        );
        let f = parse_workspace_facts(&nodes, &edges, 100, 100);

        assert_eq!(
            f.nodes,
            vec![("n1".into(), "Note".into()), ("t1".into(), "Topic".into())]
        );
        assert_eq!(
            f.edges,
            vec![("n1".into(), "REFERENCES".into(), "t1".into())]
        );
        // Scalar + expanded list props; workspace_id/id excluded.
        assert!(f
            .props
            .contains(&("n1".into(), "title".into(), "Hi".into())));
        assert!(f.props.contains(&("n1".into(), "tags".into(), "a".into())));
        assert!(f.props.contains(&("n1".into(), "tags".into(), "b".into())));
        assert!(f
            .props
            .contains(&("t1".into(), "display_name".into(), "Planning".into())));
        assert!(!f
            .props
            .iter()
            .any(|(_, k, _)| k == "workspace_id" || k == "id"));
        assert!(!f.truncated);
    }

    #[test]
    fn parse_workspace_facts_flags_truncation_at_the_cap() {
        let nodes = qr(&["id", "labels", "props"], &[json!(["n1", ["Note"], {}])]);
        let edges = qr(&["from", "type", "to"], &[]);
        // A cap of 1 with 1 node row → possibly clipped → flagged.
        assert!(parse_workspace_facts(&nodes, &edges, 1, 50).truncated);
    }

    #[test]
    fn logic_schema_matches_graph_enums() {
        // The Datalog crate mirrors the closed §6.3 taxonomy; pin them together so
        // adding a NodeLabel/EdgeType without updating catalerum-logic fails loudly.
        let mut labels: Vec<&str> = ALL_LABELS.iter().map(|l| l.as_cypher()).collect();
        labels.sort_unstable();
        let mut logic_labels = catalerum_logic::LABELS.to_vec();
        logic_labels.sort_unstable();
        assert_eq!(labels, logic_labels);

        let mut edges: Vec<&str> = ALL_EDGES.iter().map(|e| e.as_cypher()).collect();
        edges.sort_unstable();
        let mut logic_edges = catalerum_logic::EDGE_TYPES.to_vec();
        logic_edges.sort_unstable();
        assert_eq!(edges, logic_edges);
    }
}
