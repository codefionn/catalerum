//! The automation execution engine (SOUL §11): run an automation's typed actions
//! in order, recording durable run/step state via the [`ExecutionState`] port.
//!
//! The action execution itself is **pluggable** via [`ActionRunner`] — the §7 LLM
//! agent loop (`LlmAgent`), the §20 Executor (`RunCommand`), channel notify, etc.
//! bind in as concrete runners in later slices. This driver owns the
//! orchestration + audit: open a run, execute each action as an ordered step,
//! finalize. Trigger registration and Valkey/job-queue dispatch (deciding *when*
//! to call [`execute`]) are a further slice.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::engine::{ExecutionError, ExecutionState};
use crate::graph::{
    step_action_json, ForEachRegion, Graph, NodeKind, MAX_LOOP_BODY_NODES, MAX_LOOP_ITERATIONS,
};
use crate::trigger::TriggerEvent;
use crate::{Action, AutomationSpec};
use catalerum_core::model::{
    Automation, AutomationStep, ExtractedAttachment, Grant, RunStatus, StepStatus,
};
use catalerum_core::{AutomationRun, AutomationRunId, GrantId, MailboxId, WorkspaceId};

/// The outcome of executing one [`Action`] — a terminal [`StepStatus`] plus its
/// output / error. An [`ActionRunner`] reports failure **in band** (a `Failed`
/// outcome), so one bad action doesn't abort the engine out of band.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionOutcome {
    pub status: StepStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
}

impl ActionOutcome {
    /// A successful action, optionally carrying an output.
    #[must_use]
    pub fn succeeded(output: Option<Value>) -> Self {
        Self {
            status: StepStatus::Succeeded,
            output,
            error: None,
        }
    }

    /// A failed action — stops the run (the remaining actions do not execute).
    #[must_use]
    pub fn failed(error: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Failed,
            output: None,
            error: Some(error.into()),
        }
    }

    /// A skipped action (e.g. a per-action condition excluded it) — the run
    /// continues.
    #[must_use]
    pub fn skipped() -> Self {
        Self {
            status: StepStatus::Skipped,
            output: None,
            error: None,
        }
    }
}

