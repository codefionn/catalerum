//! Integration test: the automation [`execute`] driver (SOUL §11) records durable
//! run/step state around a pluggable [`ActionRunner`]. Happy path, stop-on-first-
//! failure, skipped-steps-don't-fail-the-run, and an invalid spec → failed run.
//!
//! DB-gated like the store tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use catalerum_automation::{
    execute, execute_for_job, Action, ActionOutcome, ActionRunner, CodeRunner, FailCodeRunner,
};
use catalerum_core::model::{RunStatus, StepStatus};
use catalerum_core::WorkspaceId;
use catalerum_store::{NewAutomation, Store};
use serde_json::{json, Value};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// An [`ActionRunner`] that returns a scripted outcome per call (by order),
/// defaulting to success past the script's end, and records the kinds it saw.
struct ScriptedRunner {
    outcomes: Vec<ActionOutcome>,
    idx: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl ScriptedRunner {
    fn new(outcomes: Vec<ActionOutcome>) -> Self {
        Self {
            outcomes,
            idx: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl ActionRunner for ScriptedRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        action: &Action,
        _trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        let i = self.idx.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(format!("{:?}", action.kind));
        self.outcomes
            .get(i)
            .cloned()
            .unwrap_or_else(|| ActionOutcome::succeeded(None))
    }
}

fn auto(name: &str, actions: Vec<Value>) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
        condition: None,
        actions,
        spec: None,
        grant_id: None,
    }
}

/// Like [`auto`] but with a top-level `condition` predicate (SOUL §11).
fn auto_cond(name: &str, condition: Value, actions: Vec<Value>) -> NewAutomation {
    NewAutomation {
        condition: Some(condition),
        ..auto(name, actions)
    }
}

