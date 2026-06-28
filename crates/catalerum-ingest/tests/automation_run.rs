//! Integration test: the `SyncWorker` dispatches `run_automation` jobs (SOUL
//! §11/§6.2). It claims the job, loads the automation, and runs it via an injected
//! [`ActionRunner`] — recording durable run/step state — and skips a disabled
//! automation.
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

mod common;

use std::time::Duration;

use catalerum_automation::{Action, ActionOutcome, ActionRunner, TriggerEvent};
use catalerum_core::model::RunStatus;
use catalerum_core::WorkspaceId;
use catalerum_ingest::{
    dispatch_trigger_event, enqueue_run_automation, AutomationContext, RunAutomationPayload,
    SyncWorker,
};
use catalerum_store::{JobRow, JobStatus, NewAutomation, Store};
use serde_json::{json, Value};

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// A runner that succeeds every action — enough to prove the worker drives the
/// run; the real tool-backed runner is tested in `catalerum-api`.
struct SuccessRunner;

#[async_trait::async_trait]
impl ActionRunner for SuccessRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        _action: &Action,
        _trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        ActionOutcome::succeeded(None)
    }
}

fn automation(name: &str, enabled: bool, actions: Vec<Value>) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled,
        triggers: vec![json!({ "kind": "webhook", "path": "/x" })],
        condition: None,
        actions,
        spec: None,
        grant_id: None,
    }
}

