//! Integration test (SOUL §11 Phase B): the `SyncWorker` runs a node-graph
//! automation whose inline **Code / Condition** nodes execute on the real
//! [`ScriptCodeRunner`](catalerum_script::ScriptCodeRunner) (Boa-sandboxed JS),
//! installed on the worker's [`AutomationContext`] via `with_code_runner`.
//!
//! Two end-to-end paths, each dispatched as a durable `run_automation` job and
//! drained through the real worker:
//! - **Code node** — a JS transform reads the trigger event and produces a value;
//!   the run Succeeds and the Code node's recorded step output **is the JS result**
//!   (and a downstream Action node sees it in its `inputs`).
//! - **Condition node** — a JS condition routes to the true branch; the true-branch
//!   action runs (Succeeded) while the false-branch action is **Skipped**.
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use catalerum_automation::{Action, ActionOutcome, ActionRunner};
use catalerum_core::model::{RunStatus, StepStatus};
use catalerum_core::WorkspaceId;
use catalerum_ingest::{enqueue_run_automation, AutomationContext, SyncWorker};
use catalerum_script::ScriptCodeRunner;
use catalerum_store::{JobRow, JobStatus, NewAutomation, Store};
use serde_json::{json, Value};

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// An [`ActionRunner`] that records, per node it ran, the `inputs` map the engine
/// built for it (keyed by the action's `mark` param). Always succeeds — enough to
/// prove the action ran *and* what upstream data it saw (so a Code node's output
/// feeding downstream can be asserted).
#[derive(Default)]
struct RecordingRunner {
    /// node mark -> the `inputs` object the engine handed that node.
    seen_inputs: Mutex<std::collections::HashMap<String, Value>>,
}

#[async_trait::async_trait]
impl ActionRunner for RecordingRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        // The engine passes the merged `{ trigger, inputs }` context as `trigger`.
        let ctx = trigger.cloned().unwrap_or(Value::Null);
        let mark = action.params.get("mark").cloned().unwrap_or(Value::Null);
        if let Some(node) = mark.as_str() {
            let inputs = ctx.get("inputs").cloned().unwrap_or(Value::Null);
            self.seen_inputs
                .lock()
                .unwrap()
                .insert(node.to_string(), inputs);
        }
        ActionOutcome::succeeded(Some(json!({ "ran": mark })))
    }
}

/// A graph automation whose `spec` carries `graph` (under the `"graph"` key the
/// executor reads). The webhook trigger matches the dispatched event.
fn graph_auto(name: &str, graph: Value) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "webhook", "path": "/g" })],
        condition: None,
        // A non-empty legacy `actions` column (a compiled shadow); the graph is
        // what actually runs.
        actions: vec![json!({ "kind": "summarize" })],
        spec: Some(json!({ "graph": graph })),
        grant_id: None,
    }
}

/// Build a `SyncWorker` with a recording action runner + the real Boa
/// `ScriptCodeRunner` installed, returning the runner handle so a test can inspect
/// the inputs each action saw.
fn worker_with_script(store: &Store) -> (SyncWorker, Arc<RecordingRunner>) {
    let recorder = Arc::new(RecordingRunner::default());
    let runner: Arc<dyn ActionRunner> = recorder.clone();
    let code: Arc<dyn catalerum_automation::CodeRunner> = Arc::new(ScriptCodeRunner::new());
    let worker = SyncWorker::new(store.clone())
        .with_automation_context(AutomationContext::new(runner).with_code_runner(code));
    (worker, recorder)
}

/// Drive `worker.poll_once()` until `job_id` reaches a terminal state.
async fn drain_to_terminal(store: &Store, worker: &SyncWorker, job_id: uuid::Uuid) -> JobRow {
    for _ in 0..100 {
        let row = store.job_queue().get(job_id).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            return row;
        }
        if !worker.poll_once().await.expect("poll_once") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    store.job_queue().get(job_id).await.expect("get job")
}

