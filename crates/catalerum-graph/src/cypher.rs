//! Parameterized Cypher: the [`Statement`] type, the idempotent statement
//! builders, and `/tx/commit` response parsing (SOUL §6.3).
//!
//! Every builder scopes its node patterns on `{workspace_id: $workspace_id}` so
//! a query can only ever touch one workspace's slice (SOUL §18). Labels and edge
//! types are interpolated from the closed [`NodeLabel`]/[`EdgeType`] enums
//! ([`crate::model`]); everything else is a `$parameter`.

use serde::Serialize;
use serde_json::{Map, Value};

use catalerum_core::WorkspaceId;

use crate::error::{GraphError, Neo4jError, Result};
use crate::model::{EdgeType, NodeLabel, NodeRef};

/// One parameterized Cypher statement, ready to post in a `/tx/commit`
/// transaction body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Statement {
    /// The Cypher text, with `$name` placeholders.
    pub statement: String,
    /// The bound parameters.
    pub parameters: Map<String, Value>,
}

impl Statement {
    fn new(statement: impl Into<String>, parameters: Map<String, Value>) -> Self {
        Self {
            statement: statement.into(),
            parameters,
        }
    }
}

fn params(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// Idempotently upsert a node keyed on `(workspace_id, id)`, overlaying `props`.
/// `props` must not contain `workspace_id`/`id` (the canonical key fields); the
/// typed writers in [`crate::store`] guarantee that.
pub fn merge_node(
    workspace_id: WorkspaceId,
    node: &NodeRef,
    props: Map<String, Value>,
) -> Statement {
    let statement = format!(
        "MERGE (n:{label} {{workspace_id: $workspace_id, id: $id}})\nSET n += $props",
        label = node.label.as_cypher()
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("id", Value::from(node.id.clone())),
            ("props", Value::Object(props)),
        ]),
    )
}

/// Idempotently upsert a directed relationship between two nodes. The endpoints
/// are `MERGE`d too (by `(workspace_id, id)`), so the edge writer never fails on
/// a not-yet-projected endpoint — it creates a thin placeholder that a later
/// [`merge_node`] enriches (SOUL §6.3: idempotent `MERGE` on stable ids).
pub fn merge_edge(
    workspace_id: WorkspaceId,
    from: &NodeRef,
    edge: EdgeType,
    to: &NodeRef,
) -> Statement {
    let statement = format!(
        "MERGE (a:{from_label} {{workspace_id: $workspace_id, id: $from_id}})\n\
         MERGE (b:{to_label} {{workspace_id: $workspace_id, id: $to_id}})\n\
         MERGE (a)-[r:{edge}]->(b)",
        from_label = from.label.as_cypher(),
        to_label = to.label.as_cypher(),
        edge = edge.as_cypher(),
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("from_id", Value::from(from.id.clone())),
            ("to_id", Value::from(to.id.clone())),
        ]),
    )
}

/// Idempotently upsert a **link** relationship ([`EdgeType::RelatesTo`]) keyed on
/// `(workspace_id, link_id)`, so each Postgres `links` row maps to exactly one
/// edge — two links between the same ordered pair (different labels) coexist as
/// distinct edges. Both endpoints are `MERGE`d as thin nodes a later
/// [`merge_node`] enriches (so the edge writer never fails on a not-yet-projected
/// endpoint, §6.3). `props` overlays the edge (label/note/updated_at) and must not
/// contain the key fields `workspace_id`/`link_id`.
pub fn merge_link_edge(
    workspace_id: WorkspaceId,
    link_id: &str,
    from: &NodeRef,
    to: &NodeRef,
    props: Map<String, Value>,
) -> Statement {
    let statement = format!(
        "MERGE (a:{from_label} {{workspace_id: $workspace_id, id: $from_id}})\n\
         MERGE (b:{to_label} {{workspace_id: $workspace_id, id: $to_id}})\n\
         MERGE (a)-[r:{edge} {{workspace_id: $workspace_id, link_id: $link_id}}]->(b)\n\
         SET r += $props",
        from_label = from.label.as_cypher(),
        to_label = to.label.as_cypher(),
        edge = EdgeType::RelatesTo.as_cypher(),
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("from_id", Value::from(from.id.clone())),
            ("to_id", Value::from(to.id.clone())),
            ("link_id", Value::from(link_id.to_owned())),
            ("props", Value::Object(props)),
        ]),
    )
}