#[tokio::test]
async fn executor_drives_runs_and_records_steps() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping executor test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("exec", &format!("exec-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // 1. Happy path: every action succeeds → run Succeeded, steps in order with output.
    let happy = store
        .automations()
        .create(
            ws.id,
            &auto(
                "happy",
                vec![
                    json!({ "kind": "llm_agent", "skills": ["weekly-review"] }),
                    json!({ "kind": "summarize" }),
                ],
            ),
        )
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![
        ActionOutcome::succeeded(Some(json!({ "note_id": "abc" }))),
        ActionOutcome::succeeded(None),
    ]);
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &happy,
        Some(json!({ "kind": "schedule" })),
    )
    .await
    .expect("execute happy");
    assert_eq!(run.status, RunStatus::Succeeded);
    assert!(run.finished_at.is_some());
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].status, StepStatus::Succeeded);
    assert_eq!(steps[0].action["kind"], json!("llm_agent"));
    assert_eq!(steps[0].output.as_ref().unwrap()["note_id"], json!("abc"));
    assert_eq!(steps[1].status, StepStatus::Succeeded);
    assert_eq!(runner.calls(), 2, "both actions ran in order");

    // 2. A failing action stops the run; later actions never execute.
    let sad = store
        .automations()
        .create(
            ws.id,
            &auto(
                "sad",
                vec![
                    json!({ "kind": "notify", "channel": "matrix" }),
                    json!({ "kind": "summarize" }),
                ],
            ),
        )
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![ActionOutcome::failed("channel offline")]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &sad, None)
        .await
        .expect("execute sad");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("channel offline"));
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 1, "execution stopped at the first failure");
    assert_eq!(steps[0].status, StepStatus::Failed);
    // The failure detail is recorded on the step row itself, not just the run.
    assert_eq!(steps[0].error.as_deref(), Some("channel offline"));
    assert!(run.finished_at.is_some());
    assert_eq!(runner.calls(), 1, "only the first action was attempted");

    // 2b. A *mid-run* failure: an earlier success is preserved, the failing step
    // is recorded, and the action after it never runs.
    let mid = store
        .automations()
        .create(
            ws.id,
            &auto(
                "mid",
                vec![
                    json!({ "kind": "summarize" }),
                    json!({ "kind": "notify", "channel": "matrix" }),
                    json!({ "kind": "create_note" }),
                ],
            ),
        )
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![
        ActionOutcome::succeeded(Some(json!({ "n": 1 }))),
        ActionOutcome::failed("boom"),
    ]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &mid, None)
        .await
        .expect("execute mid");
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.error.as_deref(), Some("boom"));
    assert_eq!(runner.calls(), 2, "the third action never ran");
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 2, "only the attempted steps are recorded");
    assert_eq!(
        steps[0].status,
        StepStatus::Succeeded,
        "the earlier success is preserved"
    );
    assert_eq!(steps[0].ordinal, 0);
    assert_eq!(steps[1].status, StepStatus::Failed);
    assert_eq!(steps[1].ordinal, 1);

    // 3. A skipped step does not fail the run; execution continues.
    let skip = store
        .automations()
        .create(
            ws.id,
            &auto(
                "skip",
                vec![
                    json!({ "kind": "summarize" }),
                    json!({ "kind": "notify", "channel": "matrix" }),
                ],
            ),
        )
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![
        ActionOutcome::skipped(),
        ActionOutcome::succeeded(None),
    ]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &skip, None)
        .await
        .expect("execute skip");
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "a skipped step does not fail the run"
    );
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].status, StepStatus::Skipped);
    assert_eq!(steps[1].status, StepStatus::Succeeded);

    // 4. A malformed stored automation (no actions) is recorded as a failed run.
    let empty = store
        .automations()
        .create(ws.id, &auto("empty", vec![]))
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &empty, None)
        .await
        .expect("execute empty");
    assert_eq!(run.status, RunStatus::Failed);
    assert!(run.error.as_deref().unwrap().contains("no actions"));
    assert!(store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(runner.calls(), 0, "an invalid spec runs no actions");

    // 5. Executing in the wrong workspace is rejected before any action runs —
    // `start_run`'s tenancy guard surfaces as the sole out-of-band error (§18).
    let ws2 = store
        .workspaces()
        .create("exec-b", &format!("exec-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws2");
    let runner = ScriptedRunner::new(vec![]);
    let denied = execute(&store, &runner, &FailCodeRunner, ws2.id, &happy, None).await;
    assert!(matches!(denied, Err(catalerum_store::StoreError::NotFound)));
    assert_eq!(
        runner.calls(),
        0,
        "no action runs for a run that never opened"
    );
}

/// A re-driven `run_automation` job (a worker crashed mid-run) **resumes** its
/// existing run rather than starting a fresh one and re-running completed actions
/// (SOUL §5/§11). Models the crash by hand-recording a run + a finished step 0,
/// then re-driving the same `job_id`: only action 1 re-runs, no duplicate run.
#[tokio::test]
async fn execute_for_job_resumes_a_crashed_run_without_re_running_done_steps() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping resume test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("resume", &format!("resume-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // Two distinct actions so we can tell which ran.
    let automation = store
        .automations()
        .create(
            ws.id,
            &auto(
                "two-step",
                vec![json!({ "kind": "summarize" }), json!({ "kind": "notify" })],
            ),
        )
        .await
        .unwrap();

    let runs = store.automation_runs();
    let job_id = uuid::Uuid::new_v4();
    let trigger = json!({ "kind": "schedule", "fired_at": "2026-06-15T09:00:00Z" });
    // Crash simulation: open the run for `job_id`, finish step 0, never reach step 1.
    let run = runs
        .start_run(
            ws.id,
            automation.id,
            None,
            Some(trigger.clone()),
            Some(job_id),
        )
        .await
        .unwrap();
    let step0 = runs
        .add_step(ws.id, run.id, 0, json!({ "kind": "summarize" }))
        .await
        .unwrap();
    runs.finish_step(ws.id, step0.id, StepStatus::Succeeded, None, None)
        .await
        .unwrap();

    // Re-drive the SAME job → resume.
    let runner = ScriptedRunner::new(vec![]);
    let finished = execute_for_job(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        Some(trigger),
        Some(job_id),
    )
    .await
    .unwrap();

    assert_eq!(finished.id, run.id, "resumed the same run, not a fresh one");
    assert_eq!(finished.status, RunStatus::Succeeded);
    assert_eq!(runner.calls(), 1, "only the unfinished action (1) re-ran");
    assert_eq!(
        runner.seen.lock().unwrap().as_slice(),
        &["Notify".to_string()],
        "step 0 (Summarize) was NOT re-run — no double side-effect"
    );
    // Exactly one run for the automation (no duplicate fresh run), with both steps done.
    assert_eq!(
        runs.list_runs(ws.id, automation.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let steps = runs.list_steps(ws.id, run.id).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert!(steps.iter().all(|s| s.status == StepStatus::Succeeded));

    // And `find_active_run_by_job` no longer returns it (now terminal).
    assert!(runs
        .find_active_run_by_job(ws.id, job_id)
        .await
        .unwrap()
        .is_none());
}

/// §19 deny-safe resume: a run executes under the grant it **opened** with, not the
/// automation's *current* grant. If that grant is deleted while the run is crashed,
/// the re-drive must **fail the run closed** — never silently widen to base authority
/// because the automation's live `grant_id` was nulled by the grant's deletion. This
/// also keeps the audit record (`run.grant_id`) and the execution authority in
/// agreement for the run's whole lifetime.
#[tokio::test]
async fn resume_uses_the_runs_recorded_grant_and_fails_closed_if_it_was_deleted() {
    use catalerum_core::capability::{Action, Capability, Constraints, Resource};

    let Some(url) = test_db_url() else {
        eprintln!("skipping resume-grant test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "resumegrant",
            &format!("resumegrant-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");

    // A grant, and an automation that runs under it.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "powers",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &Constraints::default(),
        )
        .await
        .expect("grant");
    let mut spec = auto(
        "two-step",
        vec![json!({ "kind": "summarize" }), json!({ "kind": "notify" })],
    );
    spec.grant_id = Some(grant.id);
    let automation = store.automations().create(ws.id, &spec).await.unwrap();

    // Crash simulation: open the run UNDER the grant for `job_id`, finish step 0.
    let runs = store.automation_runs();
    let job_id = uuid::Uuid::new_v4();
    let run = runs
        .start_run(
            ws.id,
            automation.id,
            automation.grant_id,
            None,
            Some(job_id),
        )
        .await
        .unwrap();
    assert_eq!(run.grant_id, Some(grant.id));
    let step0 = runs
        .add_step(ws.id, run.id, 0, json!({ "kind": "summarize" }))
        .await
        .unwrap();
    runs.finish_step(ws.id, step0.id, StepStatus::Succeeded, None, None)
        .await
        .unwrap();

    // The grant is deleted while the run is crashed → the automation's LIVE link nulls.
    assert!(store.grants().delete(ws.id, grant.id).await.unwrap());
    let reloaded = store.automations().get(ws.id, automation.id).await.unwrap();
    assert_eq!(
        reloaded.grant_id, None,
        "the automation's live grant link was nulled"
    );

    // Re-drive with the freshly-reloaded automation (grant_id now None) — exactly what
    // the reconciler/job would pass. The resume must NOT run the remaining step under
    // base authority; it resolves the run's recorded grant (deleted) → fails closed.
    let runner = ScriptedRunner::new(vec![]);
    let finished = execute_for_job(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &reloaded,
        None,
        Some(job_id),
    )
    .await
    .unwrap();

    assert_eq!(finished.id, run.id, "resumed the same run");
    assert_eq!(
        finished.status,
        RunStatus::Failed,
        "a run whose grant was deleted mid-flight fails closed on resume — never widens to base"
    );
    assert_eq!(
        runner.calls(),
        0,
        "no remaining action runs once the recorded grant cannot be resolved"
    );
    assert_eq!(
        finished.grant_id,
        Some(grant.id),
        "the audit record still names the grant the run actually opened under"
    );
}

/// §19 time-window enforcement: a grant is authority only within its active window.
/// A run under an **expired** (or not-yet-valid) window fails closed; a run under an
/// **active** window proceeds. Windows are absolute, so the assertions are
/// deterministic regardless of when the test runs.
#[tokio::test]
async fn a_grant_outside_its_active_time_window_fails_the_run_closed() {
    use catalerum_core::capability::{Action, Capability, Constraints, Resource};

    let Some(url) = test_db_url() else {
        eprintln!("skipping time-window test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("timewin", &format!("timewin-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // Build a grant + an automation under it, returning the finalized run + call count.
    let run_under = |store: Store, ws_id, name: &'static str, constraints: Constraints| async move {
        let caps = [Capability::new(Action::Write, Resource::domain("notes"))];
        let grant = store
            .grants()
            .upsert(ws_id, name, &caps, &constraints)
            .await
            .expect("grant");
        let mut spec = auto(name, vec![json!({ "kind": "summarize" })]);
        spec.grant_id = Some(grant.id);
        let automation = store.automations().create(ws_id, &spec).await.unwrap();
        let runner = ScriptedRunner::new(vec![]);
        let run = execute(&store, &runner, &FailCodeRunner, ws_id, &automation, None)
            .await
            .unwrap();
        (run, runner.calls())
    };

    // An expired window (entirely in the past) → run fails closed, no action runs.
    let expired: Constraints = serde_json::from_value(json!({
        "time_window": { "start": "2020-01-01T00:00:00Z", "end": "2020-01-02T00:00:00Z" }
    }))
    .unwrap();
    let (run, calls) = run_under(store.clone(), ws.id, "expired", expired).await;
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "an expired-window grant fails closed"
    );
    assert_eq!(calls, 0, "no action runs under an out-of-window grant");

    // A not-yet-valid window (entirely in the future) → also fails closed.
    let future: Constraints = serde_json::from_value(json!({
        "time_window": { "start": "2999-01-01T00:00:00Z", "end": "2999-01-02T00:00:00Z" }
    }))
    .unwrap();
    let (run, calls) = run_under(store.clone(), ws.id, "future", future).await;
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "a not-yet-valid-window grant fails closed"
    );
    assert_eq!(calls, 0);

    // An active window (spans now) → the grant is in force, the run proceeds.
    let active: Constraints = serde_json::from_value(json!({
        "time_window": { "start": "2020-01-01T00:00:00Z", "end": "2999-01-01T00:00:00Z" }
    }))
    .unwrap();
    let (run, calls) = run_under(store.clone(), ws.id, "active", active).await;
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "an active-window grant runs"
    );
    assert_eq!(calls, 1, "the action ran under the in-window grant");

    // An ACTIVE window that ALSO carries a still-unenforced constraint (`rate_limit`)
    // must STILL fail closed: passing the window check must not "consume" the grant
    // and skip the has_unenforced gate. (`rate_limit` is used here, not `cost_limit`,
    // because `cost_limit` is now ENFORCED in the agent loop — see
    // `Constraints::has_unenforced` — so it no longer fails closed.)
    let active_but_capped: Constraints = serde_json::from_value(json!({
        "time_window": { "start": "2020-01-01T00:00:00Z", "end": "2999-01-01T00:00:00Z" },
        "rate_limit": 5
    }))
    .unwrap();
    let (run, calls) = run_under(store.clone(), ws.id, "capped", active_but_capped).await;
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "in-window but rate_limit unenforced → still fails closed (no has_unenforced bypass)"
    );
    assert_eq!(calls, 0);
}

/// §19 time-window enforcement is re-evaluated on the **resume** path too: a run that
/// opened under a grant whose window has since closed fails closed when re-driven —
/// the window gate is not skipped just because the run already exists.
#[tokio::test]
async fn resume_re_evaluates_the_time_window_and_fails_closed_when_expired() {
    use catalerum_core::capability::{Action, Capability, Constraints, Resource};

    let Some(url) = test_db_url() else {
        eprintln!("skipping resume-window test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("resumewin", &format!("resumewin-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // A grant whose window is already in the past, and an automation under it.
    let expired: Constraints = serde_json::from_value(json!({
        "time_window": { "start": "2020-01-01T00:00:00Z", "end": "2020-01-02T00:00:00Z" }
    }))
    .unwrap();
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "stale",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &expired,
        )
        .await
        .expect("grant");
    let mut spec = auto(
        "two-step",
        vec![json!({ "kind": "summarize" }), json!({ "kind": "notify" })],
    );
    spec.grant_id = Some(grant.id);
    let automation = store.automations().create(ws.id, &spec).await.unwrap();

    // Crash sim: open the run under the grant, finish step 0 (start_run doesn't gate
    // on the window — only the executor does, which is the point).
    let runs = store.automation_runs();
    let job_id = uuid::Uuid::new_v4();
    let run = runs
        .start_run(
            ws.id,
            automation.id,
            automation.grant_id,
            None,
            Some(job_id),
        )
        .await
        .unwrap();
    let step0 = runs
        .add_step(ws.id, run.id, 0, json!({ "kind": "summarize" }))
        .await
        .unwrap();
    runs.finish_step(ws.id, step0.id, StepStatus::Succeeded, None, None)
        .await
        .unwrap();

    // Re-drive the same job → resume → window re-checked against now (still expired)
    // → fail closed; the remaining action never runs.
    let runner = ScriptedRunner::new(vec![]);
    let finished = execute_for_job(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        None,
        Some(job_id),
    )
    .await
    .unwrap();
    assert_eq!(finished.id, run.id, "resumed the same run");
    assert_eq!(
        finished.status,
        RunStatus::Failed,
        "a resumed run whose window has closed fails closed"
    );
    assert_eq!(
        runner.calls(),
        0,
        "no remaining action runs under an expired window"
    );
}

// ---------------------------------------------------------------------------
// Node-graph (DAG) executor tests (SOUL §11, Phase A).
// ---------------------------------------------------------------------------

/// An [`ActionRunner`] for graph tests: it **echoes the input context it was
/// handed back as its output** (so a downstream node's recorded input can be
/// inspected to prove data passed along the edge) and records, per node it ran, the
/// `inputs` map it saw — keyed by the action's `node` param (encoded by the engine).
#[derive(Default)]
struct EchoRunner {
    /// node-id -> the `inputs` object the engine built for that node.
    seen_inputs: Mutex<std::collections::HashMap<String, Value>>,
}

#[async_trait::async_trait]
impl ActionRunner for EchoRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        // The engine passes the merged `{ trigger, inputs }` context as `trigger`.
        let ctx = trigger.cloned().unwrap_or(Value::Null);
        // Each test action carries a `mark` param naming the node it belongs to (the
        // engine forwards the node's `Action`, params and all, to the runner).
        let mark = action.params.get("mark").cloned().unwrap_or(Value::Null);
        if let Some(node) = mark.as_str() {
            let inputs = ctx.get("inputs").cloned().unwrap_or(Value::Null);
            self.seen_inputs
                .lock()
                .unwrap()
                .insert(node.to_string(), inputs);
        }
        // Echo a small marker plus the context the engine built, so a downstream
        // node sees *this* node's output under its id in `inputs`.
        ActionOutcome::succeeded(Some(json!({ "from": mark, "ctx": ctx })))
    }
}

/// A fake [`CodeRunner`] that returns a fixed JSON value for every code/condition
/// node (used to drive a condition's true/false branch deterministically).
struct FakeCode(Value);

#[async_trait::async_trait]
impl CodeRunner for FakeCode {
    async fn run_code(
        &self,
        _runtime: &str,
        _source: &str,
        _input: &Value,
        _workspace_id: WorkspaceId,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> Result<Value, String> {
        Ok(self.0.clone())
    }
}

/// A graph automation whose `spec` carries the given `graph` object. The legacy
/// `actions` column is non-empty (a compiled shadow) but is NOT what runs.
fn graph_auto(name: &str, graph: Value) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "webhook", "path": "/g" })],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: Some(json!({ "graph": graph })),
        grant_id: None,
    }
}

