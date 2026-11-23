//! Automation debugging + run-authority tools (SOUL §11/§19).
//!
//! An automation runs under its own authority, not the author's: the bound §19
//! grant, or — with no grant — the workspace owner bounded to base **Member**
//! capabilities ([`default_automation_capabilities`]). A chat agent authoring an
//! automation holds far more (an Owner's `*`), so a graph that works when the agent
//! tries its steps by hand can be denied on every scheduled run. These tools close
//! that loop:
//!
//! - `list_automation_runs` / `get_automation_run` — the durable run + step trail
//!   (status, error, per-step output), so an agent can see *why* a run failed.
//! - `diagnose_automation` — the effective authority, every node whose tool that
//!   authority does not cover (or that this server lacks), graph warnings, collect
//!   connection problems, and a recent-run summary, with concrete fix hints.
//! - `list_grants` / `set_automation_grant` — inspect grants and bind (or mint +
//!   bind) the grant an automation runs under. Admin-only (`grant:write`), and the
//!   grant must be ⊆ the caller's own authority (SOUL §19 attenuation).
//!
//! The create/update/edit/test/run automation tools reuse [`authority_problems`]
//! so the "your run authority can't do this" finding shows up at authoring time.

use super::*;

use catalerum_automation::graph::{Graph, NodeKind};
use catalerum_automation::{Action as AutoAction, ActionKind};
use catalerum_core::model::{AutomationRun, AutomationStep, Grant, RunStatus};
use catalerum_core::{AutomationRunId, GrantId};

use crate::action_runner::{default_automation_capabilities, tool_for};

/// Default number of runs `list_automation_runs` / `diagnose_automation` look at.
const DEFAULT_RUN_LIMIT: u64 = 10;
/// Hard cap on runs returned per call.
const MAX_RUN_LIMIT: u64 = 50;
/// Default per-step output budget (chars) in `get_automation_run`.
const DEFAULT_STEP_OUTPUT_CHARS: u64 = 2_000;
/// Hard cap on the per-step output budget.
const MAX_STEP_OUTPUT_CHARS: u64 = 20_000;
/// Longest `run_automation` will block waiting for its run to finish.
pub(crate) const MAX_RUN_WAIT_SECS: u64 = 120;

/// The authority an automation's runs execute under.
pub(crate) struct RunAuthority {
    /// The bound §19 grant, if any.
    pub(crate) grant: Option<Grant>,
    /// The bound grant id when the grant row no longer resolves (deleted).
    pub(crate) dangling_grant: Option<GrantId>,
    /// The effective capability set every action is checked against.
    pub(crate) capabilities: Vec<Capability>,
}

impl RunAuthority {
    /// The default authority (no grant): workspace owner, base-Member capabilities.
    pub(crate) fn default_authority() -> Self {
        Self {
            grant: None,
            dangling_grant: None,
            capabilities: default_automation_capabilities(),
        }
    }

    /// A grant's authority (the grant **replaces** the default, SOUL §19).
    pub(crate) fn from_grant(grant: Grant) -> Self {
        Self {
            capabilities: grant.capabilities.clone(),
            grant: Some(grant),
            dangling_grant: None,
        }
    }

    /// A human/agent-readable description of this authority.
    pub(crate) fn describe(&self) -> Json {
        let caps: Vec<String> = self.capabilities.iter().map(Capability::to_token).collect();
        match (&self.grant, self.dangling_grant) {
            (Some(g), _) => json!({
                "source": "grant",
                "grant": g.name,
                "grant_id": g.id,
                "capabilities": caps,
                "dry_run": g.constraints.dry_run,
                "note": "Runs under this grant's capabilities only (it replaces the default authority).",
            }),
            (None, Some(id)) => json!({
                "source": "missing_grant",
                "grant_id": id,
                "capabilities": caps,
                "note": "The bound grant no longer exists; runs will fail until a grant is re-bound with set_automation_grant.",
            }),
            (None, None) => json!({
                "source": "default",
                "capabilities": caps,
                "note": "No grant bound: runs as the workspace owner limited to base Member capabilities (read/write on ordinary domains; never exec, delete, or admin). Bind a grant with set_automation_grant to allow more.",
            }),
        }
    }
}