/// Delete the link edge keyed on `(workspace_id, link_id)` — the purge basis for a
/// deleted `links` row. A no-op if absent; endpoint nodes are left in place (they
/// may back other rows), mirroring how [`delete_node`] leaves shared neighbours.
pub fn delete_link_edge(workspace_id: WorkspaceId, link_id: &str) -> Statement {
    let statement = format!(
        "MATCH ()-[r:{edge} {{workspace_id: $workspace_id, link_id: $link_id}}]->()\nDELETE r",
        edge = EdgeType::RelatesTo.as_cypher(),
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("link_id", Value::from(link_id.to_owned())),
        ]),
    )
}

/// Detach-delete one node (and its relationships) within a workspace. A no-op if
/// it does not exist.
pub fn delete_node(workspace_id: WorkspaceId, node: &NodeRef) -> Statement {
    let statement = format!(
        "MATCH (n:{label} {{workspace_id: $workspace_id, id: $id}})\nDETACH DELETE n",
        label = node.label.as_cypher()
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("id", Value::from(node.id.clone())),
        ]),
    )
}

/// Detach-delete **every** node in a workspace — the per-workspace rebuild basis
/// (SOUL §3.1/§6.3: the graph is a derived projection, droppable and
/// reprojectable from Postgres truth).
pub fn delete_workspace(workspace_id: WorkspaceId) -> Statement {
    Statement::new(
        "MATCH (n {workspace_id: $workspace_id})\nDETACH DELETE n",
        params([("workspace_id", Value::from(workspace_id.to_string()))]),
    )
}

/// Count all nodes in a workspace.
pub fn count_nodes(workspace_id: WorkspaceId) -> Statement {
    Statement::new(
        "MATCH (n {workspace_id: $workspace_id})\nRETURN count(n) AS count",
        params([("workspace_id", Value::from(workspace_id.to_string()))]),
    )
}

/// Whether a node keyed on `(workspace_id, id)` with `node`'s label exists — a
/// scoped existence probe (SOUL §29). Returns a single `count` column (`0` when
/// absent, `1` when present, since `(workspace_id, id)` is the unique MERGE key).
/// Read-only; run *before* a `MERGE` in the same transaction to report
/// created-vs-deduplicated for an idempotent entity upsert.
pub fn node_exists(workspace_id: WorkspaceId, node: &NodeRef) -> Statement {
    let statement = format!(
        "MATCH (n:{label} {{workspace_id: $workspace_id, id: $id}})\nRETURN count(n) AS count",
        label = node.label.as_cypher()
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("id", Value::from(node.id.clone())),
        ]),
    )
}

/// The stable ids of the nodes `node` points at over `edge` (one outbound hop),
/// scoped to the workspace. Returns a single `id` column.
pub fn out_neighbor_ids(workspace_id: WorkspaceId, node: &NodeRef, edge: EdgeType) -> Statement {
    let statement = format!(
        "MATCH (n:{label} {{workspace_id: $workspace_id, id: $id}})-[:{edge}]->(m)\n\
         RETURN m.id AS id",
        label = node.label.as_cypher(),
        edge = edge.as_cypher(),
    );
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("id", Value::from(node.id.clone())),
        ]),
    )
}

/// The `link_id`s of every `RELATES_TO` edge in a workspace (SOUL §21), scoped on
/// the edge's `workspace_id` property. Read-only; the verification read for the
/// link projection (there is no richer typed link-read helper yet).
pub fn relates_to_link_ids(workspace_id: WorkspaceId) -> Statement {
    Statement::new(
        "MATCH ()-[r:RELATES_TO {workspace_id: $workspace_id}]->()\nRETURN r.link_id AS id",
        params([("workspace_id", Value::from(workspace_id.to_string()))]),
    )
}