/// A linear trigger→action→action graph passes each upstream node's output into the
/// downstream node's `inputs` map: the second action sees the first action's output
/// keyed by the first node's id. Proves DAG data-flow end-to-end.
#[tokio::test]
async fn graph_runs_trigger_action_action_and_passes_data_downstream() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping graph data-flow test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("graph", &format!("graph-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            { "id": "a1", "kind": "action", "action": { "kind": "summarize", "mark": "a1" } },
            { "id": "a2", "kind": "action", "action": { "kind": "notify", "mark": "a2" } }
        ],
        "edges": [ { "from": "t", "to": "a1" }, { "from": "a1", "to": "a2" } ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("flow", graph))
        .await
        .unwrap();

    let runner = EchoRunner::default();
    let event = json!({ "kind": "webhook", "path": "/g" });
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        Some(event.clone()),
    )
    .await
    .expect("execute graph");
    assert_eq!(run.status, RunStatus::Succeeded);

    // One step per node, ordered by topo index (t, a1, a2).
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 3, "one step per node");
    assert_eq!(steps[0].action["node"], json!("t"));
    assert_eq!(steps[0].action["kind"], json!("trigger"));
    assert_eq!(
        steps[0].output.as_ref().unwrap(),
        &event,
        "the trigger node's output is the firing event"
    );
    assert_eq!(steps[1].action["node"], json!("a1"));
    assert_eq!(steps[2].action["node"], json!("a2"));
    assert!(steps.iter().all(|s| s.status == StepStatus::Succeeded));

    // a1 saw the trigger node's output under "t"; a2 saw a1's output under "a1".
    let seen = runner.seen_inputs.lock().unwrap();
    let a1_inputs = &seen["a1"];
    assert_eq!(
        a1_inputs["t"], event,
        "a1's inputs carry the upstream trigger node's output"
    );
    let a2_inputs = &seen["a2"];
    // a1's echoed output is an object with `from: "a1"`; a2 must see it under "a1".
    assert_eq!(
        a2_inputs["a1"]["from"],
        json!("a1"),
        "a2's inputs carry a1's output keyed by a1's id (DAG data-flow)"
    );
}