/// Resolve the authority an automation bound to `grant_id` runs under.
pub(crate) async fn run_authority(
    store: &Store,
    ws: WorkspaceId,
    grant_id: Option<GrantId>,
) -> Result<RunAuthority> {
    let Some(id) = grant_id else {
        return Ok(RunAuthority::default_authority());
    };
    match store.grants().get(ws, id).await {
        Ok(g) => Ok(RunAuthority::from_grant(g)),
        Err(StoreError::NotFound) => Ok(RunAuthority {
            grant: None,
            dangling_grant: Some(id),
            capabilities: Vec::new(),
        }),
        Err(e) => Err(e.into()),
    }
}

/// The action nodes an automation dispatches, labelled by graph node id (or
/// `actions[i]` for a linear automation). An unparseable graph/action is skipped —
/// save-time validation reports those.
pub(crate) fn dispatched_actions(spec: Option<&Json>, actions: &[Json]) -> Vec<(String, AutoAction)> {
    if let Some(parsed) = Graph::from_spec(spec) {
        let Ok(graph) = parsed else {
            return Vec::new();
        };
        return graph
            .nodes
            .into_iter()
            .filter_map(|n| match n.kind {
                NodeKind::Action { action } => Some((n.id, action)),
                _ => None,
            })
            .collect();
    }
    actions
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            serde_json::from_value::<AutoAction>(a.clone())
                .ok()
                .map(|a| (format!("actions[{i}]"), a))
        })
        .collect()
}

/// The registry tools an action will dispatch: its backing tool, or — for an
/// `llm_agent` with a pinned `tools` list — every listed tool (an unpinned agent
/// discovers tools at run time, so nothing can be checked up front).
fn tools_for_action(action: &AutoAction) -> Vec<String> {
    if action.kind == ActionKind::LlmAgent {
        return action.as_llm_agent().map(|a| a.tools).unwrap_or_default();
    }
    tool_for(action.kind)
        .map(|t| vec![t.to_string()])
        .unwrap_or_default()
}