/// Notes that share at least one `:Topic` with the given note — a 2-hop
/// "what else is about this" query (SOUL §6.3/§6.5), workspace-scoped, ranked by
/// the number of shared topics. Read-only.
pub fn related_notes(workspace_id: WorkspaceId, note: &NodeRef, limit: i64) -> Statement {
    let statement = "\
        MATCH (n:Note {workspace_id: $workspace_id, id: $id})\n\
        MATCH (n)-[:REFERENCES]->(t:Topic)<-[:REFERENCES]-(m:Note {workspace_id: $workspace_id})\n\
        WHERE m.id <> $id\n\
        RETURN m.id AS id, m.title AS title, count(t) AS shared\n\
        ORDER BY shared DESC, m.id ASC\n\
        LIMIT $limit"
        .to_string();
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("id", Value::from(note.id.clone())),
            ("limit", Value::from(limit)),
        ]),
    )
}

/// Notes that `REFERENCES` a `:Topic` whose display name matches `topic`
/// (case-insensitive), workspace-scoped (SOUL §6.3/§6.5). Read-only.
pub fn notes_by_topic(workspace_id: WorkspaceId, topic: &str, limit: i64) -> Statement {
    let statement = "\
        MATCH (t:Topic {workspace_id: $workspace_id})<-[:REFERENCES]-(m:Note {workspace_id: $workspace_id})\n\
        WHERE toLower(t.display_name) = toLower($topic)\n\
        RETURN DISTINCT m.id AS id, m.title AS title\n\
        ORDER BY m.title ASC, m.id ASC\n\
        LIMIT $limit"
        .to_string();
    Statement::new(
        statement,
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("topic", Value::from(topic.to_string())),
            ("limit", Value::from(limit)),
        ]),
    )
}

/// Load one workspace's **nodes + properties** for the in-process Datalog
/// evaluator (SOUL §6.3/§18). Structurally scoped on `{workspace_id: $workspace_id}`
/// exactly like every other builder, so it can only ever read the caller's slice —
/// there is no query text from the user in this statement. `$cap` bounds the rows
/// Neo4j materializes. Read-only. Returns `id`, the node's `labels`, and its full
/// `props` map (the caller filters to the closed taxonomy and expands list props).
pub fn load_nodes(workspace_id: WorkspaceId, cap: i64) -> Statement {
    Statement::new(
        "MATCH (n {workspace_id: $workspace_id})\n\
         RETURN n.id AS id, labels(n) AS labels, properties(n) AS props\n\
         LIMIT $cap",
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("cap", Value::from(cap)),
        ]),
    )
}

/// Load one workspace's **edges** for the Datalog evaluator (SOUL §6.3/§18). Both
/// endpoints are scoped to `$workspace_id`, so a cross-workspace edge can never be
/// returned. `$cap` bounds the rows. Read-only. Returns `from`, the relationship
/// `type`, and `to` (stable ids).
pub fn load_edges(workspace_id: WorkspaceId, cap: i64) -> Statement {
    Statement::new(
        "MATCH (a {workspace_id: $workspace_id})-[r]->(b {workspace_id: $workspace_id})\n\
         RETURN a.id AS from, type(r) AS type, b.id AS to\n\
         LIMIT $cap",
        params([
            ("workspace_id", Value::from(workspace_id.to_string())),
            ("cap", Value::from(cap)),
        ]),
    )
}

/// Idempotently create a range index on `(workspace_id, id)` for `label`, which
/// backs the `MERGE` key. `IF NOT EXISTS` makes re-running a no-op. The index
/// name is derived from the (closed) label, so it is a safe identifier.
pub fn ensure_index(label: NodeLabel) -> Statement {
    let l = label.as_cypher();
    let statement = format!(
        "CREATE INDEX catalerum_{lower}_ws_id IF NOT EXISTS FOR (n:{l}) ON (n.workspace_id, n.id)",
        lower = l.to_ascii_lowercase(),
    );
    Statement::new(statement, Map::new())
}

