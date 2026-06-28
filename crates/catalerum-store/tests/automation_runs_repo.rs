//! Integration test: the `AutomationRunRepo` lifecycle (SOUL §11, §6.1/§18).
//! Open a run, append steps, finalize both; status + JSON round-trip; the
//! `(run_id, ordinal)` unique key; the tenancy guards on `start_run`/`add_step`;
//! and cross-workspace isolation on every read/write path.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{RunStatus, StepStatus};
use catalerum_store::{NewAutomation, Store, StoreError};
use serde_json::json;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn automation(name: &str) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
        condition: None,
        actions: vec![json!({ "kind": "summarize" })],
        spec: None,
        grant_id: None,
    }
}

#[tokio::test]
async fn run_step_lifecycle_and_isolation() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping run_step_lifecycle test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("runs", &format!("runs-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("runs-b", &format!("runs-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let auto = store
        .automations()
        .create(ws.id, &automation("daily-digest"))
        .await
        .expect("automation");
    let other_auto = store
        .automations()
        .create(other.id, &automation("their-automation"))
        .await
        .expect("other automation");

    // Open a run; it is born Running with the trigger payload and no finish stamp.
    let trigger = json!({ "kind": "schedule", "fired_at": "2026-06-15T09:00:00Z" });
    let run = store
        .automation_runs()
        .start_run(ws.id, auto.id, None, Some(trigger.clone()), None)
        .await
        .expect("start_run");
    assert_eq!(run.status, RunStatus::Running);
    assert!(!run.status.is_terminal());
    assert_eq!(run.automation_id, auto.id);
    assert_eq!(run.grant_id, None, "a run with no grant records NULL");
    assert_eq!(
        run.trigger.as_ref().unwrap()["fired_at"],
        json!("2026-06-15T09:00:00Z")
    );
    assert!(run.finished_at.is_none());
    assert_eq!(
        store
            .automation_runs()
            .get_run(ws.id, run.id)
            .await
            .unwrap()
            .id,
        run.id
    );

    // start_run is tenancy-guarded: a run can't reference another workspace's automation.
    assert!(matches!(
        store
            .automation_runs()
            .start_run(ws.id, other_auto.id, None, None, None)
            .await,
        Err(StoreError::NotFound)
    ));

    // Append two steps in order; each born Running.
    let s0 = store
        .automation_runs()
        .add_step(
            ws.id,
            run.id,
            0,
            json!({ "kind": "llm_agent", "skills": ["weekly-review"] }),
        )
        .await
        .expect("step 0");
    assert_eq!(s0.status, StepStatus::Running);
    assert_eq!(s0.ordinal, 0);
    assert_eq!(s0.action["kind"], json!("llm_agent"));
    let s1 = store
        .automation_runs()
        .add_step(ws.id, run.id, 1, json!({ "kind": "summarize" }))
        .await
        .expect("step 1");

    // Duplicate ordinal in a run → conflict; another workspace's run_id → NotFound.
    assert!(matches!(
        store
            .automation_runs()
            .add_step(ws.id, run.id, 0, json!({ "kind": "notify" }))
            .await,
        Err(StoreError::Conflict(_))
    ));
    assert!(matches!(
        store
            .automation_runs()
            .add_step(other.id, run.id, 2, json!({ "kind": "notify" }))
            .await,
        Err(StoreError::NotFound)
    ));

    // Finish step 0 succeeded with output; step 1 failed with an error.
    let s0_done = store
        .automation_runs()
        .finish_step(
            ws.id,
            s0.id,
            StepStatus::Succeeded,
            Some(json!({ "note_id": "abc" })),
            None,
        )
        .await
        .expect("finish step 0");
    assert_eq!(s0_done.status, StepStatus::Succeeded);
    assert!(s0_done.status.is_terminal());
    assert_eq!(s0_done.output.as_ref().unwrap()["note_id"], json!("abc"));
    assert!(s0_done.finished_at.is_some());
    let s1_done = store
        .automation_runs()
        .finish_step(
            ws.id,
            s1.id,
            StepStatus::Failed,
            None,
            Some("model timed out"),
        )
        .await
        .expect("finish step 1");
    assert_eq!(s1_done.status, StepStatus::Failed);
    assert_eq!(s1_done.error.as_deref(), Some("model timed out"));

    // Steps list in execution order.
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].ordinal, 0);
    assert_eq!(steps[1].ordinal, 1);

    // Finalize the run as Failed (a step failed); finished_at stamped.
    let run_done = store
        .automation_runs()
        .finish_run(ws.id, run.id, RunStatus::Failed, Some("step 1 failed"))
        .await
        .expect("finish_run");
    assert_eq!(run_done.status, RunStatus::Failed);
    assert!(run_done.status.is_terminal());
    assert!(run_done.finished_at.is_some());
    assert_eq!(run_done.error.as_deref(), Some("step 1 failed"));

    // list_runs returns the run, most-recent first.
    let runs = store
        .automation_runs()
        .list_runs(ws.id, auto.id, 10)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run.id);

    // Cross-workspace isolation on every path: another workspace sees / touches nothing.
    assert!(matches!(
        store.automation_runs().get_run(other.id, run.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store
            .automation_runs()
            .finish_run(other.id, run.id, RunStatus::Cancelled, None)
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store
            .automation_runs()
            .finish_step(other.id, s0.id, StepStatus::Skipped, None, None)
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(store
        .automation_runs()
        .list_steps(other.id, run.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .automation_runs()
        .list_runs(other.id, auto.id, 10)
        .await
        .unwrap()
        .is_empty());
    // The wrong-workspace mutations left the real rows intact.
    assert_eq!(
        store
            .automation_runs()
            .get_run(ws.id, run.id)
            .await
            .unwrap()
            .status,
        RunStatus::Failed
    );

    // Deleting the automation cascades its runs + steps away (FK ON DELETE CASCADE).
    store.automations().delete(ws.id, auto.id).await.unwrap();
    assert!(matches!(
        store.automation_runs().get_run(ws.id, run.id).await,
        Err(StoreError::NotFound)
    ));
}

/// §19 audit: a run **snapshots** the grant it executed under, and that snapshot is
/// an immutable historical fact — deleting the grant detaches the automation's live
/// link but must NOT rewrite the run's record (the column has no FK by design).
#[tokio::test]
async fn run_snapshots_its_grant_for_audit_and_survives_grant_deletion() {
    use catalerum_core::capability::{Action, Capability, Constraints, Resource};

    let Some(url) = test_db_url() else {
        eprintln!("skipping grant-audit test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("runaudit", &format!("runaudit-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    // A grant, and an automation that runs under it.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "audited",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &Constraints::default(),
        )
        .await
        .expect("grant");
    let mut spec = automation("audited-bot");
    spec.grant_id = Some(grant.id);
    let auto = store
        .automations()
        .create(ws.id, &spec)
        .await
        .expect("auto");
    assert_eq!(auto.grant_id, Some(grant.id));

    // The run snapshots the grant id it executed under (recorded + DB round-trip).
    let run = store
        .automation_runs()
        .start_run(ws.id, auto.id, auto.grant_id, None, None)
        .await
        .expect("start_run");
    assert_eq!(
        run.grant_id,
        Some(grant.id),
        "the run records its authorizing grant"
    );
    assert_eq!(
        store
            .automation_runs()
            .get_run(ws.id, run.id)
            .await
            .unwrap()
            .grant_id,
        Some(grant.id),
        "the audit fact round-trips through the DB"
    );

    // Delete the grant: the automation's LIVE link nulls (composite FK SET NULL),
    // but the run's audit grant_id SURVIVES — history is immutable.
    assert!(store
        .grants()
        .delete(ws.id, grant.id)
        .await
        .expect("delete grant"));
    assert_eq!(
        store
            .automations()
            .get(ws.id, auto.id)
            .await
            .unwrap()
            .grant_id,
        None,
        "the automation's live grant link is nulled on grant deletion"
    );
    assert_eq!(
        store
            .automation_runs()
            .get_run(ws.id, run.id)
            .await
            .unwrap()
            .grant_id,
        Some(grant.id),
        "the run's audit grant_id survives grant deletion (no FK) — history is immutable"
    );
}