fn action_kind_name(kind: ActionKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// Every node whose tool `capabilities` does not cover, or that `registry` does
/// not hold (e.g. `open_terminal` on a server with no executor). Each finding is
/// `{ node, action, tool, problem, requires?, message }`. With no registry only the
/// tool-name mapping is known, so nothing is reported.
pub(crate) fn authority_problems(
    registry: Option<&ToolRegistry>,
    spec: Option<&Json>,
    actions: &[Json],
    capabilities: &[Capability],
) -> Vec<Json> {
    let Some(registry) = registry else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (node, action) in dispatched_actions(spec, actions) {
        let kind = action_kind_name(action.kind);
        for tool_name in tools_for_action(&action) {
            let Some(tool) = registry.get(&tool_name) else {
                out.push(json!({
                    "node": node,
                    "action": kind,
                    "tool": tool_name,
                    "problem": "tool_unavailable",
                    "message": format!(
                        "node '{node}' ({kind}) uses tool `{tool_name}`, which this server does not provide (its backend is not configured), so the node will fail"
                    ),
                }));
                continue;
            };
            let Some(required) = tool.required_capability() else {
                continue;
            };
            if capabilities.iter().any(|held| held.covers(&required)) {
                continue;
            }
            let token = required.to_token();
            out.push(json!({
                "node": node,
                "action": kind,
                "tool": tool_name,
                "problem": "not_authorized",
                "requires": token,
                "message": format!(
                    "node '{node}' ({kind}) calls `{tool_name}`, which requires `{token}`; the automation's run authority does not include it, so every run will fail there. Bind a grant that includes `{token}` with set_automation_grant."
                ),
            }));
        }
    }
    out
}

/// [`authority_problems`] for a saved automation, as warning strings for the
/// create/update/edit responses. Best-effort: a grant lookup error yields nothing.
pub(crate) async fn authority_warnings(
    store: &Store,
    ctx: &ToolContext,
    automation: &catalerum_core::Automation,
) -> Vec<String> {
    let Ok(authority) = run_authority(store, automation.workspace_id, automation.grant_id).await
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    if let Some(id) = authority.dangling_grant {
        out.push(format!(
            "the bound grant {id} no longer exists; bind one with set_automation_grant"
        ));
    }
    out.extend(
        authority_problems(
            ctx.registry.as_ref(),
            automation.spec.as_ref(),
            &automation.actions,
            &authority.capabilities,
        )
        .iter()
        .filter_map(|p| p["message"].as_str().map(str::to_string)),
    );
    out
}

fn status_name(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{status:?}").to_lowercase())
}

fn duration_ms(
    start: chrono::DateTime<chrono::Utc>,
    end: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<i64> {
    end.map(|e| (e - start).num_milliseconds())
}

/// Compact view of one run.
pub(crate) fn run_view(run: &AutomationRun) -> Json {
    let trigger_kind = run
        .trigger
        .as_ref()
        .and_then(|t| t.get("kind").or_else(|| t.get("trigger").and_then(|t| t.get("kind"))))
        .and_then(Json::as_str);
    json!({
        "run_id": run.id,
        "status": status_name(run.status),
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "duration_ms": duration_ms(run.started_at, run.finished_at),
        "error": run.error,
        "trigger_kind": trigger_kind,
        "grant_id": run.grant_id,
    })
}

/// Clip a JSON value to roughly `max_chars` of serialized text: small values pass
/// through, large ones become a truncated string plus a marker.
fn clip_json(value: &Json, max_chars: usize) -> Json {
    let text = value.to_string();
    if text.chars().count() <= max_chars {
        return value.clone();
    }
    let head: String = text.chars().take(max_chars).collect();
    json!({ "truncated": true, "total_chars": text.chars().count(), "head": head })
}

fn step_view(step: &AutomationStep, max_chars: usize) -> Json {
    // Graph steps record `{node, kind: "action"|"code"|…, action: {kind, …}}`;
    // linear steps record the action spec itself.
    let node = step.action.get("node").cloned().unwrap_or(Json::Null);
    let kind = step
        .action
        .get("action")
        .and_then(|a| a.get("kind"))
        .or_else(|| step.action.get("kind"))
        .cloned()
        .unwrap_or(Json::Null);
    json!({
        "ordinal": step.ordinal,
        "node": node,
        "kind": kind,
        "status": serde_json::to_value(step.status).unwrap_or(Json::Null),
        "error": step.error,
        "duration_ms": duration_ms(step.started_at, step.finished_at),
        "output": step.output.as_ref().map(|o| clip_json(o, max_chars)),
        "action": clip_json(&step.action, max_chars.min(1_000)),
    })
}

/// Aggregate facts about a newest-first run list.
pub(crate) fn runs_summary(runs: &[AutomationRun]) -> Json {
    let count = |s: RunStatus| runs.iter().filter(|r| r.status == s).count();
    let consecutive_failures = runs
        .iter()
        .take_while(|r| r.status == RunStatus::Failed)
        .count();
    let last_success = runs.iter().find(|r| r.status == RunStatus::Succeeded);
    let last_error = runs.iter().find_map(|r| r.error.clone());
    json!({
        "looked_at": runs.len(),
        "succeeded": count(RunStatus::Succeeded),
        "failed": count(RunStatus::Failed),
        "running": count(RunStatus::Running),
        "cancelled": count(RunStatus::Cancelled),
        "consecutive_failures": consecutive_failures,
        "last_run_at": runs.first().map(|r| r.started_at),
        "last_success_at": last_success.map(|r| r.started_at),
        "last_error": last_error,
    })
}

async fn automation_by_name(
    store: &Store,
    ws: WorkspaceId,
    name: &str,
) -> Result<catalerum_core::Automation> {
    store
        .automations()
        .get_by_name(ws, name)
        .await?
        .ok_or_else(|| {
            Error::invalid(format!(
                "no automation named '{name}'; use list_automations to see the names"
            ))
        })
}

/// `list_automation_runs` — an automation's recent runs, newest first.
pub(crate) struct ListAutomationRunsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListAutomationRunsTool {
    fn name(&self) -> &str {
        "list_automation_runs"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "List an automation's recent runs, newest first: run_id, status \
         (running/succeeded/failed/cancelled), start/finish time, duration, the run \
         error, and what triggered it, plus a summary (failure counts, consecutive \
         failures, last success, last error). Use this to check whether a scheduled \
         automation actually works; open one run's per-step detail with \
         get_automation_run, or call diagnose_automation for causes and fixes."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." },
                "limit": { "type": "integer", "description": "How many runs to return (default 10, max 50)." },
                "status": {
                    "type": "string",
                    "enum": ["running", "succeeded", "failed", "cancelled"],
                    "description": "Only return runs with this status."
                }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let limit = opt_clamped_u64(&args, "limit", DEFAULT_RUN_LIMIT, MAX_RUN_LIMIT);
        let status = opt_str_some(&args, "status");
        let automation = automation_by_name(&self.store, ws, &name).await?;
        // With a status filter, scan the max window so the filter has rows to pick.
        let fetch = if status.is_some() { MAX_RUN_LIMIT } else { limit };
        let runs = self
            .store
            .automation_runs()
            .list_runs(ws, automation.id, fetch as i64)
            .await?;
        let summary = runs_summary(&runs);
        let shown: Vec<Json> = runs
            .iter()
            .filter(|r| status.as_deref().is_none_or(|s| status_name(r.status) == s))
            .take(limit as usize)
            .map(run_view)
            .collect();
        Ok(json!({
            "automation": automation.name,
            "enabled": automation.enabled,
            "runs": shown,
            "summary": summary,
        }))
    }
}

/// `get_automation_run` — one run with its per-step trail.
pub(crate) struct GetAutomationRunTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for GetAutomationRunTool {
    fn name(&self) -> &str {
        "get_automation_run"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "Fetch one automation run by run_id (from list_automation_runs or \
         run_automation) with its steps in execution order: each step's node, action \
         kind, status, error, duration, and (clipped) output. Use it to find which \
         node failed and what it returned. A run that failed before any step started \
         (for example an authorization denial) has its reason in the run's `error`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "The run's id." },
                "max_output_chars": {
                    "type": "integer",
                    "description": "Per-step output budget in characters (default 2000, max 20000). Larger outputs come back truncated."
                }
            },
            "required": ["run_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let run_id: AutomationRunId = parse_id(&args, "run_id")?;
        let max_chars = opt_clamped_u64(
            &args,
            "max_output_chars",
            DEFAULT_STEP_OUTPUT_CHARS,
            MAX_STEP_OUTPUT_CHARS,
        ) as usize;
        let runs = self.store.automation_runs();
        let run = runs.get_run(ws, run_id).await.map_err(|e| match e {
            StoreError::NotFound => Error::invalid(format!("no automation run {run_id}")),
            other => other.into(),
        })?;
        let steps = runs.list_steps(ws, run_id).await?;
        let automation = self
            .store
            .automations()
            .get(ws, run.automation_id)
            .await
            .ok()
            .map(|a| a.name);
        let mut view = run_view(&run);
        if let Some(obj) = view.as_object_mut() {
            obj.insert("automation".into(), json!(automation));
            obj.insert("trigger".into(), run.trigger.as_ref().map_or(Json::Null, |t| clip_json(t, 2_000)));
            obj.insert(
                "steps".into(),
                Json::Array(steps.iter().map(|s| step_view(s, max_chars)).collect()),
            );
        }
        Ok(view)
    }
}

/// `diagnose_automation` — why an automation fails (or would fail), with fixes.
pub(crate) struct DiagnoseAutomationTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DiagnoseAutomationTool {
    fn name(&self) -> &str {
        "diagnose_automation"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "automation")
    }
    fn description(&self) -> &str {
        "Diagnose an automation: the authority its runs execute under (the bound \
         grant, or the default owner-as-Member authority, which cannot run exec / \
         terminal / delete tools), every node whose tool that authority does not \
         cover or that this server does not provide, graph wiring warnings, collect \
         connection problems, a summary of recent runs with the latest errors, and \
         concrete `hints` for fixing it. Call this first when an automation fails or \
         right after authoring one. Note: your own chat permissions are NOT what the \
         automation runs with."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let automation = automation_by_name(&self.store, ws, &name).await?;
        let authority = run_authority(&self.store, ws, automation.grant_id).await?;
        let problems = authority_problems(
            ctx.registry.as_ref(),
            automation.spec.as_ref(),
            &automation.actions,
            &authority.capabilities,
        );
        let warnings = super::automations::graph_warnings(automation.spec.as_ref());
        let connection_error = crate::connection_status::validate_collect_connections(
            &self.store,
            ws,
            &automation.triggers,
        )
        .await
        .err();
        let runs = self
            .store
            .automation_runs()
            .list_runs(ws, automation.id, DEFAULT_RUN_LIMIT as i64)
            .await?;
        let summary = runs_summary(&runs);
        let recent_failures: Vec<Json> = runs
            .iter()
            .filter(|r| r.status == RunStatus::Failed)
            .take(3)
            .map(run_view)
            .collect();

        let mut hints: Vec<String> = Vec::new();
        if !automation.enabled {
            hints.push("The automation is disabled; enable it with set_automation_enabled.".into());
        }
        if authority.dangling_grant.is_some() {
            hints.push("Its bound grant was deleted. Bind a grant with set_automation_grant.".into());
        }
        let mut missing: Vec<String> = problems
            .iter()
            .filter(|p| p["problem"] == "not_authorized")
            .filter_map(|p| p["requires"].as_str().map(str::to_string))
            .collect();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|m| format!("\"{m}\""))
                .collect::<Vec<_>>()
                .join(", ");
            hints.push(match &authority.grant {
                Some(g) => format!(
                    "Grant '{}' lacks {list}. An Owner/Admin should call set_automation_grant {{\"name\": \"{}\", \"capabilities\": [{list}]}}; that adds these to the grant and keeps what it already holds.",
                    g.name, automation.name
                ),
                None => format!(
                    "An Owner/Admin should call set_automation_grant {{\"name\": \"{}\", \"capabilities\": [{list}]}}; that mints a grant with the default Member authority plus these and binds it.",
                    automation.name
                ),
            });
        }
        if problems.iter().any(|p| p["problem"] == "tool_unavailable") {
            hints.push("Some nodes use tools this server does not provide; replace those nodes or configure the backend (e.g. [exec] for terminal tools).".into());
        }
        if let Some(e) = &connection_error {
            hints.push(format!("Collect trigger problem: {e}"));
        }
        if !warnings.is_empty() {
            hints.push("Fix the graph warnings: nodes not reachable from a trigger never run.".into());
        }
        if runs.is_empty() {
            hints.push("It has never run. Use run_automation with wait_secs to try it once and see the outcome.".into());
        } else if summary["consecutive_failures"].as_u64().unwrap_or(0) > 0 && missing.is_empty() {
            hints.push("The latest runs failed. Open one with get_automation_run to see which step failed and its output.".into());
        }

        Ok(json!({
            "automation": automation.name,
            "enabled": automation.enabled,
            "triggers": automation.triggers,
            "authority": authority.describe(),
            "problems": problems,
            "graph_warnings": warnings,
            "connection_error": connection_error,
            "runs": summary,
            "recent_failures": recent_failures,
            "hints": hints,
        }))
    }
}