/// A webhook Trigger → a **Code** node (JS transform) → an Action node, dispatched +
/// drained through the worker with a real `ScriptCodeRunner`. The run Succeeds, the
/// Code node's step output **is the JS result**, and the downstream Action sees that
/// result in its `inputs`.
#[tokio::test]
async fn worker_runs_js_code_node_and_records_its_output() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping js code-node worker test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("jscode", &format!("jscode-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // The JS reads the trigger event and the (empty) upstream inputs and builds an
    // object — proving the bound `input` carries the merged `{ trigger, inputs }`.
    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            {
                "id": "code", "kind": "code", "runtime": "js",
                "source": "return { doubled: input.trigger.n * 2, path: input.trigger.path };"
            },
            { "id": "a", "kind": "action", "action": { "kind": "summarize", "mark": "a" } }
        ],
        "edges": [ { "from": "t", "to": "code" }, { "from": "code", "to": "a" } ]
    });
    let automation = store
        .automations()
        .create(ws.id, &graph_auto("codeflow", graph))
        .await
        .unwrap();

    let (worker, recorder) = worker_with_script(&store);
    let event = json!({ "kind": "webhook", "path": "/g", "n": 21 });
    let job = enqueue_run_automation(&store, ws.id, automation.id, Some(event.clone()))
        .await
        .unwrap();

    let row = drain_to_terminal(&store, &worker, job).await;
    assert_eq!(
        row.status().unwrap(),
        JobStatus::Done,
        "the run_automation job completed"
    );

    let runs = store
        .automation_runs()
        .list_runs(ws.id, automation.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "one run recorded");
    assert_eq!(
        runs[0].status,
        RunStatus::Succeeded,
        "the run with a JS code node Succeeds: {:?}",
        runs[0].error
    );

    let steps = store
        .automation_runs()
        .list_steps(ws.id, runs[0].id)
        .await
        .unwrap();
    let code_step = steps
        .iter()
        .find(|s| s.action["node"] == json!("code"))
        .expect("a step for the code node");
    assert_eq!(code_step.status, StepStatus::Succeeded);
    // The Code node's step output is exactly the JS function's return value.
    assert_eq!(
        code_step.output.as_ref().unwrap(),
        &json!({ "doubled": 42, "path": "/g" }),
        "the code node's step output is the JS result"
    );

    // The downstream action saw the code node's output under the code node's id.
    let seen = recorder.seen_inputs.lock().unwrap();
    let a_inputs = seen.get("a").expect("the downstream action ran");
    assert_eq!(
        a_inputs["code"],
        json!({ "doubled": 42, "path": "/g" }),
        "the action's inputs carry the code node's JS output (DAG data-flow)"
    );
}

/// A webhook Trigger → a **Condition** node (JS `return input.trigger.n > 5`) →
/// true/false branch actions, run through the worker with a real `ScriptCodeRunner`.
/// The JS condition is truthy, so the true-branch action runs (Succeeded) and the
/// false-branch action is **Skipped**.
#[tokio::test]
async fn worker_runs_js_condition_node_and_routes_the_true_branch() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping js condition-node worker test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("jscond", &format!("jscond-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let graph = json!({
        "nodes": [
            { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/g" } },
            {
                "id": "c", "kind": "condition", "runtime": "js",
                "source": "return input.trigger.n > 5;"
            },
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
        .create(ws.id, &graph_auto("condflow", graph))
        .await
        .unwrap();

    let (worker, recorder) = worker_with_script(&store);
    // n = 10 > 5 → the JS condition is truthy → the "true" branch is taken.
    let event = json!({ "kind": "webhook", "path": "/g", "n": 10 });
    let job = enqueue_run_automation(&store, ws.id, automation.id, Some(event))
        .await
        .unwrap();

    let row = drain_to_terminal(&store, &worker, job).await;
    assert_eq!(row.status().unwrap(), JobStatus::Done);

    let runs = store
        .automation_runs()
        .list_runs(ws.id, automation.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].status,
        RunStatus::Succeeded,
        "the run with a JS condition Succeeds: {:?}",
        runs[0].error
    );

    let steps = store
        .automation_runs()
        .list_steps(ws.id, runs[0].id)
        .await
        .unwrap();
    let status_of = |node: &str| -> StepStatus {
        steps
            .iter()
            .find(|s| s.action["node"] == json!(node))
            .unwrap_or_else(|| panic!("a step for node {node}"))
            .status
    };
    // The condition node ran and its step output is the JS boolean.
    let cond_step = steps
        .iter()
        .find(|s| s.action["node"] == json!("c"))
        .expect("a step for the condition node");
    assert_eq!(cond_step.status, StepStatus::Succeeded);
    assert_eq!(
        cond_step.output.as_ref().unwrap(),
        &json!(true),
        "the condition node's step output is the JS boolean result"
    );
    // The truthy condition routes to the "true" branch; the "false" branch is Skipped.
    assert_eq!(status_of("yes"), StepStatus::Succeeded, "true branch ran");
    assert_eq!(
        status_of("no"),
        StepStatus::Skipped,
        "the not-taken false branch is Skipped"
    );

    let seen = recorder.seen_inputs.lock().unwrap();
    assert!(seen.contains_key("yes"), "the true-branch action ran");
    assert!(
        !seen.contains_key("no"),
        "the false-branch action did not run"
    );
}