/// The result of one statement: its `columns` and the `rows` (each a vector of
/// JSON values aligned to `columns`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    /// The `RETURN` column names, in order.
    pub columns: Vec<String>,
    /// One entry per returned record; each aligned to `columns`.
    pub rows: Vec<Vec<Value>>,
}

impl QueryResult {
    /// Pull a single scalar column out of every row by name (e.g. `"id"`), as
    /// strings. Rows missing the column or holding a non-string are skipped.
    #[must_use]
    pub fn string_column(&self, name: &str) -> Vec<String> {
        let Some(idx) = self.columns.iter().position(|c| c == name) else {
            return Vec::new();
        };
        self.rows
            .iter()
            .filter_map(|r| r.get(idx).and_then(Value::as_str).map(str::to_owned))
            .collect()
    }

    /// The first cell of the first row as an `i64`, if any (for `count(n)`-style
    /// single-value queries).
    #[must_use]
    pub fn scalar_i64(&self) -> Option<i64> {
        self.rows.first()?.first()?.as_i64()
    }
}

/// Parse a Neo4j `/tx/commit` JSON body into per-statement [`QueryResult`]s.
///
/// Neo4j reports Cypher failures in `errors` with an HTTP `200`, so a non-empty
/// `errors` array becomes [`GraphError::Cypher`] — the transport succeeded but
/// the transaction did not.
pub fn parse_commit_response(body: &Value) -> Result<Vec<QueryResult>> {
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let parsed: Vec<Neo4jError> = errors
                .iter()
                .map(|e| serde_json::from_value(e.clone()))
                .collect::<std::result::Result<_, _>>()
                .map_err(|e| GraphError::Malformed(format!("undecodable error entry: {e}")))?;
            return Err(GraphError::Cypher(parsed));
        }
    }

    let results = body
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| GraphError::Malformed("response missing `results` array".into()))?;

    results.iter().map(parse_one_result).collect()
}