/// `list_grants` — the workspace's §19 grants and what uses them.
pub(crate) struct ListGrantsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ListGrantsTool {
    fn name(&self) -> &str {
        "list_grants"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "grant")
    }
    fn description(&self) -> &str {
        "List the workspace's capability grants (Owner/Admin only): name, \
         capabilities as `domain:action[@selector]` tokens, constraints, and which \
         automations and agent profiles run under each. Use with \
         set_automation_grant to give an automation the authority it needs."
    }
    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }
    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let grants = self.store.grants().list_by_workspace(ws).await?;
        let automations = self.store.automations().list_by_workspace(ws).await?;
        let profiles = self.store.agent_profiles().list_by_workspace(ws).await?;
        let items: Vec<Json> = grants
            .iter()
            .map(|g| {
                let used_by_automations: Vec<&str> = automations
                    .iter()
                    .filter(|a| a.grant_id == Some(g.id))
                    .map(|a| a.name.as_str())
                    .collect();
                let used_by_profiles: Vec<&str> = profiles
                    .iter()
                    .filter(|p| p.grant_id == Some(g.id))
                    .map(|p| p.name.as_str())
                    .collect();
                json!({
                    "id": g.id,
                    "name": g.name,
                    "capabilities": g.capabilities.iter().map(Capability::to_token).collect::<Vec<_>>(),
                    "constraints": g.constraints,
                    "used_by_automations": used_by_automations,
                    "used_by_profiles": used_by_profiles,
                })
            })
            .collect();
        Ok(json!({ "grants": items }))
    }
}