/// A condition node routes execution down its `"true"`/`"false"` out-edge: with a
/// fake CodeRunner returning a truthy value, the `"true"` branch action runs and the
/// `"false"` branch action is Skipped (and vice-versa).
#[tokio::test]
async fn graph_condition_routes_the_truthy_branch_and_skips_the_other() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping graph condition test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("graphcond", &format!("graphcond-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            { "id": "c", "kind": "condition", "runtime": "js", "source": "input" },
            { "id": "yes", "kind": "action", "action": { "kind": "summarize", "mark": "yes" } },
            { "id": "no", "kind": "action", "action": { "kind": "notify", "mark": "no" } }
        ],
        "edges": [
            { "from": "t", "to": "c" },
            { "from": "c", "to": "yes", "from_port": "true" },
            { "from": "c", "to": "no", "from_port": "false" }
        ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("cond", graph))
        .await
        .unwrap();

    let status_of = |steps: &[catalerum_core::model::AutomationStep], node: &str| -> StepStatus {
        steps
            .iter()
            .find(|s| s.action["node"] == json!(node))
            .unwrap_or_else(|| panic!("step for node {node}"))
            .status
    };

    // Truthy condition → "yes" runs, "no" is Skipped.
    let runner = EchoRunner::default();
    let run = execute(
        &store,
        &runner,
        &FakeCode(json!(true)),
        ws.id,
        &automation,
        Some(json!({ "kind": "webhook", "path": "/g" })),
    )
    .await
    .expect("execute true branch");
    assert_eq!(run.status, RunStatus::Succeeded);
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(status_of(&steps, "c"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "yes"), StepStatus::Succeeded);
    assert_eq!(
        status_of(&steps, "no"),
        StepStatus::Skipped,
        "the not-taken branch is Skipped"
    );
    assert!(
        runner.seen_inputs.lock().unwrap().contains_key("yes"),
        "the true-branch action ran"
    );
    assert!(
        !runner.seen_inputs.lock().unwrap().contains_key("no"),
        "the false-branch action did not run"
    );

    // Falsy condition → "no" runs, "yes" is Skipped.
    let runner = EchoRunner::default();
    let run = execute(
        &store,
        &runner,
        &FakeCode(json!(false)),
        ws.id,
        &automation,
        Some(json!({ "kind": "webhook", "path": "/g" })),
    )
    .await
    .expect("execute false branch");
    assert_eq!(run.status, RunStatus::Succeeded);
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(status_of(&steps, "no"), StepStatus::Succeeded);
    assert_eq!(status_of(&steps, "yes"), StepStatus::Skipped);
}