/// Drive `worker.poll_once()` until `job_id` reaches a terminal (`done`/`failed`)
/// state — scoped to our own job id, so it is robust to a shared queue holding
/// other tests' leftover jobs.
async fn drain_to_terminal(store: &Store, worker: &SyncWorker, job_id: uuid::Uuid) -> JobRow {
    for _ in 0..50 {
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

#[tokio::test]
async fn worker_runs_enqueued_automation_and_skips_disabled() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping run_automation worker test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("autowork", &format!("autowork-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let runner: std::sync::Arc<dyn ActionRunner> = std::sync::Arc::new(SuccessRunner);
    let worker =
        SyncWorker::new(store.clone()).with_automation_context(AutomationContext::new(runner));

    // 1. An enabled automation with two actions → the worker runs it end-to-end.
    let nightly = store
        .automations()
        .create(
            ws.id,
            &automation(
                "nightly",
                true,
                vec![
                    json!({ "kind": "summarize" }),
                    json!({ "kind": "notify", "channel": "ops" }),
                ],
            ),
        )
        .await
        .unwrap();
    let job = enqueue_run_automation(
        &store,
        ws.id,
        nightly.id,
        Some(json!({ "kind": "webhook", "path": "/x" })),
    )
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
        .list_runs(ws.id, nightly.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1, "a run was recorded");
    assert_eq!(runs[0].status, RunStatus::Succeeded);
    assert_eq!(
        runs[0].trigger.as_ref().unwrap()["path"],
        json!("/x"),
        "the trigger payload is recorded"
    );
    assert_eq!(
        store
            .automation_runs()
            .list_steps(ws.id, runs[0].id)
            .await
            .unwrap()
            .len(),
        2,
        "both actions ran as steps"
    );

    // 2. A disabled automation is skipped: the job completes, no run is recorded.
    let paused = store
        .automations()
        .create(
            ws.id,
            &automation("paused", false, vec![json!({ "kind": "summarize" })]),
        )
        .await
        .unwrap();
    let job = enqueue_run_automation(&store, ws.id, paused.id, None)
        .await
        .unwrap();
    let row = drain_to_terminal(&store, &worker, job).await;
    assert_eq!(
        row.status().unwrap(),
        JobStatus::Done,
        "a skipped (disabled) automation still completes its job"
    );
    assert!(
        store
            .automation_runs()
            .list_runs(ws.id, paused.id, 10)
            .await
            .unwrap()
            .is_empty(),
        "a disabled automation records no run"
    );
}

fn task_moved(board: &str, to_column: &str) -> Value {
    json!({ "kind": "task_moved", "board": board, "to_column": to_column })
}

/// Fail-closed dispatch (SOUL §18): a fire at an **archived** workspace matches
/// nothing — the shared `dispatch_trigger_event` bridge enqueues no jobs even
/// though an enabled, matching automation exists. This is the chokepoint every
/// by-id trigger surface (authed fire, public token fire, webhooks, Kanban,
/// storage, channels) funnels through, so proving it here proves all of them.
#[tokio::test]
async fn dispatch_trigger_event_at_archived_workspace_matches_nothing() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping archived-dispatch test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("arch", &format!("arch-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // An enabled automation that matches the event we'll fire.
    let mut a = automation("hit", true, vec![json!({ "kind": "summarize" })]);
    a.triggers = vec![task_moved("sprint", "done")];
    store.automations().create(ws.id, &a).await.unwrap();

    let event = TriggerEvent::TaskMoved {
        board: "sprint".into(),
        to_column: "done".into(),
    };
    // While live, the match fires (baseline).
    let live = dispatch_trigger_event(&store, ws.id, &event).await.unwrap();
    assert_eq!(live.len(), 1, "the enabled automation matches while live");

    // Archive the workspace, then re-fire: the bridge fails closed — no jobs.
    store.workspaces().archive(ws.id).await.expect("archive");
    let archived = dispatch_trigger_event(&store, ws.id, &event).await.unwrap();
    assert!(
        archived.is_empty(),
        "a fire at an archived workspace matches nothing"
    );
}

/// Fail-closed at claim time (SOUL §18): a `run_automation` job already sitting in
/// the queue when the workspace is archived is **skipped** — the worker settles it
/// (`Done`, no run recorded) rather than executing an automation in an archived
/// workspace. Guards the dispatch → durable-job → worker window that archiving
/// (a bare `archived_at` stamp) does not drain.
#[tokio::test]
async fn queued_run_at_archived_workspace_is_skipped() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping archived-run-claim test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("archrun", &format!("archrun-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let runner: std::sync::Arc<dyn ActionRunner> = std::sync::Arc::new(SuccessRunner);
    let worker =
        SyncWorker::new(store.clone()).with_automation_context(AutomationContext::new(runner));

    let auto = store
        .automations()
        .create(
            ws.id,
            &automation("queued", true, vec![json!({ "kind": "summarize" })]),
        )
        .await
        .unwrap();
    // Enqueue the run, THEN archive — so the job is pending when the flag lands.
    let job = enqueue_run_automation(&store, ws.id, auto.id, None)
        .await
        .unwrap();
    store.workspaces().archive(ws.id).await.expect("archive");

    let row = drain_to_terminal(&store, &worker, job).await;
    assert_eq!(
        row.status().unwrap(),
        JobStatus::Done,
        "a skipped (archived-workspace) run still completes its job"
    );
    assert!(
        store
            .automation_runs()
            .list_runs(ws.id, auto.id, 10)
            .await
            .unwrap()
            .is_empty(),
        "no run is recorded for a job claimed after the workspace was archived"
    );
}

#[tokio::test]
async fn dispatch_trigger_event_enqueues_only_matching_enabled_automations() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping dispatch_trigger_event test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("dispatch", &format!("dispatch-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // Three automations: an enabled match, an enabled non-match, and a disabled
    // match — each triggered on a task moving into a column (the `automation()`
    // helper's webhook trigger replaced with a `task_moved` one).
    let mk = |name: &str, enabled: bool, to_column: &str| {
        let mut a = automation(name, enabled, vec![json!({ "kind": "summarize" })]);
        a.triggers = vec![task_moved("sprint", to_column)];
        a
    };
    // Two enabled matches (fan-out), one enabled non-match, one disabled match.
    let hit = store
        .automations()
        .create(ws.id, &mk("hit", true, "done"))
        .await
        .unwrap();
    let hit2 = store
        .automations()
        .create(ws.id, &mk("hit2", true, "done"))
        .await
        .unwrap();
    store
        .automations()
        .create(ws.id, &mk("wrong", true, "doing"))
        .await
        .unwrap();
    store
        .automations()
        .create(ws.id, &mk("off", false, "done"))
        .await
        .unwrap();

    // An event into "done" enqueues a job for *every* enabled match (fan-out).
    let event = TriggerEvent::TaskMoved {
        board: "sprint".into(),
        to_column: "done".into(),
    };
    let jobs = dispatch_trigger_event(&store, ws.id, &event).await.unwrap();
    assert_eq!(
        jobs.len(),
        2,
        "every enabled, matching automation is enqueued"
    );

    // The two jobs target the two matching automations, each carrying the event.
    let mut targets = Vec::new();
    for job in &jobs {
        let row = store.job_queue().get(*job).await.unwrap();
        let payload: RunAutomationPayload = serde_json::from_value(row.payload().clone()).unwrap();
        assert_eq!(
            payload.trigger.as_ref().unwrap()["to_column"],
            json!("done")
        );
        targets.push(payload.automation_id);
    }
    assert!(
        targets.contains(&hit.id) && targets.contains(&hit2.id),
        "both matches enqueued, distinct"
    );

    // An event nothing matches enqueues nothing.
    let nomatch = TriggerEvent::TaskMoved {
        board: "sprint".into(),
        to_column: "blocked".into(),
    };
    assert!(dispatch_trigger_event(&store, ws.id, &nomatch)
        .await
        .unwrap()
        .is_empty());
}