/// `set_automation_grant` — bind, mint + bind, or unbind an automation's grant.
pub(crate) struct SetAutomationGrantTool {
    pub(crate) store: Store,
}

/// Parse `capabilities` (tokens or capability objects) from tool args.
fn parse_capability_args(value: &Json) -> Result<Vec<Capability>> {
    let items = value
        .as_array()
        .ok_or_else(|| Error::invalid("`capabilities` must be an array"))?;
    items
        .iter()
        .map(|item| match item {
            Json::String(s) => Capability::parse_token(s).map_err(Error::invalid),
            other => serde_json::from_value::<Capability>(other.clone())
                .map_err(|e| Error::invalid(format!("invalid capability {other}: {e}"))),
        })
        .collect()
}

/// Merge `extra` into `base`, dropping exact duplicates (by token).
fn merge_capabilities(base: Vec<Capability>, extra: Vec<Capability>) -> Vec<Capability> {
    let mut seen: HashSet<String> = HashSet::new();
    base.into_iter()
        .chain(extra)
        .filter(|c| seen.insert(c.to_token()))
        .collect()
}

/// §19 attenuation: every capability must be ⊆ the caller's own authority. A
/// context without capabilities holds nothing, so it can confer nothing.
fn check_within_caller(ctx: &ToolContext, caps: &[Capability]) -> Result<()> {
    let held = ctx
        .capabilities
        .as_deref()
        .ok_or_else(|| Error::unauthorized("no authority to confer capabilities from"))?;
    for c in caps {
        if attenuate(held, c).is_err() {
            return Err(Error::unauthorized(format!(
                "`{}` exceeds your own authority; a grant can only confer what you hold",
                c.to_token()
            )));
        }
    }
    Ok(())
}

