//! Automations REST (SOUL §11) — author and manage a workspace's automations.
//!
//! Workspace-scoped to the authenticated principal (SOUL §18): the client never
//! names a workspace, so cross-workspace reach is impossible by construction.
//!
//! **Capability-gated (SOUL §19)** via the shared [`Auth::require`] gate, like the
//! other data REST surfaces. Reads require `automation:read` (every role); writes
//! — create / replace / delete / enable — require `automation:write` (a Viewer is
//! `403 Forbidden`, deny-by-default).
//!
//! A create/replace **validates the typed spec** ([`catalerum_automation`]) before
//! persisting: an automation with an unknown trigger/action `kind`, a missing
//! required field, or no triggers/actions is rejected `400` rather than stored as
//! a definition that could never run. `grant_id` is **not** accepted from the
//! client — the §19 grant an automation runs under is assigned by the policy
//! engine (a later slice), never claimed over this surface.
//!
//! Routes:
//! - `GET    /automations`               list this workspace's automations
//! - `POST   /automations`               create (`201`; `409` dup name; `400` bad spec)
//! - `GET    /automations/node-types`     the node-type catalog (docs for authoring)
//! - `GET    /automations/node-types/search?q=…&limit=N` semantic node-type search
//! - `GET    /automations/{name}`        fetch one by name (`404` if absent)
//! - `PUT    /automations/{name}`        create-or-replace the named automation
//! - `DELETE /automations/{name}`        delete (`204`; `404` if absent)
//! - `POST   /automations/{name}/enabled` pause/resume — `{ "enabled": bool }`
//! - `POST   /automations/{name}/collect` "collect now" — one immediate poll of a
//!   Collect-headed automation (`202`; `404` if absent; `400` if not a collect flow)
//! - `GET    /automations/{name}/runs`    recent runs, newest first (`?limit=N`)
//! - `GET    /automations/{name}/runs/{run_id}` one run + its steps (the audit trail)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use catalerum_automation::graph::Graph;
use catalerum_automation::{NodeDoc, Trigger};
use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::id::AutomationRunId;
use catalerum_core::model::{AutomationRun, AutomationStep};
use catalerum_core::Automation;
use catalerum_store::NewAutomation;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the automation routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/automations", get(list).post(create))
        // The static `node-types` segments are registered alongside `{name}`; axum's
        // router prefers a literal path over a `{name}` capture, so these resolve
        // before `/automations/{name}` (a real automation can't be named "node-types").
        .route("/automations/node-types", get(list_node_types))
        .route("/automations/node-types/search", get(search_node_types))
        .route(
            "/automations/{name}",
            get(get_one).put(upsert).delete(delete_automation),
        )
        .route("/automations/{name}/enabled", post(set_enabled))
        .route("/automations/{name}/collect", post(collect_now))
        .route("/automations/{name}/runs", get(list_runs))
        .route("/automations/{name}/runs/{run_id}", get(get_run))
}

/// Body for `POST /automations`. Only `name` is required; `enabled` defaults to
/// `true`. `grant_id` is intentionally absent (assigned by the policy engine).
#[derive(Debug, Deserialize)]
pub struct CreateAutomation {
    /// Unique (per workspace) automation name.
    pub name: String,
    /// Whether the automation is active. Defaults to `true`.
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    /// Trigger specs (`{ "kind": "schedule", "cron": "…" }`, …).
    #[serde(default)]
    pub triggers: Vec<Value>,
    /// Optional condition predicate.
    #[serde(default)]
    pub condition: Option<Value>,
    /// Ordered typed action specs.
    #[serde(default)]
    pub actions: Vec<Value>,
    /// The full original authoring spec (round-tripped verbatim).
    #[serde(default)]
    pub spec: Option<Value>,
}

/// Body for `PUT /automations/{name}` — a full replacement; the name comes from
/// the path (create-or-replace semantics).
#[derive(Debug, Deserialize)]
pub struct UpdateAutomation {
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    #[serde(default)]
    pub triggers: Vec<Value>,
    #[serde(default)]
    pub condition: Option<Value>,
    #[serde(default)]
    pub actions: Vec<Value>,
    #[serde(default)]
    pub spec: Option<Value>,
}

/// Body for `POST /automations/{name}/enabled`.
#[derive(Debug, Deserialize)]
pub struct SetEnabled {
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

/// Validate + normalize an automation name (the per-workspace unique key).
fn clean_name(raw: &str) -> ApiResult<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("automation name must not be empty"));
    }
    Ok(name.to_string())
}