/// A graph with a Code node fails under the Phase-A default [`FailCodeRunner`] (no
/// runtime configured), and the run is recorded `Failed` with that message — Boa
/// lands in Phase B.
#[tokio::test]
async fn graph_code_node_fails_under_the_phase_a_default_runner() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping graph code-node test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("graphcode", &format!("graphcode-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            { "id": "code", "kind": "code", "runtime": "js", "source": "input + 1" }
        ],
        "edges": [ { "from": "t", "to": "code" } ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("code", graph))
        .await
        .unwrap();

    let runner = EchoRunner::default();
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        Some(json!({ "kind": "webhook", "path": "/g" })),
    )
    .await
    .expect("execute graph with code node");
    assert_eq!(run.status, RunStatus::Failed);
    assert!(
        run.error
            .as_deref()
            .unwrap()
            .contains("no code runtime configured"),
        "the Phase-A default fails a code node: {:?}",
        run.error
    );
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    // The trigger node succeeded; the code node failed.
    let code_step = steps
        .iter()
        .find(|s| s.action["node"] == json!("code"))
        .unwrap();
    assert_eq!(code_step.status, StepStatus::Failed);
}

/// Crash-resume on the graph path (SOUL §5/§11): a node already `Succeeded` on a
/// prior attempt is **not re-run**, its recorded output still feeds downstream, and
/// only the unfinished node executes — with no duplicate run. Models the crash by
/// hand-recording the trigger node (ord 0) and `a1` (ord 1, with an output) as
/// Succeeded, then re-driving the same `job_id`; only `a2` (ord 2) runs.
#[tokio::test]
async fn graph_resume_does_not_re_run_done_nodes_and_reuses_their_output() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping graph resume test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("graphres", &format!("graphres-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            { "id": "a1", "kind": "action", "action": { "kind": "summarize", "mark": "a1" } },
            { "id": "a2", "kind": "action", "action": { "kind": "notify", "mark": "a2" } }
        ],
        "edges": [ { "from": "t", "to": "a1" }, { "from": "a1", "to": "a2" } ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("flow", graph))
        .await
        .unwrap();

    // Crash sim: open the run for `job_id`, finish ord 0 (trigger) and ord 1 (a1 with
    // a recorded output) Succeeded; never reach ord 2 (a2).
    let runs = store.automation_runs();
    let job_id = uuid::Uuid::new_v4();
    let event = json!({ "kind": "webhook", "path": "/g" });
    let run = runs
        .start_run(
            ws.id,
            automation.id,
            None,
            Some(event.clone()),
            Some(job_id),
        )
        .await
        .unwrap();
    let s0 = runs
        .add_step(ws.id, run.id, 0, json!({ "node": "t", "kind": "trigger" }))
        .await
        .unwrap();
    runs.finish_step(
        ws.id,
        s0.id,
        StepStatus::Succeeded,
        Some(event.clone()),
        None,
    )
    .await
    .unwrap();
    let a1_out = json!({ "from": "a1", "ctx": "recovered" });
    let s1 = runs
        .add_step(ws.id, run.id, 1, json!({ "node": "a1", "kind": "action" }))
        .await
        .unwrap();
    runs.finish_step(
        ws.id,
        s1.id,
        StepStatus::Succeeded,
        Some(a1_out.clone()),
        None,
    )
    .await
    .unwrap();

    // Re-drive the same job → resume: only a2 runs, and it sees a1's *recorded* output.
    let runner = EchoRunner::default();
    let finished = execute_for_job(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        Some(event),
        Some(job_id),
    )
    .await
    .expect("resume graph run");

    assert_eq!(finished.id, run.id, "resumed the same run, not a fresh one");
    assert_eq!(finished.status, RunStatus::Succeeded);
    // Only a2 ran (t and a1 were already done). Scope the guard so it isn't held
    // across the awaits below.
    {
        let seen = runner.seen_inputs.lock().unwrap();
        assert_eq!(seen.len(), 1, "only the unfinished node a2 ran");
        assert!(seen.contains_key("a2"));
        assert_eq!(
            seen["a2"]["a1"], a1_out,
            "a2 saw a1's RECORDED output (not a re-run), proving resume reuses prior output"
        );
    }
    // Exactly one run, all three steps Succeeded.
    assert_eq!(
        runs.list_runs(ws.id, automation.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let steps = runs.list_steps(ws.id, run.id).await.unwrap();
    assert_eq!(steps.len(), 3);
    assert!(steps.iter().all(|s| s.status == StepStatus::Succeeded));
}

/// A linear automation's top-level `condition` (SOUL §11) gates its actions: a
/// falsy condition records every action `Skipped` (run still Succeeds, no side
/// effect), a truthy one runs them, and a code predicate that can't be evaluated
/// fails the run **closed** (no action runs on an unchecked predicate).
#[tokio::test]
async fn linear_condition_gates_actions() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping condition test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("cond", &format!("cond-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let runs = store.automation_runs();
    let actions = vec![
        json!({ "kind": "summarize" }),
        json!({ "kind": "create_note" }),
    ];

    // (1) A falsy literal condition skips every action; the run still Succeeds and
    //     no action side effect runs. `FailCodeRunner` proves a literal never calls
    //     the code runner.
    let off = store
        .automations()
        .create(ws.id, &auto_cond("cond-off", json!(false), actions.clone()))
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![]);
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &off,
        Some(json!({ "kind": "schedule" })),
    )
    .await
    .expect("execute cond-off");
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "a falsy condition gated cleanly — the run Succeeds with no work"
    );
    let steps = runs.list_steps(ws.id, run.id).await.unwrap();
    assert_eq!(steps.len(), 2);
    assert!(
        steps.iter().all(|s| s.status == StepStatus::Skipped),
        "every action is recorded Skipped by the condition"
    );
    assert_eq!(runner.calls(), 0, "no action side effect ran");

    // (2) A truthy literal condition runs the actions normally.
    let on = store
        .automations()
        .create(ws.id, &auto_cond("cond-on", json!(true), actions.clone()))
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &on, None)
        .await
        .expect("execute cond-on");
    assert_eq!(run.status, RunStatus::Succeeded);
    let steps = runs.list_steps(ws.id, run.id).await.unwrap();
    assert!(
        steps.iter().all(|s| s.status == StepStatus::Succeeded),
        "a truthy condition runs every action"
    );
    assert_eq!(
        runner.calls(),
        2,
        "both actions ran when the condition passed"
    );

    // (3) A code predicate that can't be evaluated (no runtime under FailCodeRunner)
    //     fails the run closed — no action runs on an unchecked predicate.
    let coded = store
        .automations()
        .create(
            ws.id,
            &auto_cond(
                "cond-code",
                json!({ "runtime": "js", "source": "return true;" }),
                actions.clone(),
            ),
        )
        .await
        .unwrap();
    let runner = ScriptedRunner::new(vec![]);
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &coded, None)
        .await
        .expect("execute cond-code");
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "an uncheckable condition fails the run closed"
    );
    assert!(
        run.error
            .as_deref()
            .unwrap_or_default()
            .contains("no code runtime"),
        "the failure carries the predicate-eval error, got: {:?}",
        run.error
    );
    assert_eq!(
        runs.list_steps(ws.id, run.id).await.unwrap().len(),
        0,
        "no action step was recorded — the gate failed before any action"
    );
    assert_eq!(runner.calls(), 0);
}