#[async_trait]
impl Tool for SetAutomationGrantTool {
    fn name(&self) -> &str {
        "set_automation_grant"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "grant")
    }
    fn description(&self) -> &str {
        "Set the capability grant an automation runs under (Owner/Admin only). \
         Without a grant an automation runs as the workspace owner limited to base \
         Member capabilities, so nodes that need e.g. `exec:run` (open_terminal, \
         run_command, terminal_*) are denied on every run. Three forms: \
         (1) {name, grant} binds an existing grant by name (see list_grants); \
         (2) {name, capabilities: [\"exec:run\", ...]} ADDS these capabilities to the \
         grant named `grant` (default: the grant the automation is already bound to, \
         else \"automation:<name>\") and binds it; capabilities the grant already holds \
         are kept. A grant that does not exist yet is minted with the default Member \
         authority plus these (include_default=false for exactly these). Pass \
         replace=true to set the grant to exactly the default authority plus these, \
         dropping everything else it held; (3) {name, unbind: true} returns to the \
         default authority. A grant replaces the default authority, and can never \
         exceed your own. Returns the new authority and any nodes it still does not cover."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The automation's name." },
                "grant": { "type": "string", "description": "Grant name: the existing grant to bind, or the name for a newly minted one when `capabilities` is given (default \"automation:<name>\")." },
                "capabilities": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Capability tokens to add to the grant, `domain:action[@selector]`, e.g. \"exec:run\", \"web:read\", \"storage:write@default/reports/*\", \"note:delete\". List only what is missing: the grant keeps what it already holds unless `replace` is true."
                },
                "include_default": { "type": "boolean", "description": "With `capabilities`, when minting a new grant or with `replace`: also include the default Member authority (default true). The grant replaces the default, so leaving this on keeps ordinary read/write working." },
                "replace": { "type": "boolean", "description": "With `capabilities`: replace the grant's capabilities instead of adding to them (default false). Drops every capability not listed, so a terminal or storage permission granted earlier is lost unless you list it again." },
                "unbind": { "type": "boolean", "description": "Remove the grant; runs fall back to the default authority." }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let automation = automation_by_name(&self.store, ws, &name).await?;
        let unbind = args.get("unbind").and_then(Json::as_bool).unwrap_or(false);
        let grant_name = opt_str_some(&args, "grant");
        let caps_arg = args.get("capabilities").filter(|v| !v.is_null());

        let grant: Option<Grant> = if unbind {
            if grant_name.is_some() || caps_arg.is_some() {
                return Err(Error::invalid(
                    "`unbind` cannot be combined with `grant` or `capabilities`",
                ));
            }
            None
        } else if let Some(raw) = caps_arg {
            let extra = parse_capability_args(raw)?;
            if extra.is_empty() {
                return Err(Error::invalid("`capabilities` is empty"));
            }
            let replace = args.get("replace").and_then(Json::as_bool).unwrap_or(false);
            let include_default = args
                .get("include_default")
                .and_then(Json::as_bool)
                .unwrap_or(true);
            let all_grants = self.store.grants().list_by_workspace(ws).await?;
            // Default to the grant the automation already runs under, so "add
            // what diagnose_automation says is missing" extends the live grant
            // instead of minting a second one without its other capabilities.
            let gname = grant_name
                .or_else(|| {
                    automation
                        .grant_id
                        .and_then(|id| all_grants.iter().find(|g| g.id == id))
                        .map(|g| g.name.clone())
                })
                .unwrap_or_else(|| format!("automation:{}", automation.name));
            let existing = all_grants.into_iter().find(|g| g.name == gname);
            // Adding to an existing grant keeps what it holds: re-running this to
            // fix one missing capability must not drop the ones fixed before
            // (that ping-pong removed exec:run from a live automation's grant).
            let (base, held) = match &existing {
                Some(g) if !replace => (g.capabilities.clone(), g.capabilities.clone()),
                _ if include_default => (default_automation_capabilities(), Vec::new()),
                _ => (Vec::new(), Vec::new()),
            };
            let caps = merge_capabilities(base, extra);
            // §19 attenuation on what this call confers; capabilities the grant
            // already holds were conferred by whoever set them.
            let held_tokens: HashSet<String> = held.iter().map(Capability::to_token).collect();
            let conferred: Vec<Capability> = caps
                .iter()
                .filter(|c| !held_tokens.contains(&c.to_token()))
                .cloned()
                .collect();
            check_within_caller(ctx, &conferred)?;
            // Changing an existing grant changes it for everything bound to it;
            // only allow that when nothing else depends on it.
            let constraints = match &existing {
                Some(g) => {
                    let others: Vec<String> = self
                        .store
                        .automations()
                        .list_by_workspace(ws)
                        .await?
                        .into_iter()
                        .filter(|a| a.grant_id == Some(g.id) && a.id != automation.id)
                        .map(|a| format!("automation '{}'", a.name))
                        .chain(
                            self.store
                                .agent_profiles()
                                .list_by_workspace(ws)
                                .await?
                                .into_iter()
                                .filter(|p| p.grant_id == Some(g.id))
                                .map(|p| format!("agent profile '{}'", p.name)),
                        )
                        .collect();
                    if !others.is_empty() {
                        return Err(Error::invalid(format!(
                            "grant '{gname}' is also used by {}; pick another `grant` name so changing it here doesn't widen them",
                            others.join(", ")
                        )));
                    }
                    g.constraints.clone()
                }
                None => Default::default(),
            };
            Some(
                self.store
                    .grants()
                    .upsert(ws, &gname, &caps, &constraints)
                    .await?,
            )
        } else if let Some(gname) = grant_name {
            let g = self
                .store
                .grants()
                .list_by_workspace(ws)
                .await?
                .into_iter()
                .find(|g| g.name == gname)
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "no grant named '{gname}'; see list_grants, or pass `capabilities` to mint one"
                    ))
                })?;
            check_within_caller(ctx, &g.capabilities)?;
            Some(g)
        } else {
            return Err(Error::invalid(
                "pass `grant` (bind an existing grant), `capabilities` (mint + bind), or `unbind: true`",
            ));
        };

        let new = catalerum_store::NewAutomation {
            name: automation.name.clone(),
            enabled: automation.enabled,
            triggers: automation.triggers.clone(),
            condition: automation.condition.clone(),
            actions: automation.actions.clone(),
            spec: automation.spec.clone(),
            grant_id: grant.as_ref().map(|g| g.id),
        };
        let saved = self.store.automations().upsert_by_name(ws, &new).await?;
        let authority = match grant {
            Some(g) => RunAuthority::from_grant(g),
            None => RunAuthority::default_authority(),
        };
        let problems = authority_problems(
            ctx.registry.as_ref(),
            saved.spec.as_ref(),
            &saved.actions,
            &authority.capabilities,
        );
        Ok(json!({
            "automation": saved.name,
            "authority": authority.describe(),
            "remaining_problems": problems,
        }))
    }
}