fn parse_one_result(result: &Value) -> Result<QueryResult> {
    let columns = result
        .get("columns")
        .and_then(Value::as_array)
        .map(|cols| {
            cols.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let rows = result
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                .filter_map(|d| d.get("row").and_then(Value::as_array).cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(QueryResult { columns, rows })
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{EntityId, EntityKind, NoteId};
    use serde_json::json;

    fn ws() -> WorkspaceId {
        WorkspaceId::from_uuid(uuid::Uuid::from_u128(7))
    }

    #[test]
    fn merge_node_scopes_on_workspace_and_id() {
        let note = NodeRef::note(NoteId::from_uuid(uuid::Uuid::from_u128(1)));
        let mut props = Map::new();
        props.insert("title".into(), json!("Groceries"));
        let s = merge_node(ws(), &note, props);

        assert!(s
            .statement
            .starts_with("MERGE (n:Note {workspace_id: $workspace_id, id: $id})"));
        assert!(s.statement.contains("SET n += $props"));
        assert_eq!(s.parameters["workspace_id"], json!(ws().to_string()));
        assert_eq!(s.parameters["id"], json!(note.id));
        assert_eq!(s.parameters["props"]["title"], json!("Groceries"));
    }

    #[test]
    fn merge_edge_merges_both_endpoints_and_relationship() {
        let note = NodeRef::note(NoteId::from_uuid(uuid::Uuid::from_u128(1)));
        let person = NodeRef::entity(
            EntityKind::Person,
            EntityId::from_uuid(uuid::Uuid::from_u128(2)),
        );
        let s = merge_edge(ws(), &note, EdgeType::References, &person);

        assert!(s
            .statement
            .contains("MERGE (a:Note {workspace_id: $workspace_id, id: $from_id})"));
        assert!(s
            .statement
            .contains("MERGE (b:Person {workspace_id: $workspace_id, id: $to_id})"));
        assert!(s.statement.contains("MERGE (a)-[r:REFERENCES]->(b)"));
        assert_eq!(s.parameters["from_id"], json!(note.id));
        assert_eq!(s.parameters["to_id"], json!(person.id));
    }

    #[test]
    fn merge_link_edge_keys_on_link_id_and_overlays_props() {
        let note = NodeRef::note(NoteId::from_uuid(uuid::Uuid::from_u128(1)));
        let event = NodeRef::event(catalerum_core::EventId::from_uuid(uuid::Uuid::from_u128(2)));
        let mut props = Map::new();
        props.insert("label".into(), json!("follow-up"));
        let s = merge_link_edge(ws(), "link-42", &note, &event, props);

        assert!(s
            .statement
            .contains("MERGE (a:Note {workspace_id: $workspace_id, id: $from_id})"));
        assert!(s
            .statement
            .contains("MERGE (b:Event {workspace_id: $workspace_id, id: $to_id})"));
        // Keyed on (workspace_id, link_id) so distinct rows stay distinct edges.
        assert!(s.statement.contains(
            "MERGE (a)-[r:RELATES_TO {workspace_id: $workspace_id, link_id: $link_id}]->(b)"
        ));
        assert!(s.statement.contains("SET r += $props"));
        assert_eq!(s.parameters["link_id"], json!("link-42"));
        assert_eq!(s.parameters["from_id"], json!(note.id));
        assert_eq!(s.parameters["to_id"], json!(event.id));
        assert_eq!(s.parameters["props"]["label"], json!("follow-up"));
    }

    #[test]
    fn delete_link_edge_matches_only_the_keyed_relationship() {
        let s = delete_link_edge(ws(), "link-42");
        assert_eq!(
            s.statement,
            "MATCH ()-[r:RELATES_TO {workspace_id: $workspace_id, link_id: $link_id}]->()\nDELETE r"
        );
        assert_eq!(s.parameters["workspace_id"], json!(ws().to_string()));
        assert_eq!(s.parameters["link_id"], json!("link-42"));
    }

    #[test]
    fn delete_workspace_targets_only_the_workspace() {
        let s = delete_workspace(ws());
        assert_eq!(
            s.statement,
            "MATCH (n {workspace_id: $workspace_id})\nDETACH DELETE n"
        );
        assert_eq!(s.parameters.len(), 1);
        assert_eq!(s.parameters["workspace_id"], json!(ws().to_string()));
    }

    #[test]
    fn ensure_index_name_and_label_derive_from_the_closed_enum() {
        let s = ensure_index(NodeLabel::Note);
        assert_eq!(
            s.statement,
            "CREATE INDEX catalerum_note_ws_id IF NOT EXISTS FOR (n:Note) ON (n.workspace_id, n.id)"
        );
        assert!(s.parameters.is_empty());
    }

    #[test]
    fn node_exists_is_scoped_read_only_count_probe() {
        let person = NodeRef::entity(
            EntityKind::Person,
            EntityId::from_uuid(uuid::Uuid::from_u128(9)),
        );
        let s = node_exists(ws(), &person);
        assert!(s
            .statement
            .starts_with("MATCH (n:Person {workspace_id: $workspace_id, id: $id})"));
        assert!(s.statement.contains("RETURN count(n) AS count"));
        // Read-only: no write clause.
        let upper = s.statement.to_uppercase();
        assert!(!upper.contains("CREATE") && !upper.contains("MERGE") && !upper.contains("DELETE"));
        assert_eq!(s.parameters["workspace_id"], json!(ws().to_string()));
        assert_eq!(s.parameters["id"], json!(person.id));
    }

    #[test]
    fn out_neighbor_ids_builds_single_hop_with_id_projection() {
        let note = NodeRef::note(NoteId::from_uuid(uuid::Uuid::from_u128(1)));
        let s = out_neighbor_ids(ws(), &note, EdgeType::References);
        assert!(s.statement.contains("-[:REFERENCES]->(m)"));
        assert!(s.statement.contains("RETURN m.id AS id"));
    }

    #[test]
    fn related_notes_is_read_only_workspace_scoped_two_hop() {
        let note = NodeRef::note(NoteId::from_uuid(uuid::Uuid::from_u128(1)));
        let s = related_notes(ws(), &note, 5);
        // Two REFERENCES hops through a shared Topic, both Note ends scoped.
        assert!(s.statement.contains(
            "(n)-[:REFERENCES]->(t:Topic)<-[:REFERENCES]-(m:Note {workspace_id: $workspace_id})"
        ));
        assert!(s.statement.contains("WHERE m.id <> $id"));
        assert!(s.statement.contains("count(t) AS shared"));
        assert!(s.statement.contains("LIMIT $limit"));
        // Read-only: no write clauses.
        let upper = s.statement.to_uppercase();
        assert!(!upper.contains("CREATE") && !upper.contains("MERGE") && !upper.contains("DELETE"));
        assert_eq!(s.parameters["workspace_id"], json!(ws().to_string()));
        assert_eq!(s.parameters["limit"], json!(5));
    }

    #[test]
    fn notes_by_topic_matches_display_name_case_insensitively_and_scopes() {
        let s = notes_by_topic(ws(), "Planning", 10);
        assert!(s.statement.contains("(t:Topic {workspace_id: $workspace_id})<-[:REFERENCES]-(m:Note {workspace_id: $workspace_id})"));
        assert!(s
            .statement
            .contains("toLower(t.display_name) = toLower($topic)"));
        assert!(s.statement.contains("RETURN DISTINCT m.id AS id"));
        assert_eq!(s.parameters["topic"], json!("Planning"));
        assert_eq!(s.parameters["limit"], json!(10));
    }

    #[test]
    fn load_nodes_and_edges_are_scoped_read_only_and_capped() {
        let n = load_nodes(ws(), 500);
        assert!(n
            .statement
            .contains("MATCH (n {workspace_id: $workspace_id})"));
        assert!(n
            .statement
            .contains("RETURN n.id AS id, labels(n) AS labels, properties(n) AS props"));
        assert!(n.statement.contains("LIMIT $cap"));
        assert_eq!(n.parameters["workspace_id"], json!(ws().to_string()));
        assert_eq!(n.parameters["cap"], json!(500));

        let e = load_edges(ws(), 250);
        // Both endpoints scoped → no cross-workspace edge can be returned (§18).
        assert!(e
            .statement
            .contains("(a {workspace_id: $workspace_id})-[r]->(b {workspace_id: $workspace_id})"));
        assert!(e
            .statement
            .contains("RETURN a.id AS from, type(r) AS type, b.id AS to"));
        assert_eq!(e.parameters["cap"], json!(250));

        // Read-only: neither builder carries a write clause.
        for s in [&n.statement, &e.statement] {
            let upper = s.to_uppercase();
            assert!(
                !upper.contains("CREATE") && !upper.contains("MERGE") && !upper.contains("DELETE")
            );
        }
    }

    #[test]
    fn parse_commit_response_extracts_columns_and_rows() {
        let body = json!({
            "results": [{
                "columns": ["id"],
                "data": [
                    { "row": ["a"], "meta": [null] },
                    { "row": ["b"], "meta": [null] }
                ]
            }],
            "errors": []
        });
        let results = parse_commit_response(&body).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].columns, vec!["id".to_string()]);
        assert_eq!(
            results[0].string_column("id"),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn parse_commit_response_surfaces_cypher_errors() {
        let body = json!({
            "results": [],
            "errors": [
                { "code": "Neo.ClientError.Statement.SyntaxError", "message": "boom" }
            ]
        });
        match parse_commit_response(&body) {
            Err(GraphError::Cypher(errs)) => {
                assert_eq!(errs.len(), 1);
                assert_eq!(errs[0].code, "Neo.ClientError.Statement.SyntaxError");
                assert_eq!(errs[0].message, "boom");
            }
            other => panic!("expected Cypher error, got {other:?}"),
        }
    }

    #[test]
    fn parse_commit_response_scalar_count() {
        let body = json!({
            "results": [{ "columns": ["count"], "data": [ { "row": [42], "meta": [null] } ] }],
            "errors": []
        });
        let results = parse_commit_response(&body).unwrap();
        assert_eq!(results[0].scalar_i64(), Some(42));
    }

    #[test]
    fn parse_commit_response_requires_results_array() {
        assert!(matches!(
            parse_commit_response(&json!({ "errors": [] })),
            Err(GraphError::Malformed(_))
        ));
    }
}