// ---------------------------------------------------------------------------
// Idempotent-redelivery gate (SOUL §11/§29).
// ---------------------------------------------------------------------------

/// An [`ActionRunner`] that records every node `mark` it actually ran and echoes a
/// `newly_written` param into its output when present — so a "write" node can
/// simulate the store reporting insert-vs-found-existing, which the engine uses to
/// latch a redelivery. Absent → a plain success.
#[derive(Default)]
struct RecordingRunner {
    ran: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl ActionRunner for RecordingRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        action: &Action,
        _trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        let mark = action
            .params
            .get("mark")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        self.ran.lock().unwrap().push(mark);
        if let Some(nw) = action.params.get("newly_written") {
            return ActionOutcome::succeeded(Some(json!({ "newly_written": nw })));
        }
        ActionOutcome::succeeded(Some(json!({ "ok": true })))
    }
}

/// The canonical collect flow `trigger → WriteEmail → LlmAgent → LabelEmail`: on a
/// **first delivery** every node runs; on an at-least-once **redelivery** (the write
/// reports `newly_written == false`) the idempotent write still runs — advancing the
/// cursor — but the non-idempotent classify + label auto-Skip, so no double-spend
/// (SOUL §29). Uses a webhook trigger to isolate the engine gate from collect wiring.
#[tokio::test]
async fn graph_redelivery_skips_non_idempotent_downstream_of_a_write() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping redelivery test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("redeliv", &format!("redeliv-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let make = |newly_written: bool| {
        json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
                { "id": "w", "kind": "action",
                  "action": { "kind": "write_email", "mark": "w", "newly_written": newly_written } },
                { "id": "c", "kind": "action", "action": { "kind": "llm_agent", "mark": "c" } },
                { "id": "l", "kind": "action", "action": { "kind": "label_email", "mark": "l" } }
            ],
            "edges": [ { "from": "t", "to": "w" }, { "from": "w", "to": "c" }, { "from": "c", "to": "l" } ]
        })
    };
    let event = json!({ "kind": "webhook", "path": "/g" });

    // First delivery: the write is newly-written → classify + label run.
    let fresh = store
        .automations()
        .create(ws.id, &graph_auto("fresh", make(true)))
        .await
        .unwrap();
    let runner = RecordingRunner::default();
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &fresh,
        Some(event.clone()),
    )
    .await
    .expect("fresh run");
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        *runner.ran.lock().unwrap(),
        vec!["w", "c", "l"],
        "a first delivery runs write + classify + label"
    );

    // Redelivery: the write reports the item was already stored → only the idempotent
    // write runs; the non-idempotent classify + label auto-skip.
    let redeliv = store
        .automations()
        .create(ws.id, &graph_auto("redeliv", make(false)))
        .await
        .unwrap();
    let runner = RecordingRunner::default();
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &redeliv,
        Some(event.clone()),
    )
    .await
    .expect("redeliv run");
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "a redelivery run still Succeeds — auto-skips are not failures, so commit_on advances"
    );
    assert_eq!(
        *runner.ran.lock().unwrap(),
        vec!["w"],
        "only the idempotent write runs on a redelivery — no double-spent LlmAgent, no re-label"
    );
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    let status_of = |n: &str| {
        steps
            .iter()
            .find(|s| s.action["node"] == json!(n))
            .unwrap_or_else(|| panic!("no step for node {n}"))
            .status
    };
    assert_eq!(
        status_of("w"),
        StepStatus::Succeeded,
        "the write runs to commit the cursor"
    );
    assert_eq!(
        status_of("c"),
        StepStatus::Skipped,
        "llm_agent auto-skipped on redelivery"
    );
    assert_eq!(
        status_of("l"),
        StepStatus::Skipped,
        "label_email auto-skipped on redelivery"
    );
}