/// Poll the run spawned by `job_id` until it leaves `running` or `wait_secs`
/// elapse. Returns the run (maybe still running) and, when it failed, its steps.
pub(crate) async fn wait_for_job_run(
    store: &Store,
    ws: WorkspaceId,
    job_id: uuid::Uuid,
    wait_secs: u64,
) -> Result<Json> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(wait_secs);
    let runs = store.automation_runs();
    loop {
        let run = runs.latest_run_by_job(ws, job_id).await?;
        let done = run.as_ref().is_some_and(|r| r.status != RunStatus::Running);
        if done || tokio::time::Instant::now() >= deadline {
            return Ok(match run {
                None => json!({ "status": "queued", "note": "The worker has not started the run yet; check later with list_automation_runs." }),
                Some(r) => {
                    let mut view = run_view(&r);
                    if r.status == RunStatus::Failed {
                        let steps = runs.list_steps(ws, r.id).await.unwrap_or_default();
                        if let Some(obj) = view.as_object_mut() {
                            obj.insert(
                                "failed_steps".into(),
                                Json::Array(
                                    steps
                                        .iter()
                                        .filter(|s| s.error.is_some())
                                        .map(|s| step_view(s, 1_000))
                                        .collect(),
                                ),
                            );
                        }
                    }
                    view
                }
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    }
}

/// Register the automation debugging + grant-binding tools.
pub(crate) fn register_automation_debug_tools(registry: &mut ToolRegistry, store: &Store) {
    registry.register(Arc::new(ListAutomationRunsTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(GetAutomationRunTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(DiagnoseAutomationTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(ListGrantsTool {
        store: store.clone(),
    }));
    registry.register(Arc::new(SetAutomationGrantTool {
        store: store.clone(),
    }));
}