/// Reject a malformed spec (`400`) before persisting — unknown trigger/action
/// `kind`, a missing required field, or empty trigger/action lists (SOUL §11).
fn validate_spec(
    triggers: &[Value],
    condition: Option<&Value>,
    actions: &[Value],
) -> ApiResult<()> {
    catalerum_automation::AutomationSpec::from_json(triggers, condition, actions)
        .map(|_| ())
        .map_err(|e| ApiError::bad_request(format!("invalid automation: {e}")))
}

/// The capabilities an automation's **collect** triggers demand of their author
/// (SOUL §11/§19): one `email:read@<connection>` / `calendar:read@<connection>`
/// per `CollectEmail`/`CollectCalendar` trigger — the same capability the poll
/// enforces at run time against the automation's grant (the pull is a provider
/// *read*; landing items locally stays gated `*:write` at the write nodes).
/// Unparseable triggers and collect triggers with no connection are skipped here
/// — spec validation rejects those shapes separately.
fn collect_capabilities(triggers: &[Value]) -> Vec<Capability> {
    triggers
        .iter()
        .filter_map(|t| {
            let trigger = serde_json::from_value::<Trigger>(t.clone()).ok()?;
            let domain = match &trigger {
                Trigger::CollectEmail { .. } => "email",
                Trigger::CollectCalendar { .. } => "calendar",
                Trigger::CollectSql { .. } => "db",
                _ => return None,
            };
            let connection = trigger.collect_connection()?.trim().to_string();
            if connection.is_empty() {
                return None;
            }
            Some(Capability::new(
                Action::Read,
                Resource::new(domain, connection),
            ))
        })
        .collect()
}

/// Authoring-time collect-capability gate (SOUL §11/§19): creating/replacing an
/// automation headed by a Collect trigger requires the **token's** effective
/// authority to cover pulling that connection. `caps` is [`Auth::capabilities`] —
/// the grant's capabilities when the bearer is **grant-scoped**, else the role's
/// base set — so the check is grant-aware: a token minted `{automation:write}` but
/// lacking `email:read@conn` can no longer author a `CollectEmail` for that
/// connection (REST-authored automations store `grant_id: None`, so the run-time
/// `authorize_collect` is inert and the poll would otherwise run under default
/// Member authority — a grant bypass). For a non-scoped token this is unchanged:
/// `capabilities()` returns the role's base set, and Member+ holds domain-wide
/// `email:read`/`calendar:read`, so authoring a collector still passes.
///
/// The match mirrors [`Auth::require`] for a grant-scoped token: a held capability
/// [covers](Capability::covers) the request iff its action + resource subsume it —
/// so a domain-wide `email:read` **or** the exact `email:read@conn` selector both
/// satisfy `email:read@conn`.
fn require_collect_authority(caps: &[Capability], triggers: &[Value]) -> ApiResult<()> {
    for requested in collect_capabilities(triggers) {
        if !caps.iter().any(|held| held.covers(&requested)) {
            return Err(ApiError::Forbidden(format!(
                "authoring a collect automation for connection `{}` requires {}:read@{} authority",
                requested.resource.selector.as_deref().unwrap_or("?"),
                requested.resource.domain,
                requested.resource.selector.as_deref().unwrap_or("?"),
            )));
        }
    }
    Ok(())
}