/// Executes one automation action (SOUL §11). Implementations bind an action kind
/// to real behavior (the §7 agent loop for `LlmAgent`, the §20 Executor for
/// `RunCommand`, a channel send for `Notify`, …). The [`execute`] driver records
/// the durable step around each call; a runner only does the work and reports an
/// [`ActionOutcome`].
///
/// `trigger` is the event/payload that fired the run (the same value recorded on
/// the run, JSON), so a runner can act on its context — e.g. an `LlmAgent` is told
/// *what* triggered it. `None` for an ad-hoc/manual run.
///
/// `grant` is the automation's §19 capability grant, if it runs under one — the
/// **attenuated authority** the action dispatches with (it *replaces* the runner's
/// default capabilities, never widening beyond what an admin conferred). `None` →
/// the runner's default authority (its base role).
#[async_trait]
pub trait ActionRunner: Send + Sync {
    /// Run `action` in `workspace_id` (fired by `trigger`, under `grant`), returning
    /// its outcome.
    async fn run(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome;

    /// Archive a collected message's raw `.eml` + attachments to the workspace's
    /// files store and link them onto the stored email row (SOUL §9/§28/§29).
    ///
    /// This is the storage half of the `WriteEmail` flow, invoked by the collect
    /// pipeline **after** the message is durably written — separate from [`run`]
    /// because the transient raw bytes are `#[serde(skip)]` on `Email` and so never
    /// survive into the trigger JSON a `WriteEmail` action deserializes (and the
    /// attachment parts are already MIME-extracted here, where a parser lives). A
    /// runner backed by a storage registry writes `emails/<mailbox>/<uid>/raw.eml`
    /// and one object per attachment, then records the refs; the archived objects
    /// ride the §10 object-ingest pipeline for free. Best-effort and idempotent
    /// (a redelivery whose row is already archived is a no-op) — archival is derived,
    /// the email row is the source of truth. The default is a **no-op** (a runner
    /// with no store, e.g. a test double, simply doesn't archive).
    async fn archive_email(
        &self,
        _workspace_id: WorkspaceId,
        _mailbox_id: MailboxId,
        _uid: &str,
        _raw: Option<Vec<u8>>,
        _attachments: Vec<ExtractedAttachment>,
    ) {
    }

    /// Delete the archived objects (raw `.eml` + attachments) previously written by
    /// [`archive_email`] for a message being reconciled away (SOUL §9/§28) — the
    /// deletion twin, keyed by their object `keys`. Invoked by the collect deletion
    /// reconcile so an upstream-deleted message doesn't leave orphaned blobs (or a
    /// stale search index) behind, mirroring the storage route's own object-delete
    /// cleanup. Best-effort and idempotent; the default is a **no-op**.
    async fn cleanup_email_archive(&self, _workspace_id: WorkspaceId, _keys: Vec<String>) {}
}

/// Executes an inline **code node** of a graph automation (SOUL §11) — the seam
/// the DAG executor uses for [`NodeKind::Code`](crate::graph::NodeKind::Code) and
/// [`NodeKind::Condition`](crate::graph::NodeKind::Condition) nodes, mirroring
/// [`ActionRunner`] for actions. `runtime` selects the language (`"js"` → Boa in
/// Phase B); `source` is the node's inline code; `input` is the merged
/// `{ trigger, inputs }` context. Returns the code's JSON result (for a Condition
/// node, the engine then takes its *truthiness*), or an error string that fails the
/// node (and so the run).
///
/// Keeping this a trait keeps `catalerum-automation` **pure** and unit-testable
/// with a fake runner — no scripting engine dependency lives in this crate.
#[async_trait]
pub trait CodeRunner: Send + Sync {
    /// Run `source` under `runtime` with `input`, returning its JSON result.
    ///
    /// `workspace_id` + the run's §19 `grant` carry the node's **authority**: a
    /// `"js"` runtime backed by a tool host may call registry tools
    /// (`catalerum.callTool`) under exactly this authority — the same one an
    /// [`ActionRunner`] node runs with — so a code node can do no more than an
    /// action node could. A runner with no tool host ignores them and the node
    /// stays a pure transform.
    ///
    /// # Errors
    /// A message describing why the code could not run / failed.
    async fn run_code(
        &self,
        runtime: &str,
        source: &str,
        input: &Value,
        workspace_id: WorkspaceId,
        grant: Option<&Grant>,
    ) -> Result<Value, String>;
}

/// The Phase-A default [`CodeRunner`]: **no runtime is configured**, so every Code
/// / Condition node fails with a clear message. A graph built only of Trigger and
/// Action nodes runs fully under this; inline code lands in Phase B (Boa).
pub struct FailCodeRunner;

#[async_trait]
impl CodeRunner for FailCodeRunner {
    async fn run_code(
        &self,
        runtime: &str,
        _source: &str,
        _input: &Value,
        _workspace_id: WorkspaceId,
        _grant: Option<&Grant>,
    ) -> Result<Value, String> {
        Err(format!(
            "no code runtime configured for runtime '{runtime}'"
        ))
    }
}

/// Run `automation` end-to-end (SOUL §11): open a run, execute each typed action
/// as an ordered step via `runner`, and finalize. A direct (non-job) invocation —
/// always a fresh run; see [`execute_for_job`] for the resumable, job-spawned path.
///
/// A run is `Succeeded` only if every step succeeded or was skipped; the first
/// `Failed` step stops execution and the run is `Failed`. A malformed stored spec
/// is recorded as a `Failed` run (with the parse error), not a silent error —
/// the run history always reflects an attempt. Returns the finalized
/// [`AutomationRun`].
///
/// # Errors
/// [`ExecutionError`] if the durable run/step state cannot be written — e.g. the
/// automation does not exist in `workspace_id` (`start_run`'s tenancy guard).
pub(crate) async fn execute_with_state(
    state: &dyn ExecutionState,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    automation: &Automation,
    trigger: Option<Value>,
    job_id: Option<Uuid>,
) -> Result<AutomationRun, ExecutionError> {
    execute_for_job_with_state(
        state,
        runner,
        code,
        workspace_id,
        automation,
        trigger,
        job_id,
    )
    .await
}

/// Like the direct engine execution, but **resumable** across a crash
/// (SOUL §5/§11/§6.2). When
/// `job_id` is the `run_automation` job driving this run and a still-`running` run
/// already exists for it (a previous worker crashed mid-run and the §6.2 reconciler
/// re-drove the job), the engine **resumes that run** instead of starting a fresh
/// one: steps that already finished `Succeeded`/`Skipped` are **not re-executed**
/// (so their side effects don't double-fire), and execution continues from the
/// first unfinished action. A step left `Running` at the crash is re-run (its
/// row reused) — the irreducible at-most-one-action duplicate, since the engine
/// can't know whether that in-flight action's effect committed; full exactly-once
/// would need the action itself to be idempotent. `job_id = None` always starts
/// fresh.
///
/// # Errors
/// As [`execute_with_state`].
async fn execute_for_job_with_state(
    state: &dyn ExecutionState,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    automation: &Automation,
    trigger: Option<Value>,
    job_id: Option<Uuid>,
) -> Result<AutomationRun, ExecutionError> {
    // Resume the job's in-progress run if one exists (a crash re-drive), else open
    // a fresh one — recording `job_id` so a *future* re-drive can find it. `prior`
    // maps each already-recorded step's ordinal → step, so completed ones are
    // skipped and an in-flight one is reused rather than re-`add_step`ed (which the
    // `(run_id, ordinal)` unique key would reject).
    let resumed = match job_id {
        Some(jid) => state.find_active_run_by_job(workspace_id, jid).await?,
        None => None,
    };
    // `run_grant_id` is the §19 authority this run executes under — sourced from the
    // **run**, not the automation's *current* `grant_id`. For a fresh run they're the
    // same (start_run snapshots the automation's grant). For a **resumed** run they
    // can differ (an admin re-pointed or deleted the automation's grant after the
    // crash); using the run's recorded grant keeps a run's authority consistent
    // start-to-finish and keeps the audit record honest — the run never executes
    // under an authority other than the one it is recorded as having run under.
    let (run_id, run_grant_id, mut prior): (
        AutomationRunId,
        Option<GrantId>,
        HashMap<i32, AutomationStep>,
    ) = match resumed {
        Some(rid) => {
            let run = state.get_run(workspace_id, rid).await?;
            let steps = state.list_steps(workspace_id, rid).await?;
            (
                rid,
                run.grant_id,
                steps.into_iter().map(|s| (s.ordinal, s)).collect(),
            )
        }
        None => {
            let run = state
                .start_run(workspace_id, automation, trigger.clone(), job_id)
                .await?;
            (run.id, run.grant_id, HashMap::new())
        }
    };

    // A node-graph automation stores its definition in the `spec` JSON's `"graph"`
    // key; its legacy `triggers`/`actions` columns are only the *compiled* dispatch
    // shadow (and `actions` may be empty), so the linear `AutomationSpec::parse`
    // (which rejects empty actions) is skipped for it. `graph` holds the parsed +
    // validated DAG; a present-but-malformed/invalid graph fails the run loudly.
    let graph: Option<Graph> = match Graph::from_spec(automation.spec.as_ref()) {
        Some(parsed) => match parsed.and_then(|g| g.validate().map(|()| g)) {
            Ok(g) => Some(g),
            Err(e) => {
                return state
                    .finish_run(
                        workspace_id,
                        run_id,
                        RunStatus::Failed,
                        Some(&format!("invalid graph spec: {e}")),
                    )
                    .await;
            }
        },
        None => None,
    };

    // A malformed stored automation is recorded as a failed run, not propagated.
    // For a graph automation the linear spec is not driven, so its parse is skipped.
    let spec = match (&graph, AutomationSpec::parse(automation)) {
        (Some(_), _) => None,
        (None, Ok(spec)) => Some(spec),
        (None, Err(e)) => {
            return state
                .finish_run(
                    workspace_id,
                    run_id,
                    RunStatus::Failed,
                    Some(&e.to_string()),
                )
                .await;
        }
    };

    // Resolve the run's §19 grant — the authority every action runs under. A grant
    // that can't be loaded **fails the run** (deny-safe: never run under the wrong
    // authority, and never *widen* to base authority because the grant vanished). The
    // run's `grant_id` has no FK, so it CAN dangle — a grant deleted mid-run resolves
    // to `NotFound` here and fails the run closed rather than silently running
    // unconstrained; a transient error likewise fails the run, which the job retries.
    let grant: Option<Grant> = match run_grant_id {
        Some(gid) => match state.resolve_grant(workspace_id, gid).await {
            Ok(g) => Some(g),
            Err(e) => {
                return state
                    .finish_run(
                        workspace_id,
                        run_id,
                        RunStatus::Failed,
                        Some(&format!("resolving grant {gid}: {e}")),
                    )
                    .await;
            }
        },
        None => None,
    };

    // §19 time window: a grant is authority only within its active window (if it
    // sets one), evaluated at run open. Outside it — expired or not yet valid — the
    // grant confers nothing, so fail the run closed (deny-safe); never run under an
    // out-of-window grant.
    if let Some(window) = grant
        .as_ref()
        .and_then(|g| g.constraints.time_window.as_ref())
    {
        let now = chrono::Utc::now();
        if !window.contains(now) {
            return state
                .finish_run(
                    workspace_id,
                    run_id,
                    RunStatus::Failed,
                    Some(&format!(
                        "grant's active time window [{}, {}) does not include now ({now}); \
                         refusing to run an out-of-window grant",
                        window.start, window.end
                    )),
                )
                .await;
        }
    }

    // The runtime enforces a grant's *capabilities* (via the runner) plus its
    // `dry_run` (simulated at dispatch) and `time_window` (above) constraints. The
    // **remaining** constraints (env allow-list, rate/cost caps, per-action approval)
    // await the policy engine (§19); running with them silently dropped is unsafe, so
    // refuse — deny-safe — until they're enforced.
    if grant
        .as_ref()
        .is_some_and(|g| g.constraints.has_unenforced())
    {
        return state
            .finish_run(
                workspace_id,
                run_id,
                RunStatus::Failed,
                Some(
                    "grant carries constraints (env/rate/cost/approval) the runtime does not yet \
                     enforce; refusing to run unconstrained",
                ),
            )
            .await;
    }

    // Node-graph automations (SOUL §11): a valid DAG was parsed from the `spec`
    // JSON's `"graph"` key, so drive *that* (data flows along edges) and return —
    // otherwise fall through to the legacy linear `actions` loop, 100% unchanged.
    // This sits AFTER the grant/time-window/unenforced-constraint gates above, so a
    // graph run is gated identically to a linear one.
    if let Some(graph) = graph {
        return run_graph(
            state,
            runner,
            code,
            workspace_id,
            run_id,
            &graph,
            trigger.as_ref(),
            grant.as_ref(),
            prior,
        )
        .await;
    }
    // Past here `spec` is always `Some` (a graph automation returned above).
    let spec = spec.expect("linear automation has a parsed spec");

    // Evaluate the automation's top-level `condition` (SOUL §11) before running any
    // action: a JS predicate over the trigger — the *same* condition language as a
    // graph Condition node — or a literal (see [`eval_condition`]). A **falsy**
    // condition runs no action (each is recorded `Skipped` and the run still
    // Succeeds — the gate fired cleanly, it just said "no"); a condition that
    // **can't be evaluated** fails the run closed (we never run actions on a
    // predicate we couldn't check). A condition-less automation (`None`) runs
    // unconditionally, exactly as before.
    let run_actions = match spec.condition.as_ref() {
        None => true,
        Some(cond) => {
            match eval_condition(code, cond, trigger.as_ref(), workspace_id, grant.as_ref()).await {
                Ok(pass) => pass,
                Err(e) => {
                    return state
                        .finish_run(workspace_id, run_id, RunStatus::Failed, Some(&e))
                        .await;
                }
            }
        }
    };

    // Idempotent-redelivery latch for the linear path (SOUL §11/§29), mirroring the
    // graph executor: a redelivering trigger — or an upstream write reporting the item
    // was already stored (`newly_written == false`) — auto-Skips the non-idempotent
    // actions that follow, so an at-least-once redelivery doesn't double-fire them.
    let mut redelivery = trigger
        .as_ref()
        .and_then(|t| t.get("redelivery"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut failure: Option<String> = None;
    for (ordinal, action) in spec.actions.iter().enumerate() {
        let ord = ordinal as i32;
        // Reuse a step recorded on a prior attempt: skip it if already done (don't
        // re-run the side effect), else re-run on the existing row.
        let step_id = match prior.remove(&ord) {
            Some(prev) if matches!(prev.status, StepStatus::Succeeded | StepStatus::Skipped) => {
                continue;
            }
            Some(prev) => prev.id,
            None => {
                let action_json = serde_json::to_value(action).unwrap_or(Value::Null);
                state
                    .add_step(workspace_id, run_id, ord, action_json)
                    .await?
                    .id
            }
        };
        // The condition gated the run off: record each action as `Skipped` (audit:
        // "the condition excluded it") instead of executing its side effect.
        if !run_actions {
            state
                .finish_step(workspace_id, step_id, StepStatus::Skipped, None, None)
                .await?;
            continue;
        }
        // Redelivery gate: auto-Skip a non-idempotent action on a redelivery (unless it
        // opts back in) — the linear twin of the graph executor's per-node gate.
        if redelivery && !action.kind.is_idempotent() && !rerun_on_redelivery(action) {
            state
                .finish_step(workspace_id, step_id, StepStatus::Skipped, None, None)
                .await?;
            continue;
        }
        let outcome = runner
            .run(workspace_id, action, trigger.as_ref(), grant.as_ref())
            .await;
        // Latch the run as a redelivery once a write reports the item was already
        // stored, so the non-idempotent actions after it auto-skip.
        if outcome
            .output
            .as_ref()
            .and_then(|v| v.get("newly_written"))
            .and_then(Value::as_bool)
            == Some(false)
        {
            redelivery = true;
        }
        state
            .finish_step(
                workspace_id,
                step_id,
                outcome.status,
                outcome.output,
                outcome.error.as_deref(),
            )
            .await?;
        if outcome.status == StepStatus::Failed {
            failure = Some(
                outcome
                    .error
                    .unwrap_or_else(|| format!("action {ordinal} failed")),
            );
            break;
        }
    }

    let status = if failure.is_some() {
        RunStatus::Failed
    } else {
        RunStatus::Succeeded
    };
    state
        .finish_run(workspace_id, run_id, status, failure.as_deref())
        .await
}

/// Evaluate a linear automation's top-level `condition` against the firing
/// `trigger` (SOUL §11), returning whether its actions should run.
///
/// A `{ "runtime": "js", "source": "…" }` object is a **code predicate**: it runs
/// through the [`CodeRunner`] with input `{ "trigger": <event>, "inputs": {} }` —
/// the *same* condition language a graph [`Condition`](NodeKind::Condition) node
/// uses, so there is one predicate language across the linear and graph surfaces,
/// not two — and its JSON result is taken as [`is_truthy`]. Any other JSON value
/// is a **literal** gate (`is_truthy` of it: a bare `true` runs; `false`/`null`/
/// `0`/`""`/`[]`/`{}` skip). A future richer (declarative) predicate language can
/// layer on by recognising its own shape ahead of this literal fallback.
///
/// # Errors
/// A code predicate that errors — syntax/runtime/timeout, or (under the Phase-A
/// [`FailCodeRunner`]) no runtime configured — returns the runner's message, and
/// the caller fails the run closed rather than running actions on an unchecked
/// predicate.
async fn eval_condition(
    code: &dyn CodeRunner,
    condition: &Value,
    trigger: Option<&Value>,
    workspace_id: WorkspaceId,
    grant: Option<&Grant>,
) -> Result<bool, String> {
    if let (Some(runtime), Some(source)) = (
        condition.get("runtime").and_then(Value::as_str),
        condition.get("source").and_then(Value::as_str),
    ) {
        let mut input = Map::new();
        input.insert("trigger".into(), trigger.cloned().unwrap_or(Value::Null));
        input.insert("inputs".into(), Value::Object(Map::new()));
        let result = code
            .run_code(runtime, source, &Value::Object(input), workspace_id, grant)
            .await?;
        return Ok(is_truthy(&result));
    }
    Ok(is_truthy(condition))
}

/// Whether a code/condition result is **truthy** (a Condition node routes its
/// `"true"` branch on truthy, `"false"` otherwise). Mirrors JS-ish coercion over
/// JSON: `false`/`null` and zero numbers and empty string/array/object are falsy;
/// everything else (incl. non-empty containers) is truthy.
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Drive a node-graph automation as a DAG (SOUL §11): topo-walk the graph passing
/// each node's output to its downstream nodes, recording **one durable step per
/// node** (reusing `add_step`/`finish_step`), gated identically to the linear loop.
///
/// Seeding: the entry trigger node(s) are those whose [`Trigger`](crate::Trigger)
/// **matches** the run's `trigger` event; if the run has no event (manual) or none
/// match, *all* trigger nodes are entries. A non-entry node executes only when it
/// sits on a **taken** branch — at least one upstream edge from a succeeded node
/// whose `from_port` was selected (a condition routes `"true"`/`"false"`); else it
/// is recorded `Skipped`. Each executed node receives
/// `{ "trigger": <event-or-null>, "inputs": { <upstream_id>: <output> } }`.
///
/// Crash-resume: a node already recorded `Succeeded`/`Skipped` on a prior attempt
/// (looked up by its topo-index `ordinal` in `prior`) is **not re-run** — its prior
/// output/branch is reused so downstream routing is identical. A `Failed` node
/// fails the run and stops, matching the linear loop.
#[allow(clippy::too_many_arguments)]
async fn run_graph(
    state: &dyn ExecutionState,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    run_id: AutomationRunId,
    graph: &Graph,
    trigger: Option<&Value>,
    grant: Option<&Grant>,
    mut prior: HashMap<i32, AutomationStep>,
) -> Result<AutomationRun, ExecutionError> {
    let order = match graph.topo_order() {
        Ok(o) => o,
        // `validate` already ran before dispatch, so this is unreachable in practice;
        // fail the run closed rather than panic if it ever isn't.
        Err(e) => {
            return state
                .finish_run(workspace_id, run_id, RunStatus::Failed, Some(&e))
                .await;
        }
    };

    // The firing event, typed, to seed entry triggers (best-effort: a trigger payload
    // that doesn't deserialize to a `TriggerEvent` means "no specific match", so all
    // trigger nodes seed — keeps a manual/opaque trigger from starving the graph).
    let event: Option<TriggerEvent> = trigger.and_then(|v| serde_json::from_value(v.clone()).ok());

    // Idempotent-redelivery gate (SOUL §11/§29). At-least-once collect delivery means
    // an already-processed item can fire a run again (the in-flight window a crash
    // opens between an item's `WriteEmail` and the ledger commit); on such a
    // **redelivery** the non-idempotent nodes (`LlmAgent` tokens, `LabelEmail`,
    // `Notify`, …) must NOT re-fire. The run is a redelivery iff the firing event says
    // so (`trigger.redelivery == true`) or — the durable, crash-surviving signal — an
    // upstream write reports the item was **already stored** (`newly_written == false`,
    // §29). Once set it latches for the rest of the run; downstream non-idempotent
    // action nodes then auto-Skip (see [`ActionKind::is_idempotent`]) and every node's
    // input envelope carries `redelivery` so a Condition/Code node can branch on it.
    let mut redelivery = trigger
        .and_then(|t| t.get("redelivery"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // Loop regions (SOUL §11): each ForEach head drives its body once per element.
    // `validate` already vetted these before dispatch; a stray error here degrades to
    // "no loops" rather than panicking. Body + LoopEnd nodes are **region-internal**:
    // the outer walk skips them (they run inside the ForEach), keyed by node id, while
    // the ForEach is dispatched at its own topo position (its upstreams already ran).
    let regions = graph.for_each_regions().unwrap_or_default();
    let region_by_foreach: HashMap<&str, (usize, &ForEachRegion)> = regions
        .iter()
        .enumerate()
        .map(|(i, r)| (r.for_each.as_str(), (i, r)))
        .collect();
    let region_internal: HashSet<&str> = regions
        .iter()
        .flat_map(|r| {
            r.body
                .iter()
                .map(String::as_str)
                .chain(std::iter::once(r.loop_end.as_str()))
        })
        .collect();

    // Per-node execution bookkeeping, keyed by node id:
    // - `output`: the value a succeeded node produced (feeds downstream `inputs`).
    // - `branch`: for a succeeded **condition** node, the single out-port taken
    //   (`"true"`/`"false"`); other succeeded nodes take every out-port (`None`).
    // - `succeeded`: the node ran to a non-failing terminal (its out-edges may fire).
    let mut output: HashMap<String, Value> = HashMap::new();
    let mut branch: HashMap<String, Option<String>> = HashMap::new();
    let mut succeeded: HashMap<String, bool> = HashMap::new();

    let mut failure: Option<String> = None;
    for (idx, node_id) in order.iter().enumerate() {
        let ord = idx as i32;
        let node = match graph.node(node_id) {
            Some(n) => n,
            None => continue,
        };

        // Region-internal nodes (a loop body + its LoopEnd) are driven by their
        // ForEach head, which runs them once per element — skip them in the outer walk.
        if region_internal.contains(node_id.as_str()) {
            continue;
        }

        // A ForEach head: run its whole loop region here (SOUL §11). It sits at its own
        // topo position (all its upstreams already produced output), records one durable
        // step for itself, and publishes the per-iteration results array on its LoopEnd
        // for the nodes downstream of the loop.
        if let NodeKind::ForEach { .. } = &node.kind {
            let &(region_index, region) = region_by_foreach
                .get(node_id.as_str())
                .expect("every for_each node has a resolved region");
            // Active iff some upstream edge into it was taken (never a trigger entry).
            let active = graph.edges.iter().any(|e| {
                e.to == *node_id
                    && succeeded.get(&e.from).copied().unwrap_or(false)
                    && match branch.get(&e.from) {
                        Some(Some(port)) => *port == e.from_port,
                        _ => true,
                    }
            });
            // Resume: reuse the ForEach's own step row. A prior Succeeded ForEach is
            // re-run so its LoopEnd output is restored — but the body steps inside are
            // reused by ordinal, so nothing re-executes (side effects don't repeat).
            let reuse_step_id = match prior.remove(&ord) {
                Some(prev) if prev.status == StepStatus::Skipped => continue,
                Some(prev) => Some(prev.id),
                None => None,
            };
            if !active {
                match reuse_step_id {
                    Some(id) => {
                        state
                            .finish_step(workspace_id, id, StepStatus::Skipped, None, None)
                            .await?;
                    }
                    None => {
                        let sid = state
                            .add_step(workspace_id, run_id, ord, step_action_json(node))
                            .await?
                            .id;
                        state
                            .finish_step(workspace_id, sid, StepStatus::Skipped, None, None)
                            .await?;
                    }
                }
                continue;
            }
            // The ForEach's input envelope (its upstream outputs), from which its
            // `source` array path is resolved.
            let mut inputs = Map::new();
            for up in graph.upstream(node_id) {
                if let Some(out) = output.get(&up) {
                    inputs.insert(up, out.clone());
                }
            }
            let mut fin = Map::new();
            fin.insert("trigger".into(), trigger.cloned().unwrap_or(Value::Null));
            fin.insert("inputs".into(), Value::Object(inputs));
            let foreach_input = Value::Object(fin);

            let step_id = match reuse_step_id {
                Some(id) => id,
                None => {
                    state
                        .add_step(workspace_id, run_id, ord, step_action_json(node))
                        .await?
                        .id
                }
            };
            let outcome = run_for_each(
                state,
                runner,
                code,
                workspace_id,
                run_id,
                graph,
                region,
                region_index,
                &foreach_input,
                &prior,
                grant,
                trigger,
                redelivery,
            )
            .await?;
            let out = (outcome.status == StepStatus::Succeeded).then(|| outcome.summary.clone());
            state
                .finish_step(
                    workspace_id,
                    step_id,
                    outcome.status,
                    out,
                    outcome.error.as_deref(),
                )
                .await?;
            match outcome.status {
                StepStatus::Succeeded => {
                    succeeded.insert(node_id.clone(), true);
                    output.insert(node_id.clone(), outcome.summary);
                    // Publish the loop's results on its LoopEnd for downstream nodes.
                    succeeded.insert(region.loop_end.clone(), true);
                    output.insert(region.loop_end.clone(), outcome.results);
                }
                StepStatus::Failed => {
                    failure = Some(
                        outcome
                            .error
                            .unwrap_or_else(|| format!("for_each {node_id} failed")),
                    );
                    break;
                }
                StepStatus::Skipped | StepStatus::Running => {}
            }
            continue;
        }

        // Is this node on a path that should execute? An entry trigger always is; any
        // other node is active iff some upstream edge into it was *taken* (its source
        // succeeded and the edge's `from_port` matches the source's branch decision).
        let is_entry = node.is_trigger()
            && match &event {
                // With a typed event, only the trigger(s) it matches seed.
                Some(ev) => match &node.kind {
                    NodeKind::Trigger { trigger } => trigger.matches(ev),
                    _ => false,
                },
                // No typed event → every trigger node seeds.
                None => true,
            };
        let on_taken_branch = graph.edges.iter().any(|e| {
            e.to == *node_id
                && succeeded.get(&e.from).copied().unwrap_or(false)
                && match branch.get(&e.from) {
                    Some(Some(port)) => *port == e.from_port,
                    _ => true,
                }
        });
        let active = is_entry || on_taken_branch;

        // Resume: a node already terminal (Succeeded/Skipped) on a prior attempt is
        // not re-run — its recorded output/branch is restored so downstream routing
        // is identical. A node left Failed/Running at the crash is re-run **on its
        // existing row** (its id is reused below rather than `add_step`-ed again,
        // which the `(run_id, ordinal)` unique key would reject).
        let reuse_step_id = match prior.remove(&ord) {
            Some(prev) if prev.status == StepStatus::Succeeded => {
                succeeded.insert(node_id.clone(), true);
                if let Some(out) = prev.output {
                    // A condition's recorded output reconstructs its taken branch.
                    if matches!(node.kind, NodeKind::Condition { .. }) {
                        branch.insert(node_id.clone(), Some(branch_port(&out).to_string()));
                    }
                    // Restore the redelivery latch from a resumed write's output, so a
                    // crash-resume gates the same downstream nodes a fresh run would.
                    if out.get("newly_written").and_then(Value::as_bool) == Some(false) {
                        redelivery = true;
                    }
                    output.insert(node_id.clone(), out);
                }
                continue;
            }
            Some(prev) if prev.status == StepStatus::Skipped => continue,
            Some(prev) => Some(prev.id),
            None => None,
        };

        // Redelivery gate (SOUL §11/§29): on a redelivery of an already-written item,
        // a non-idempotent action node is auto-Skipped so it doesn't double-fire —
        // unless it opts back in with `"rerun_on_redelivery": true`. The idempotent
        // write itself still runs (it must, to advance the collect cursor) and pure
        // transforms/reads still run. This never cascades a needed write off, because
        // `redelivery` is only set *by* an upstream write, so any keyed write is
        // either upstream (already ran) or not gated by this branch.
        let redelivery_skip = active
            && redelivery
            && matches!(&node.kind, NodeKind::Action { action }
                if !action.kind.is_idempotent() && !rerun_on_redelivery(action));

        // Not on any taken path (or gated off as a redelivery) → record a Skipped step
        // and move on (the run survives).
        if !active || redelivery_skip {
            match reuse_step_id {
                Some(id) => {
                    state
                        .finish_step(workspace_id, id, StepStatus::Skipped, None, None)
                        .await?;
                }
                None => {
                    let step_id = state
                        .add_step(workspace_id, run_id, ord, step_action_json(node))
                        .await?
                        .id;
                    state
                        .finish_step(workspace_id, step_id, StepStatus::Skipped, None, None)
                        .await?;
                }
            }
            continue;
        }

        // Build the node's input context: the firing event + each upstream node's
        // output (only upstreams that produced one appear).
        let mut inputs = Map::new();
        for up in graph.upstream(node_id) {
            if let Some(out) = output.get(&up) {
                inputs.insert(up, out.clone());
            }
        }
        let mut input = Map::new();
        input.insert("trigger".into(), trigger.cloned().unwrap_or(Value::Null));
        input.insert("inputs".into(), Value::Object(inputs));
        // Expose the run's redelivery latch so a Condition/Code node can branch on it
        // (`input.redelivery`) — the general seam beyond the built-in action auto-skip.
        input.insert("redelivery".into(), Value::Bool(redelivery));
        let input = Value::Object(input);

        // One durable step per node; reuse a prior Running/Failed row if resuming
        // (re-`add_step` would collide on the `(run_id, ordinal)` unique key).
        let step_id = match reuse_step_id {
            Some(id) => id,
            None => {
                state
                    .add_step(workspace_id, run_id, ord, step_action_json(node))
                    .await?
                    .id
            }
        };

        // Execute by kind, producing a terminal `StepStatus` + output/error.
        let (status, out, err): (StepStatus, Option<Value>, Option<String>) = match &node.kind {
            NodeKind::Trigger { .. } => {
                // A trigger node's output is the firing event (or null for a manual run).
                (
                    StepStatus::Succeeded,
                    Some(trigger.cloned().unwrap_or(Value::Null)),
                    None,
                )
            }
            NodeKind::Action { action } => {
                let outcome = runner.run(workspace_id, action, Some(&input), grant).await;
                (outcome.status, outcome.output, outcome.error)
            }
            NodeKind::Code { runtime, source } => {
                match code
                    .run_code(runtime, source, &input, workspace_id, grant)
                    .await
                {
                    Ok(v) => (StepStatus::Succeeded, Some(v), None),
                    Err(e) => (StepStatus::Failed, None, Some(e)),
                }
            }
            NodeKind::Condition { runtime, source } => {
                match code
                    .run_code(runtime, source, &input, workspace_id, grant)
                    .await
                {
                    Ok(v) => {
                        let taken = branch_port(&v);
                        branch.insert(node_id.clone(), Some(taken.to_string()));
                        (StepStatus::Succeeded, Some(v), None)
                    }
                    Err(e) => (StepStatus::Failed, None, Some(e)),
                }
            }
            // Loop nodes are handled before this dispatch: a ForEach runs its region
            // in the dedicated block above (which `continue`s), and a LoopEnd is
            // region-internal and skipped at the top of the loop — so neither reaches here.
            NodeKind::ForEach { .. } | NodeKind::LoopEnd { .. } => {
                unreachable!("loop nodes are handled before the per-node dispatch")
            }
        };

        state
            .finish_step(workspace_id, step_id, status, out.clone(), err.as_deref())
            .await?;

        match status {
            StepStatus::Succeeded => {
                succeeded.insert(node_id.clone(), true);
                if let Some(v) = out {
                    // A write reporting the item was **already stored**
                    // (`newly_written == false`) latches the run as a redelivery, so
                    // every downstream non-idempotent action node auto-skips (SOUL §29).
                    if v.get("newly_written").and_then(Value::as_bool) == Some(false) {
                        redelivery = true;
                    }
                    output.insert(node_id.clone(), v);
                }
            }
            StepStatus::Failed => {
                failure = Some(err.unwrap_or_else(|| format!("node {node_id} failed")));
                break;
            }
            // A node runner reporting Skipped just doesn't propagate to its downstream.
            StepStatus::Skipped | StepStatus::Running => {}
        }
    }

    let status = if failure.is_some() {
        RunStatus::Failed
    } else {
        RunStatus::Succeeded
    };
    state
        .finish_run(workspace_id, run_id, status, failure.as_deref())
        .await
}

/// Whether an action node opts a non-idempotent action back into running on an
/// at-least-once **redelivery** (SOUL §11/§29) via an action param
/// `"rerun_on_redelivery": true`. The engine otherwise auto-Skips non-idempotent
/// actions on a redelivery (see [`ActionKind::is_idempotent`]); this is the escape
/// hatch for an author who *wants* the action to re-fire every time.
fn rerun_on_redelivery(action: &Action) -> bool {
    action
        .params
        .get("rerun_on_redelivery")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The out-port a condition's result routes down: `"true"` when truthy, else
/// `"false"` (see [`is_truthy`]).
fn branch_port(v: &Value) -> &'static str {
    if is_truthy(v) {
        "true"
    } else {
        "false"
    }
}

/// The base ordinal for loop-body steps — kept well above any node's topo index
/// (a graph has far fewer than a million nodes) so region and outer step ordinals
/// never collide on the durable `(run_id, ordinal)` key.
const REGION_ORD_BASE: i64 = 1_000_000;
/// Per-region ordinal stride: `MAX_LOOP_ITERATIONS * MAX_LOOP_BODY_NODES`, so each
/// region owns a disjoint block and `(iteration, body_pos)` maps to a unique offset.
const REGION_ORD_STRIDE: i64 = (MAX_LOOP_ITERATIONS * MAX_LOOP_BODY_NODES) as i64;

/// The result of running one [`ForEachRegion`]: the ForEach node's own summary
/// output, the per-iteration results array published on its LoopEnd, and a
/// terminal status (`Failed` carries the message).
struct ForEachOutcome {
    status: StepStatus,
    /// The ForEach node's own step output (e.g. `{ "iterations": n }`).
    summary: Value,
    /// The array of per-iteration body results, published on the LoopEnd.
    results: Value,
    error: Option<String>,
}

/// Resolve a dotted `path` (`inputs.web_search.searches.rust.results`, `trigger.kind`,
/// `inputs.list.0`) against `ctx`: object keys traverse maps, all-digit segments
/// index arrays. Mirrors the API's action-param templating resolver.
fn resolve_path<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = ctx;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Run a [`ForEachRegion`] (SOUL §11): resolve its `source` array from the
/// ForEach's input envelope and run the body once per element, binding `item`
/// (and optional `index`) as top-level template variables. Records **one durable
/// step per (body node, iteration)** with a deterministic region ordinal, so a
/// crash resumes by reusing succeeded body steps (nothing re-executes). Stops at
/// the first failing body step, matching the DAG's stop-on-first-failure.
#[allow(clippy::too_many_arguments)]
async fn run_for_each(
    state: &dyn ExecutionState,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    run_id: AutomationRunId,
    graph: &Graph,
    region: &ForEachRegion,
    region_index: usize,
    foreach_input: &Value,
    prior: &HashMap<i32, AutomationStep>,
    grant: Option<&Grant>,
    trigger: Option<&Value>,
    redelivery: bool,
) -> Result<ForEachOutcome, ExecutionError> {
    // The array to iterate. Absent/null → zero iterations (an empty loop, not an
    // error). A present non-array is a hard error (fail the loop loudly).
    let items: Vec<Value> = match resolve_path(foreach_input, &region.source) {
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => {
            return Ok(ForEachOutcome {
                status: StepStatus::Failed,
                summary: Value::Null,
                results: Value::Null,
                error: Some(format!(
                    "for_each source '{}' did not resolve to an array",
                    region.source
                )),
            })
        }
    };
    let cap = region
        .max_iterations
        .unwrap_or(MAX_LOOP_ITERATIONS)
        .min(MAX_LOOP_ITERATIONS);
    if items.len() > cap {
        return Ok(ForEachOutcome {
            status: StepStatus::Failed,
            summary: Value::Null,
            results: Value::Null,
            error: Some(format!(
                "for_each source '{}' has {} items, exceeding the limit of {cap}",
                region.source,
                items.len()
            )),
        });
    }

    // The ForEach's upstream outputs are visible to every body node (as `inputs.*`);
    // in-region node outputs layer on per iteration.
    let base_inputs: Map<String, Value> = foreach_input
        .get("inputs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let body_set: HashSet<&str> = region.body.iter().map(String::as_str).collect();

    let mut results: Vec<Value> = Vec::with_capacity(items.len());
    for (i, element) in items.iter().enumerate() {
        // Per-iteration in-region node bookkeeping.
        let mut local_out: HashMap<String, Value> = HashMap::new();
        let mut local_branch: HashMap<String, Option<String>> = HashMap::new();
        let mut local_succeeded: HashMap<String, bool> = HashMap::new();

        for (pos, body_id) in region.body.iter().enumerate() {
            let node = match graph.node(body_id) {
                Some(n) => n,
                None => continue,
            };
            let ord_i64 = REGION_ORD_BASE
                + region_index as i64 * REGION_ORD_STRIDE
                + i as i64 * MAX_LOOP_BODY_NODES as i64
                + pos as i64;
            if ord_i64 > i64::from(i32::MAX) {
                return Ok(ForEachOutcome {
                    status: StepStatus::Failed,
                    summary: Value::Null,
                    results: Value::Null,
                    error: Some("for_each step ordinal space exhausted".to_string()),
                });
            }
            let ord = ord_i64 as i32;

            // Active iff a region-entry (driven by the ForEach) or on a taken branch
            // from an in-region upstream that succeeded this iteration.
            let is_region_entry = graph
                .edges
                .iter()
                .any(|e| e.from == region.for_each && e.to == *body_id);
            let on_taken = graph.edges.iter().any(|e| {
                e.to == *body_id
                    && body_set.contains(e.from.as_str())
                    && local_succeeded.get(&e.from).copied().unwrap_or(false)
                    && match local_branch.get(&e.from) {
                        Some(Some(port)) => *port == e.from_port,
                        _ => true,
                    }
            });
            let active = is_region_entry || on_taken;

            // Resume: a body step already Succeeded/Skipped is restored, not re-run.
            let reuse_step_id = match prior.get(&ord) {
                Some(prev) if prev.status == StepStatus::Succeeded => {
                    local_succeeded.insert(body_id.clone(), true);
                    if let Some(out) = &prev.output {
                        if matches!(node.kind, NodeKind::Condition { .. }) {
                            local_branch
                                .insert(body_id.clone(), Some(branch_port(out).to_string()));
                        }
                        local_out.insert(body_id.clone(), out.clone());
                    }
                    continue;
                }
                Some(prev) if prev.status == StepStatus::Skipped => continue,
                Some(prev) => Some(prev.id),
                None => None,
            };

            // The per-iteration step action carries the node identity + iteration index.
            let step_action = {
                let mut sa = step_action_json(node);
                if let Value::Object(m) = &mut sa {
                    m.insert("iteration".into(), Value::from(i));
                    m.insert("for_each".into(), Value::String(region.for_each.clone()));
                }
                sa
            };

            // Redelivery gate inside a loop (SOUL §11/§29): if the whole run is a
            // redelivery, a non-idempotent body action auto-Skips too (unless it opts
            // in with `"rerun_on_redelivery": true`) — no double-firing per element.
            let redelivery_skip = active
                && redelivery
                && matches!(&node.kind, NodeKind::Action { action }
                    if !action.kind.is_idempotent() && !rerun_on_redelivery(action));

            if !active || redelivery_skip {
                match reuse_step_id {
                    Some(id) => {
                        state
                            .finish_step(workspace_id, id, StepStatus::Skipped, None, None)
                            .await?;
                    }
                    None => {
                        let sid = state
                            .add_step(workspace_id, run_id, ord, step_action)
                            .await?
                            .id;
                        state
                            .finish_step(workspace_id, sid, StepStatus::Skipped, None, None)
                            .await?;
                    }
                }
                continue;
            }

            // Body input envelope: the ForEach's upstream outputs + this iteration's
            // in-region upstream outputs, plus the loop `item`/`index` variables.
            let mut inputs = base_inputs.clone();
            for up in graph.upstream(body_id) {
                if let Some(out) = local_out.get(&up) {
                    inputs.insert(up, out.clone());
                }
            }
            let mut env = Map::new();
            env.insert("trigger".into(), trigger.cloned().unwrap_or(Value::Null));
            env.insert("inputs".into(), Value::Object(inputs));
            env.insert("redelivery".into(), Value::Bool(redelivery));
            env.insert(region.item.clone(), element.clone());
            if let Some(ix) = &region.index {
                env.insert(ix.clone(), Value::from(i));
            }
            let env = Value::Object(env);

            let step_id = match reuse_step_id {
                Some(id) => id,
                None => {
                    state
                        .add_step(workspace_id, run_id, ord, step_action)
                        .await?
                        .id
                }
            };

            let (status, out, err): (StepStatus, Option<Value>, Option<String>) = match &node.kind {
                NodeKind::Action { action } => {
                    let outcome = runner.run(workspace_id, action, Some(&env), grant).await;
                    (outcome.status, outcome.output, outcome.error)
                }
                NodeKind::Code { runtime, source } => {
                    match code
                        .run_code(runtime, source, &env, workspace_id, grant)
                        .await
                    {
                        Ok(v) => (StepStatus::Succeeded, Some(v), None),
                        Err(e) => (StepStatus::Failed, None, Some(e)),
                    }
                }
                NodeKind::Condition { runtime, source } => {
                    match code
                        .run_code(runtime, source, &env, workspace_id, grant)
                        .await
                    {
                        Ok(v) => {
                            local_branch.insert(body_id.clone(), Some(branch_port(&v).to_string()));
                            (StepStatus::Succeeded, Some(v), None)
                        }
                        Err(e) => (StepStatus::Failed, None, Some(e)),
                    }
                }
                // Validation guarantees a loop body holds none of these.
                NodeKind::Trigger { .. } | NodeKind::ForEach { .. } | NodeKind::LoopEnd { .. } => {
                    unreachable!("loop body contains no trigger or nested loop node")
                }
            };

            state
                .finish_step(workspace_id, step_id, status, out.clone(), err.as_deref())
                .await?;

            match status {
                StepStatus::Succeeded => {
                    local_succeeded.insert(body_id.clone(), true);
                    if let Some(v) = out {
                        local_out.insert(body_id.clone(), v);
                    }
                }
                StepStatus::Failed => {
                    return Ok(ForEachOutcome {
                        status: StepStatus::Failed,
                        summary: Value::Null,
                        results: Value::Null,
                        error: Some(err.unwrap_or_else(|| {
                            format!("for_each body node {body_id} failed at iteration {i}")
                        })),
                    })
                }
                StepStatus::Skipped | StepStatus::Running => {}
            }
        }

        // The iteration's result = the outputs of the LoopEnd's in-region upstreams,
        // keyed by node id (so downstream reads `inputs.<loop_end>.<i>.<node>`).
        let mut result = Map::new();
        for e in graph.edges.iter().filter(|e| e.to == region.loop_end) {
            if let Some(out) = local_out.get(&e.from) {
                result.insert(e.from.clone(), out.clone());
            }
        }
        results.push(Value::Object(result));
    }

    Ok(ForEachOutcome {
        status: StepStatus::Succeeded,
        summary: serde_json::json!({ "iterations": results.len() }),
        results: Value::Array(results),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `CodeRunner` that echoes a fixed result, recording the input it saw — so a
    /// test can assert both the gate decision and that the trigger reached the
    /// predicate as `input.trigger`.
    struct EchoCode {
        result: Value,
        seen: std::sync::Mutex<Option<Value>>,
    }

    #[async_trait]
    impl CodeRunner for EchoCode {
        async fn run_code(
            &self,
            _runtime: &str,
            _source: &str,
            input: &Value,
            _workspace_id: WorkspaceId,
            _grant: Option<&Grant>,
        ) -> Result<Value, String> {
            *self.seen.lock().unwrap() = Some(input.clone());
            Ok(self.result.clone())
        }
    }

    #[test]
    fn resolve_path_traverses_objects_and_arrays() {
        let ctx = json!({
            "trigger": { "kind": "webhook" },
            "inputs": { "src": { "results": [ { "url": "a" }, { "url": "b" } ] } }
        });
        assert_eq!(
            resolve_path(&ctx, "inputs.src.results"),
            Some(&json!([ { "url": "a" }, { "url": "b" } ]))
        );
        assert_eq!(
            resolve_path(&ctx, "inputs.src.results.1.url"),
            Some(&json!("b"))
        );
        assert_eq!(resolve_path(&ctx, "trigger.kind"), Some(&json!("webhook")));
        // Missing keys / out-of-range indices / scalar traversal → None.
        assert_eq!(resolve_path(&ctx, "inputs.nope"), None);
        assert_eq!(resolve_path(&ctx, "inputs.src.results.9"), None);
        assert_eq!(resolve_path(&ctx, "trigger.kind.deeper"), None);
        // Empty path returns the root.
        assert_eq!(resolve_path(&ctx, ""), Some(&ctx));
    }

    #[tokio::test]
    async fn condition_literal_gates_on_truthiness() {
        // A literal condition never consults the code runner, so `FailCodeRunner`
        // (which errors on any code) proves no code path is taken.
        let run = FailCodeRunner;
        for (v, pass) in [
            (json!(true), true),
            (json!(false), false),
            (json!(null), false),
            (json!(0), false),
            (json!(1), true),
            (json!(""), false),
            (json!("go"), true),
            (json!([]), false),
            (json!({}), false),
            (json!({ "x": 1 }), true), // a non-empty object is truthy
        ] {
            assert_eq!(
                eval_condition(&run, &v, None, WorkspaceId::new(), None).await,
                Ok(pass),
                "literal {v} should gate to {pass}"
            );
        }
    }

    #[tokio::test]
    async fn condition_code_predicate_uses_runner_result_and_sees_the_trigger() {
        let cond = json!({ "runtime": "js", "source": "return input.trigger.ok;" });
        let trigger = json!({ "kind": "webhook", "ok": true });

        // Truthy runner result ⇒ run actions; the trigger is threaded in as
        // `input.trigger` (and `inputs` is an empty object, mirroring a graph node).
        let truthy = EchoCode {
            result: json!(true),
            seen: std::sync::Mutex::new(None),
        };
        assert_eq!(
            eval_condition(&truthy, &cond, Some(&trigger), WorkspaceId::new(), None).await,
            Ok(true)
        );
        let seen = truthy.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen["trigger"], trigger);
        assert_eq!(seen["inputs"], json!({}));

        // Falsy runner result ⇒ skip actions.
        let falsy = EchoCode {
            result: json!(0),
            seen: std::sync::Mutex::new(None),
        };
        assert_eq!(
            eval_condition(&falsy, &cond, Some(&trigger), WorkspaceId::new(), None).await,
            Ok(false)
        );
    }

    #[tokio::test]
    async fn condition_code_predicate_error_is_propagated_to_fail_closed() {
        // Under the Phase-A FailCodeRunner a code predicate can't be evaluated, so
        // `eval_condition` returns Err — the caller fails the run closed rather than
        // running actions on an unchecked predicate.
        let cond = json!({ "runtime": "js", "source": "return true;" });
        let err = eval_condition(&FailCodeRunner, &cond, None, WorkspaceId::new(), None)
            .await
            .expect_err("no runtime configured ⇒ Err");
        assert!(err.contains("no code runtime configured"), "got: {err}");
    }
}