/// The redelivery flag can be **seeded on the firing event** (`trigger.redelivery`),
/// which gates even nodes that sit upstream of any write; and a node opts back in with
/// `"rerun_on_redelivery": true`. Graph: `trigger → LlmAgent` and `trigger → Notify`
/// (both direct children). Seeded redelivery skips the LlmAgent but the opted-in
/// Notify still fires (SOUL §11/§29).
#[tokio::test]
async fn graph_redelivery_seed_and_rerun_override() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping redelivery-seed test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("redseed", &format!("redseed-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            { "id": "c", "kind": "action", "action": { "kind": "llm_agent", "mark": "c" } },
            { "id": "n", "kind": "action",
              "action": { "kind": "notify", "mark": "n", "rerun_on_redelivery": true } }
        ],
        "edges": [ { "from": "t", "to": "c" }, { "from": "t", "to": "n" } ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("seed", graph))
        .await
        .unwrap();

    let runner = RecordingRunner::default();
    // Seed the redelivery flag on the event itself.
    let event = json!({ "kind": "webhook", "path": "/g", "redelivery": true });
    let run = execute(
        &store,
        &runner,
        &FailCodeRunner,
        ws.id,
        &automation,
        Some(event),
    )
    .await
    .expect("seeded run");
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        *runner.ran.lock().unwrap(),
        vec!["n"],
        "the trigger-seeded redelivery skips the LlmAgent; the rerun-opted-in Notify still fires"
    );
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    let status_of = |node: &str| {
        steps
            .iter()
            .find(|s| s.action["node"] == json!(node))
            .unwrap()
            .status
    };
    assert_eq!(
        status_of("c"),
        StepStatus::Skipped,
        "llm_agent skipped by the seeded redelivery"
    );
    assert_eq!(
        status_of("n"),
        StepStatus::Succeeded,
        "notify opted back in with rerun_on_redelivery"
    );
}