/// Compile a node-graph automation's stored `triggers` column from its `spec`
/// (SOUL §11 Phase A). When `spec` carries a `"graph"` key, the request is a
/// **graph** automation: parse + [`Graph::validate`] it (`400` on failure), then
/// return the **compiled** triggers — each graph Trigger node's [`Trigger`] as JSON
/// — so the existing dispatch matching (which keys on the `triggers` column) still
/// fires the automation. The graph itself is round-tripped verbatim in `spec`; the
/// `triggers` column is its compiled dispatch shadow (the executor runs the graph,
/// not these triggers). Returns:
/// - `Some(Ok(triggers))` — a valid graph; persist these compiled triggers.
/// - `Some(Err(..))` — a present-but-invalid graph (a `400`).
/// - `None` — no graph (a legacy linear automation; persist the request as-is).
///
/// [`Trigger`]: catalerum_automation::Trigger
fn compile_graph_triggers(spec: Option<&Value>) -> Option<ApiResult<Vec<Value>>> {
    let parsed = Graph::from_spec(spec)?;
    Some((|| {
        let graph =
            parsed.map_err(|e| ApiError::bad_request(format!("invalid automation graph: {e}")))?;
        graph
            .validate()
            .map_err(|e| ApiError::bad_request(format!("invalid automation graph: {e}")))?;
        // The graph compiled fine; turn each Trigger node into a stored trigger JSON
        // value. `serde_json::to_value` of a `Trigger` cannot fail (it is a plain
        // tagged enum), so the unwrap is total.
        Ok(graph
            .trigger_specs()
            .iter()
            .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
            .collect())
    })())
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Automation>>> {
    let p = auth.principal();
    auth.require(Action::Read, "automation")?;
    let automations = state
        .store()
        .automations()
        .list_by_workspace(p.workspace_id)
        .await?;
    Ok(Json(automations))
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(mut body): Json<CreateAutomation>,
) -> ApiResult<(StatusCode, Json<Automation>)> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let name = clean_name(&body.name)?;
    // A graph automation (its `spec` carries a `"graph"`) compiles its dispatch
    // `triggers` from the graph and skips the linear `validate_spec` (its `actions`
    // are nodes, not the legacy column). A legacy automation is validated + stored
    // EXACTLY as before.
    let triggers = match compile_graph_triggers(body.spec.as_ref()) {
        Some(compiled) => compiled?,
        None => {
            validate_spec(&body.triggers, body.condition.as_ref(), &body.actions)?;
            body.triggers
        }
    };
    // A Collect-headed automation requires the token's **effective** authority to
    // cover the collect capability for its connection (SOUL §11/§19) — checked on
    // the *compiled* triggers so graph and legacy shapes gate identically, and
    // grant-aware (`auth.capabilities()`) so a scoped token can't author a
    // collector its grant omits.
    require_collect_authority(&auth.capabilities(), &triggers)?;
    // And the connection itself must exist (SOUL §10/§28): a placeholder id would
    // save fine and then fail forever at poll time.
    crate::connection_status::validate_collect_connections(
        state.store(),
        p.workspace_id,
        &triggers,
    )
    .await
    .map_err(ApiError::bad_request)?;
    // Auto-place origin-defaulted graph nodes (a spec authored without canvas
    // positions — e.g. over the API) so the visual editor doesn't open them
    // stacked at the origin (SOUL §11). The visual editor sends explicit
    // positions, so its writes are a no-op here.
    if let Some(spec) = body.spec.as_mut() {
        catalerum_automation::apply_auto_layout(spec);
    }
    let spec = NewAutomation {
        name,
        enabled: body.enabled,
        triggers,
        condition: body.condition,
        actions: body.actions,
        spec: body.spec,
        grant_id: None,
    };
    let automation = state
        .store()
        .automations()
        .create(p.workspace_id, &spec)
        .await?;
    Ok((StatusCode::CREATED, Json(automation)))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<Json<Automation>> {
    let p = auth.principal();
    auth.require(Action::Read, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(automation))
}

async fn upsert(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(mut body): Json<UpdateAutomation>,
) -> ApiResult<Json<Automation>> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let name = clean_name(&name)?;
    // As in `create`: a graph automation compiles its dispatch `triggers` from the
    // graph (skipping the linear validation); a legacy automation is unchanged.
    let triggers = match compile_graph_triggers(body.spec.as_ref()) {
        Some(compiled) => compiled?,
        None => {
            validate_spec(&body.triggers, body.condition.as_ref(), &body.actions)?;
            body.triggers
        }
    };
    // Same authoring gates as `create` (SOUL §11/§19 authority + §10/§28
    // connection existence), on the compiled triggers.
    require_collect_authority(&auth.capabilities(), &triggers)?;
    crate::connection_status::validate_collect_connections(
        state.store(),
        p.workspace_id,
        &triggers,
    )
    .await
    .map_err(ApiError::bad_request)?;
    // As in `create`: auto-place origin-defaulted graph nodes (SOUL §11).
    if let Some(spec) = body.spec.as_mut() {
        catalerum_automation::apply_auto_layout(spec);
    }
    let spec = NewAutomation {
        name,
        enabled: body.enabled,
        triggers,
        condition: body.condition,
        actions: body.actions,
        spec: body.spec,
        // A replace currently clears any grant (upsert overwrites every column).
        // Harmless today (no grants exist); whether a definition-replace should
        // *preserve* the run-grant is decided by the §19 policy-engine slice.
        grant_id: None,
    };
    let automation = state
        .store()
        .automations()
        .upsert_by_name(p.workspace_id, &spec)
        .await?;
    Ok(Json(automation))
}

