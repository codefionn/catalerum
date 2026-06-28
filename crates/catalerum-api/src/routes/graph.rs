//! Graph query REST (SOUL §6.3/§12) — read-only, workspace-scoped **Datalog** over
//! the derived graph, the HTTP sibling of the typed `query_graph` LLM tool (§7).
//!
//! `POST /graph/query` runs an operator-authored Datalog program against the
//! configured graph ([`AppState::graph`](crate::state::AppState::graph); `404` when
//! `[neo4j]` is off). The program is parsed + validated by
//! [`catalerum_logic`] (a syntax/safety error is a `400`), then evaluated
//! **in-process** over facts loaded for the caller's workspace only. Scope is
//! structural: the language cannot name a workspace, cannot write, and no query text
//! ever reaches Neo4j — so cross-tenant reach and injection are impossible by
//! construction (§18), replacing the old raw-Cypher path and its heuristic validator.
//! The request is `graph:query`-gated (every role holds it). Returned rows are capped
//! at [`MAX_GRAPH_QUERY_ROWS`] (a scoped-but-broad read can't dump a whole workspace
//! slice in one response, §18/§19) — a capped result (or a fact-load that hit the
//! workspace cap) is flagged `truncated`.

use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_graph::{WorkspaceFacts, MAX_WORKSPACE_EDGES, MAX_WORKSPACE_NODES};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the graph-query route.
pub fn router() -> Router<AppState> {
    Router::new().route("/graph/query", post(query))
}

/// Body for `POST /graph/query`.
#[derive(Debug, Deserialize)]
pub struct GraphQueryRequest {
    /// The Datalog program to run — optional rules `head :- body.` plus one goal
    /// `?- body.` over `node`/`edge`/`prop` (and label/edge shorthand). Scope is
    /// implicit; the language cannot name a workspace.
    pub query: String,
}

/// Hard cap on rows returned to the explorer client (§18/§19 blast-radius bound).
/// The derived graph holds one workspace's whole slice, so a scoped-but-broad goal
/// (`?- node(N, L).`) could otherwise stream the entire workspace out in one
/// response. We cap what we hand back and flag it (`truncated`) so a partial result
/// can never masquerade as a complete one.
const MAX_GRAPH_QUERY_ROWS: usize = 1_000;

/// Wall-clock backstop for evaluating one program (the evaluator also enforces its
/// own deadline; evaluation is pure and structurally terminating, SOUL §6.3).
const EVAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Truncate `rows` to at most `max`, reporting whether anything was dropped. Pure,
/// so the cap is unit-testable without a live graph.
fn cap_rows(
    mut rows: Vec<Vec<serde_json::Value>>,
    max: usize,
) -> (Vec<Vec<serde_json::Value>>, bool) {
    let truncated = rows.len() > max;
    if truncated {
        rows.truncate(max);
    }
    (rows, truncated)
}

/// Build a `catalerum_logic::Facts` EDB from a loaded [`WorkspaceFacts`] slice.
fn facts_from(wf: &WorkspaceFacts) -> catalerum_logic::Facts {
    let mut facts = catalerum_logic::Facts::new();
    for (id, label) in &wf.nodes {
        facts.node(id.as_str(), label.as_str());
    }
    for (from, ty, to) in &wf.edges {
        facts.edge(from.as_str(), ty.as_str(), to.as_str());
    }
    for (id, key, value) in &wf.props {
        facts.prop(id.as_str(), key.as_str(), value.as_str());
    }
    facts
}

/// Response for `POST /graph/query` — the column names + the returned records (each
/// row aligned to `columns`, cells JSON strings). Mirrors the evaluator's
/// `EvalOutput`.
#[derive(Debug, Serialize)]
pub struct GraphQueryResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    /// Whether the result is partial — either `rows` was capped at
    /// [`MAX_GRAPH_QUERY_ROWS`] or the workspace fact-load hit its cap. Narrow the
    /// query to see the rest.
    pub truncated: bool,
}

async fn query(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<GraphQueryRequest>,
) -> ApiResult<Json<GraphQueryResponse>> {
    let p = auth.principal();
    auth.require(Action::Query, "graph")?;
    // `404` when no graph backend is configured (mirrors `/storage`).
    let graph = state.graph().ok_or(ApiError::NotFound)?;

    let src = body.query.trim();
    if src.is_empty() {
        return Err(ApiError::bad_request("`query` is required"));
    }
    // Parse + validate the Datalog (syntax + safety). An invalid or unsafe program
    // is rejected `400`, never run — this replaces the old §18/§19 Cypher guard.
    let program = catalerum_logic::parse(src)
        .map_err(|e| ApiError::bad_request(format!("invalid query: {e}")))?;

    // Load only this workspace's facts (structurally scoped — no user text reaches
    // Neo4j, §18), then evaluate the program in-process over them.
    let loaded = graph
        .load_workspace_facts(p.workspace_id, MAX_WORKSPACE_NODES, MAX_WORKSPACE_EDGES)
        .await
        .map_err(|e| ApiError::internal(format!("graph load failed: {e}")))?;
    let load_truncated = loaded.truncated;
    let facts = facts_from(&loaded);

    // Evaluate off the async runtime (pure, sync, bounded).
    let out = tokio::task::spawn_blocking(move || {
        catalerum_logic::eval(
            &program,
            &facts,
            &catalerum_logic::EvalLimits::with_deadline(EVAL_TIMEOUT),
        )
    })
    .await
    .map_err(|e| ApiError::internal(format!("query task panicked: {e}")))?
    .map_err(|e| ApiError::bad_request(format!("query failed: {e}")))?;

    // Bound the response (§18/§19): a scoped-but-broad read can't dump the whole
    // workspace slice to the client in one shot; a capped/partial result is flagged.
    let (rows, truncated) = cap_rows(out.rows, MAX_GRAPH_QUERY_ROWS);
    Ok(Json(GraphQueryResponse {
        columns: out.columns,
        rows,
        truncated: truncated || load_truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_decodes_query() {
        let r: GraphQueryRequest =
            serde_json::from_str(r#"{"query":"?- note(N), prop(N, \"title\", T)."}"#).unwrap();
        assert!(r.query.contains("?- note(N)"));
    }

    #[test]
    fn response_serializes_columns_and_rows() {
        let resp = GraphQueryResponse {
            columns: vec!["name".to_string(), "count".to_string()],
            rows: vec![vec![serde_json::json!("Ada"), serde_json::json!("3")]],
            truncated: false,
        };
        let j = serde_json::to_value(&resp).unwrap();
        assert_eq!(j["columns"][0], "name");
        assert_eq!(j["rows"][0][0], "Ada");
        assert_eq!(j["truncated"], false);
    }

    #[test]
    fn cap_rows_truncates_and_flags_only_when_over_the_cap() {
        let row = || vec![serde_json::json!(1)];
        // At/under the cap: untouched, not flagged.
        let (rows, truncated) = cap_rows(vec![row(), row()], 2);
        assert_eq!(rows.len(), 2);
        assert!(!truncated);
        // Over the cap: truncated to `max` and flagged.
        let (rows, truncated) = cap_rows(vec![row(), row(), row()], 2);
        assert_eq!(rows.len(), 2);
        assert!(truncated);
        // Empty stays empty, never flagged.
        let (rows, truncated) = cap_rows(Vec::new(), MAX_GRAPH_QUERY_ROWS);
        assert!(rows.is_empty());
        assert!(!truncated);
    }
}