async fn delete_automation(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    state
        .store()
        .automations()
        .delete(p.workspace_id, automation.id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_enabled(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Json(body): Json<SetEnabled>,
) -> ApiResult<Json<Automation>> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    let updated = state
        .store()
        .automations()
        .set_enabled(p.workspace_id, automation.id, body.enabled)
        .await?;
    Ok(Json(updated))
}

/// The result of a "collect now" — the enqueued collect job id.
#[derive(Debug, Serialize)]
pub struct CollectNowResult {
    /// The durable collect-job id enqueued for the poll (one job per call).
    pub job: uuid::Uuid,
}

/// `POST /automations/{name}/collect` — "collect now": enqueue **one immediate poll**
/// of a Collect-headed automation (SOUL §29), bypassing the trigger's `every` cadence.
///
/// This is the manual counterpart to the scheduler's cadence-driven poll: a collect
/// automation's "run" is a *poll* that fans out one `AutomationRun` per new external
/// item — not a bare run of its actions (a `WriteEmail` with no trigger item is
/// meaningless) — so "collect now" enqueues the very same durable collect job the
/// scheduler would, only right now. `404` if the automation is absent; `400` if it
/// carries no `CollectEmail`/`CollectCalendar` trigger (nothing to collect). Gated on
/// `automation:write` (like pause/resume and firing).
async fn collect_now(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
) -> ApiResult<(StatusCode, Json<CollectNowResult>)> {
    let p = auth.principal();
    auth.require(Action::Write, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    let job = catalerum_ingest::enqueue_collect_now(state.store(), p.workspace_id, &automation)
        .await
        .map_err(|e| ApiError::internal(format!("enqueuing collect job: {e}")))?
        .ok_or_else(|| {
            ApiError::bad_request(
                "not a collect automation — needs a collect_email/collect_calendar trigger",
            )
        })?;
    Ok((StatusCode::ACCEPTED, Json(CollectNowResult { job })))
}

/// Default + max number of recent runs `GET /automations/{name}/runs` returns.
const RUNS_DEFAULT_LIMIT: i64 = 50;
const RUNS_MAX_LIMIT: i64 = 200;

/// Query for `GET /automations/{name}/runs` — `?limit=N` (clamped).
#[derive(Debug, Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /automations/{name}/runs/{run_id}` body: the run plus its ordered steps —
/// the durable audit trail of one execution (SOUL §5/§11).
#[derive(Debug, Serialize)]
pub struct RunDetail {
    pub run: AutomationRun,
    pub steps: Vec<AutomationStep>,
}

/// Resolve the effective run-list limit: the requested value (or the default),
/// clamped to `[1, RUNS_MAX_LIMIT]` so a client can neither ask for zero nor an
/// unbounded scan.
fn runs_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(RUNS_DEFAULT_LIMIT)
        .clamp(1, RUNS_MAX_LIMIT)
}

/// The recent runs of the named automation, newest first (SOUL §11 observability).
async fn list_runs(
    State(state): State<AppState>,
    auth: Auth,
    Path(name): Path<String>,
    Query(q): Query<RunsQuery>,
) -> ApiResult<Json<Vec<AutomationRun>>> {
    let p = auth.principal();
    auth.require(Action::Read, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    let limit = runs_limit(q.limit);
    let runs = state
        .store()
        .automation_runs()
        .list_runs(p.workspace_id, automation.id, limit)
        .await?;
    Ok(Json(runs))
}

/// One run of the named automation plus its steps (`404` if the run id is unknown
/// or belongs to a different automation). Workspace-scoped (SOUL §18).
async fn get_run(
    State(state): State<AppState>,
    auth: Auth,
    Path((name, run_id)): Path<(String, String)>,
) -> ApiResult<Json<RunDetail>> {
    let p = auth.principal();
    auth.require(Action::Read, "automation")?;
    let automation = state
        .store()
        .automations()
        .get_by_name(p.workspace_id, name.trim())
        .await?
        .ok_or(ApiError::NotFound)?;
    let run_id: AutomationRunId = run_id
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request("invalid run id"))?;
    let run = state
        .store()
        .automation_runs()
        .get_run(p.workspace_id, run_id)
        .await?;
    // The run is workspace-scoped already; also ensure it is *this* automation's,
    // so `/automations/{a}/runs/{run-of-b}` is a 404, not a cross-automation peek.
    if run.automation_id != automation.id {
        return Err(ApiError::NotFound);
    }
    let steps = state
        .store()
        .automation_runs()
        .list_steps(p.workspace_id, run_id)
        .await?;
    Ok(Json(RunDetail { run, steps }))
}

// ---------------------------------------------------------------------------
// Node-type catalog + semantic search (SOUL §11) — documentation for authoring
// automations, for both the visual editor and tool-using agents.
// ---------------------------------------------------------------------------

/// Default / max node-type search results.
const NODE_SEARCH_DEFAULT_LIMIT: usize = 8;
const NODE_SEARCH_MAX_LIMIT: usize = 24;

/// `GET /automations/node-types` — the full node-type catalog (every trigger /
/// action / code / condition node type with its docs, params, and example). Static,
/// global documentation; gated on `automation:read` like the rest of the surface.
/// Returns the **same shape** as the search endpoint ([`NodeTypeHit`]) with `score`
/// fixed at `0.0` (this listing is unranked), so a client decodes both identically.
async fn list_node_types(auth: Auth) -> ApiResult<Json<Vec<NodeTypeHit>>> {
    auth.require(Action::Read, "automation")?;
    let out = catalerum_automation::catalog()
        .iter()
        .cloned()
        .map(|doc| NodeTypeHit { doc, score: 0.0 })
        .collect();
    Ok(Json(out))
}

/// Query for `GET /automations/node-types/search` — `?q=…&limit=N`.
#[derive(Debug, Deserialize)]
pub struct NodeSearchQuery {
    /// Natural-language description of the node the author needs.
    #[serde(default)]
    pub q: String,
    /// Max results (clamped to `[1, NODE_SEARCH_MAX_LIMIT]`).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One ranked node-type search result: the full [`NodeDoc`] plus its relevance score.
#[derive(Debug, Serialize)]
pub struct NodeTypeHit {
    #[serde(flatten)]
    pub doc: NodeDoc,
    /// Cosine similarity to the query (higher is closer).
    pub score: f32,
}

/// `GET /automations/node-types/search?q=…&limit=N` — semantically rank the
/// node-type catalog against `q` (SOUL §11). Empty `q` → `400`. Gated `automation:read`.
async fn search_node_types(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<NodeSearchQuery>,
) -> ApiResult<Json<Vec<NodeTypeHit>>> {
    auth.require(Action::Read, "automation")?;
    let query = q.q.trim();
    if query.is_empty() {
        return Err(ApiError::bad_request("search query `q` must not be empty"));
    }
    let limit = q
        .limit
        .unwrap_or(NODE_SEARCH_DEFAULT_LIMIT)
        .clamp(1, NODE_SEARCH_MAX_LIMIT);
    let hits = state.node_index().search(query, limit).await?;
    let out = hits
        .into_iter()
        .map(|h| NodeTypeHit {
            doc: h.doc,
            score: h.score,
        })
        .collect();
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn router_builds_with_overlapping_node_types_and_name_routes() {
        // `/automations/node-types[/search]` are registered alongside the
        // `/automations/{name}` capture. Route insertion happens here, so a matchit
        // overlap conflict would panic on build — assert it doesn't (static wins).
        let _: Router<AppState> = router();
    }

    #[test]
    fn node_search_limit_clamps() {
        // The node-type search limit clamps into [1, NODE_SEARCH_MAX_LIMIT].
        let clamp = |n: Option<usize>| {
            n.unwrap_or(NODE_SEARCH_DEFAULT_LIMIT)
                .clamp(1, NODE_SEARCH_MAX_LIMIT)
        };
        assert_eq!(clamp(None), NODE_SEARCH_DEFAULT_LIMIT);
        assert_eq!(clamp(Some(0)), 1, "zero clamps up to 1");
        assert_eq!(
            clamp(Some(999)),
            NODE_SEARCH_MAX_LIMIT,
            "over-large is capped"
        );
        assert_eq!(clamp(Some(5)), 5);
    }

    #[test]
    fn create_body_defaults_enabled_true_and_empty_lists() {
        let body: CreateAutomation = serde_json::from_str(r#"{"name":"daily"}"#).unwrap();
        assert_eq!(body.name, "daily");
        assert!(body.enabled, "enabled defaults to true");
        assert!(body.triggers.is_empty() && body.actions.is_empty());
        assert!(body.condition.is_none() && body.spec.is_none());
    }

    #[test]
    fn runs_limit_defaults_and_clamps() {
        assert_eq!(runs_limit(None), RUNS_DEFAULT_LIMIT);
        assert_eq!(runs_limit(Some(10)), 10);
        assert_eq!(runs_limit(Some(0)), 1, "zero is clamped up to 1");
        assert_eq!(runs_limit(Some(-5)), 1, "negatives clamp to 1");
        assert_eq!(
            runs_limit(Some(9999)),
            RUNS_MAX_LIMIT,
            "over-large is capped"
        );
    }

    #[test]
    fn collect_capabilities_extracts_one_read_cap_per_collect_trigger() {
        // A collect trigger demands `<domain>:read@<connection>`; other triggers,
        // unparseable specs, and blank connections demand nothing here.
        let caps = collect_capabilities(&[
            json!({ "kind": "collect_email", "connection": " conn-a " }),
            json!({ "kind": "collect_calendar", "connection": "conn-b" }),
            json!({ "kind": "webhook", "path": "/h" }),
            json!({ "kind": "collect_email" }), // unparseable (no connection field)
            json!({ "kind": "nonsense" }),      // unparseable kind
        ]);
        assert_eq!(caps.len(), 2);
        assert_eq!(caps[0].action, Action::Read);
        assert_eq!(caps[0].resource.domain, "email");
        assert_eq!(
            caps[0].resource.selector.as_deref(),
            Some("conn-a"),
            "trimmed"
        );
        assert_eq!(caps[1].resource.domain, "calendar");
        assert_eq!(caps[1].resource.selector.as_deref(), Some("conn-b"));
    }

    #[test]
    fn require_collect_authority_gates_on_the_tokens_effective_authority() {
        use catalerum_core::model::{Grant, Role};
        use catalerum_core::{GrantId, UserId, WorkspaceId};

        let triggers = vec![json!({ "kind": "collect_email", "connection": "conn-1" })];

        // A non-scoped token answers from its role's base set: Member+/Owner hold
        // domain-wide `email:read`, so authoring a collector passes (unchanged).
        let owner = Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            Role::Owner,
        ));
        assert!(require_collect_authority(&owner.capabilities(), &triggers).is_ok());
        let member = Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            Role::Member,
        ));
        assert!(require_collect_authority(&member.capabilities(), &triggers).is_ok());

        // The fix: a grant-scoped token minted by an Owner but WITHOUT the
        // connection's read cap is DENIED — the minter's Owner role no longer leaks
        // through; only the grant's effective authority counts (SOUL §19).
        let ws = WorkspaceId::new();
        let narrow = Grant {
            id: GrantId::new(),
            workspace_id: ws,
            name: "automation-only".into(),
            capabilities: vec![Capability::new(
                Action::Write,
                Resource::domain("automation"),
            )],
            constraints: Default::default(),
        };
        let mut p = catalerum_iam::Principal::new(UserId::new(), ws, Role::Owner);
        p.grant_id = Some(narrow.id);
        let scoped_denied = Auth::with_grant(p, narrow);
        assert!(matches!(
            require_collect_authority(&scoped_denied.capabilities(), &triggers),
            Err(ApiError::Forbidden(_))
        ));

        // A scoped token that DOES hold the connection's read cap is allowed — both a
        // domain-wide `email:read` and the exact `email:read@conn-1` selector satisfy
        // `email:read@conn-1` (mirroring `Auth::require`'s covers-match).
        for held in [
            Capability::new(Action::Read, Resource::domain("email")),
            Capability::new(Action::Read, Resource::new("email", "conn-1")),
        ] {
            let ws = WorkspaceId::new();
            let grant = Grant {
                id: GrantId::new(),
                workspace_id: ws,
                name: "collector".into(),
                capabilities: vec![held],
                constraints: Default::default(),
            };
            let mut p = catalerum_iam::Principal::new(UserId::new(), ws, Role::Member);
            p.grant_id = Some(grant.id);
            let scoped_ok = Auth::with_grant(p, grant);
            assert!(require_collect_authority(&scoped_ok.capabilities(), &triggers).is_ok());
        }

        // A non-collect automation demands nothing, whatever the authority.
        let viewer = Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            Role::Viewer,
        ));
        assert!(require_collect_authority(
            &viewer.capabilities(),
            &[json!({ "kind": "webhook", "path": "/h" })]
        )
        .is_ok());
    }

    #[test]
    fn compile_graph_triggers_is_none_for_a_legacy_spec() {
        // No spec / a non-graph spec → None (the legacy linear path runs unchanged).
        assert!(compile_graph_triggers(None).is_none());
        assert!(compile_graph_triggers(Some(&json!({ "note": "freeform" }))).is_none());
        assert!(compile_graph_triggers(Some(&json!("scalar"))).is_none());
    }

    #[test]
    fn compile_graph_triggers_compiles_a_valid_graphs_trigger_nodes() {
        // A trigger node → an action node. The compiled `triggers` are the graph's
        // Trigger nodes serialized to JSON, so dispatch matching still fires.
        let spec = json!({ "graph": {
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "create_note", "title": "x" } }
            ],
            "edges": [ { "from": "t", "to": "a" } ]
        }});
        let triggers = compile_graph_triggers(Some(&spec)).unwrap().unwrap();
        assert_eq!(triggers.len(), 1);
        assert_eq!(triggers[0]["kind"], json!("webhook"));
        assert_eq!(triggers[0]["path"], json!("/h"));
    }

    #[test]
    fn compile_graph_triggers_rejects_an_invalid_graph() {
        // A graph with no trigger node fails validation → a 400 (not stored).
        let no_trigger = json!({ "graph": {
            "nodes": [ { "id": "a", "kind": "action", "action": { "kind": "summarize" } } ],
            "edges": []
        }});
        assert!(compile_graph_triggers(Some(&no_trigger)).unwrap().is_err());

        // A cyclic graph fails validation too.
        let cyclic = json!({ "graph": {
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [ { "from": "t", "to": "a" }, { "from": "a", "to": "t" } ]
        }});
        assert!(compile_graph_triggers(Some(&cyclic)).unwrap().is_err());

        // A malformed graph value (unknown node kind) → Some(Err), not a silent skip.
        let malformed = json!({ "graph": { "nodes": [ { "id": "t", "kind": "nope" } ] } });
        assert!(compile_graph_triggers(Some(&malformed)).unwrap().is_err());
    }

    #[test]
    fn validate_spec_rejects_an_invalid_schedule_cron() {
        // The REST create/update path validates the cron (via AutomationSpec), so a
        // `Schedule` automation that could never fire is a 400, not a stored row.
        assert!(validate_spec(
            &[json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            None,
            &[json!({ "kind": "summarize" })],
        )
        .is_ok());
        assert!(validate_spec(
            &[json!({ "kind": "schedule", "cron": "every blue moon" })],
            None,
            &[json!({ "kind": "summarize" })],
        )
        .is_err());
    }

    #[test]
    fn create_body_parses_full_shape() {
        let body: CreateAutomation = serde_json::from_str(
            r#"{"name":"x","enabled":false,"triggers":[{"kind":"schedule","cron":"0 9 * * *"}],"actions":[{"kind":"summarize"}]}"#,
        )
        .unwrap();
        assert!(!body.enabled);
        assert_eq!(body.triggers.len(), 1);
        assert_eq!(body.actions.len(), 1);
    }

    #[test]
    fn clean_name_rejects_blank_and_trims() {
        assert!(clean_name("   ").is_err());
        assert_eq!(clean_name("  daily-digest ").unwrap(), "daily-digest");
    }

    #[test]
    fn validate_spec_enforces_the_typed_contract() {
        // A well-formed spec passes.
        assert!(validate_spec(
            &[json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            None,
            &[json!({ "kind": "summarize" })],
        )
        .is_ok());
        // Empty triggers / empty actions / unknown kind / missing field all 400.
        assert!(validate_spec(&[], None, &[json!({ "kind": "summarize" })]).is_err());
        assert!(validate_spec(&[json!({ "kind": "webhook", "path": "/h" })], None, &[]).is_err());
        assert!(validate_spec(
            &[json!({ "kind": "telepathy" })],
            None,
            &[json!({ "kind": "summarize" })]
        )
        .is_err());
        assert!(validate_spec(
            &[json!({ "kind": "schedule" })],
            None,
            &[json!({ "kind": "summarize" })]
        )
        .is_err());
    }

    fn db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    /// End-to-end (SOUL §11 Phase A): a graph automation (webhook trigger node →
    /// `create_note` action node) is stored with its dispatch `triggers` **compiled
    /// from the graph**, then a matching webhook event drives it through the worker —
    /// the action node runs (the note lands) and a durable step is recorded per node.
    #[tokio::test]
    async fn a_graph_automation_compiles_triggers_and_runs_per_node_through_the_worker() {
        let Some(url) = db_url() else {
            eprintln!(
                "skipping graph-automation e2e test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
            );
            return;
        };
        use crate::action_runner::ToolActionRunner;
        use crate::tools::{build_registry, NoteIngest};
        use catalerum_automation::{ActionRunner, TriggerEvent};
        use catalerum_core::model::{Role, RunStatus, StepStatus};
        use catalerum_ingest::{dispatch_trigger_event, AutomationContext, SyncWorker};

        // Isolated db (own `job_queue`) so the worker below can't claim another
        // parallel test's `run_automation` job (and vice versa).
        let store = crate::test_db::isolated_store(&url).await;
        let ws = store
            .workspaces()
            .create("graphauto", &format!("graphauto-{}", uuid::Uuid::new_v4()))
            .await
            .expect("ws");
        // The graph runner resolves the workspace owner's authority (base-Member caps
        // cover create_note), so the workspace needs an Owner membership.
        let owner = store
            .users()
            .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
            .await
            .expect("owner user");
        store
            .memberships()
            .upsert(ws.id, owner.id, Role::Owner)
            .await
            .expect("owner membership");

        // The authoring spec the client POSTs: a node-graph automation. Its `triggers`
        // column is left empty on the wire — the handler compiles it from the graph.
        let spec = json!({ "graph": {
            "nodes": [
                { "id": "t", "kind": "trigger",
                  "trigger": { "kind": "webhook", "path": "/graph-hook" } },
                { "id": "a", "kind": "action",
                  "action": { "kind": "create_note", "title": "from graph", "markdown": "hi" } }
            ],
            "edges": [ { "from": "t", "to": "a" } ]
        }});

        // The create-handler logic: a present graph compiles its dispatch triggers.
        let triggers = compile_graph_triggers(Some(&spec))
            .expect("graph present")
            .expect("graph valid");
        assert_eq!(triggers.len(), 1, "one compiled trigger from the graph");
        assert_eq!(triggers[0]["kind"], json!("webhook"));
        assert_eq!(triggers[0]["path"], json!("/graph-hook"));

        let stored = store
            .automations()
            .create(
                ws.id,
                &NewAutomation {
                    name: "graph-bot".into(),
                    enabled: true,
                    // The wire `actions`/`triggers` are empty for a graph automation;
                    // the compiled triggers are persisted so dispatch still matches.
                    triggers: triggers.clone(),
                    condition: None,
                    actions: vec![],
                    spec: Some(spec.clone()),
                    grant_id: None,
                },
            )
            .await
            .expect("create");
        // The stored automation carries the compiled triggers (its dispatch shadow),
        // while the graph itself round-trips verbatim in `spec`.
        assert_eq!(
            stored.triggers, triggers,
            "stored triggers came from the graph"
        );
        assert!(stored.spec.as_ref().unwrap().get("graph").is_some());

        // A matching webhook event → a durable run_automation job is enqueued.
        let event = TriggerEvent::Webhook {
            path: "/graph-hook".into(),
        };
        let jobs = dispatch_trigger_event(&store, ws.id, &event)
            .await
            .expect("dispatch");
        assert_eq!(jobs.len(), 1, "the compiled webhook trigger matched");

        // embed/graph off → only Postgres is needed for create_note.
        let registry = build_registry(
            &store,
            None,
            NoteIngest::new(store.clone(), false, false),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
        );
        let runner: std::sync::Arc<dyn ActionRunner> = std::sync::Arc::new(
            ToolActionRunner::workspace_owner_authority(registry, store.clone()),
        );
        let worker =
            SyncWorker::new(store.clone()).with_automation_context(AutomationContext::new(runner));

        // Drain the job through the worker until a run is recorded.
        let mut ran = false;
        for _ in 0..50 {
            if !store
                .automation_runs()
                .list_runs(ws.id, stored.id, 5)
                .await
                .unwrap()
                .is_empty()
            {
                ran = true;
                break;
            }
            if !worker.poll_once().await.unwrap() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        assert!(ran, "the worker ran the graph automation");

        let runs = store
            .automation_runs()
            .list_runs(ws.id, stored.id, 5)
            .await
            .unwrap();
        assert_eq!(
            runs[0].status,
            RunStatus::Succeeded,
            "the graph run succeeded"
        );

        // One durable step per graph node (the trigger node + the action node), both
        // succeeded — the DAG executor recorded per-node steps.
        let steps = store
            .automation_runs()
            .list_steps(ws.id, runs[0].id)
            .await
            .unwrap();
        assert_eq!(steps.len(), 2, "one step per graph node (trigger + action)");
        assert!(
            steps.iter().all(|s| s.status == StepStatus::Succeeded),
            "every node step succeeded"
        );
        // The step rows carry the node identity (id + kind) the executor encoded.
        let kinds: Vec<&str> = steps
            .iter()
            .filter_map(|s| s.action.get("kind").and_then(|v| v.as_str()))
            .collect();
        assert!(kinds.contains(&"trigger") && kinds.contains(&"action"));

        // The action node really created the note (data flowed trigger → action).
        let notes = store
            .notes()
            .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
            .await
            .unwrap();
        assert!(
            notes.iter().any(|n| n.title == "from graph"),
            "the graph's action node created the note"
        );
    }
}
