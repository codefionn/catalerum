//! Integration test: the durable queue path — `enqueue_sync` → `poll_once`
//! claims and runs the job → the connection's events land in Postgres, and the
//! job is marked `done` (SOUL §6.2/§10).
//!
//! Same DB gating as `local_sync`: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes so the suite stays
//! green offline.

mod common;

use std::time::Duration;

use catalerum_core::model::ConnectionKind;
use catalerum_store::{DateRange, JobRow, JobStatus, Store};

const ICS: &str = "\
BEGIN:VCALENDAR\r
VERSION:2.0\r
BEGIN:VEVENT\r
UID:only@catalerum\r
DTSTART:20260613T090000Z\r
DTEND:20260613T100000Z\r
SUMMARY:Lone event\r
END:VEVENT\r
END:VCALENDAR\r
";

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// Serializes the tests in this file. They all share **one** `job_queue` table
/// (the ephemeral per-crate Postgres of `just test`), and `dequeue_one` claims
/// the globally-oldest pending job — so two tests running their workers at once
/// steal each other's jobs and observe each other's transient states (a job
/// momentarily `running`, a freshly-reclaimed job re-claimed before its
/// assertion). Holding this lock for each test's duration keeps the worker
/// behaviour deterministic without a per-test database.
fn queue_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Drive `worker.poll_once()` until `job_id` reaches a terminal (`done`/`failed`)
/// state, returning the final row. Waits on the job's **own** state rather than
/// assuming a particular `poll_once` claims it, so it is robust to a shared
/// queue holding other jobs.
async fn drain_to_terminal(
    store: &Store,
    worker: &catalerum_ingest::SyncWorker,
    job_id: uuid::Uuid,
) -> JobRow {
    for _ in 0..50 {
        let row = store.job_queue().get(job_id).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            return row;
        }
        // Nothing claimable right now (our job may be mid-run elsewhere): wait a
        // beat before retrying.
        if !worker.poll_once().await.expect("poll_once") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    store.job_queue().get(job_id).await.expect("get job")
}

#[tokio::test]
async fn worker_claims_and_runs_an_enqueued_sync_job() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping worker_claims_and_runs_an_enqueued_sync_job: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let _guard = queue_lock().lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("cal.ics"), ICS).expect("fixture");

    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create(
            "worker-test",
            &format!("worker-test-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("workspace");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "fixture",
            None,
            Some(serde_json::json!({ "provider": "local", "path": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");

    // Enqueue a durable sync job and drive the worker until *our* job is terminal
    // (scoped to the job id, robust to a shared queue holding other jobs).
    let job_id = catalerum_ingest::enqueue_sync(&store, ws.id, conn.id)
        .await
        .expect("enqueue");

    let worker = catalerum_ingest::SyncWorker::new(store.clone());
    let job = drain_to_terminal(&store, &worker, job_id).await;
    assert_eq!(
        job.status().unwrap(),
        JobStatus::Done,
        "the enqueued sync job runs to completion; last_error = {:?}",
        job.last_error
    );

    // The event is persisted.
    let events = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            DateRange::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .expect("events");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].uid, "only@catalerum");

    // cleanup
    store
        .connections()
        .delete(ws.id, conn.id)
        .await
        .expect("delete conn");
    store.workspaces().delete(ws.id).await.expect("delete ws");
}

/// The API->worker seam, end-to-end at the data layer: enqueue a `sync_calendar`
/// job exactly the way `catalerum-api`'s `POST /connections/:id/sync` route does
/// — the connection config keyed by `dir` (+ a stamped `provider`), and a job
/// payload that may omit `workspace_id` (the worker resolves it from the job
/// row's `workspace_id` column). The real worker must then ingest the `.ics`.
///
/// This is the regression guard for the two contract mismatches adversarial
/// verification found: (1) the worker rejecting a `workspace_id`-less payload,
/// and (2) the provider failing to read the API's `dir`/`base_url` config keys.
#[tokio::test]
async fn worker_runs_job_enqueued_with_api_payload_and_config() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping worker_runs_job_enqueued_with_api_payload_and_config: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let _guard = queue_lock().lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("cal.ics"), ICS).expect("fixture");

    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("api-seam", &format!("api-seam-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");

    // The connection config the API persists for kind = "local": canonical
    // `dir` key (not `path`) plus the stamped `provider` discriminator.
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "fixture",
            None,
            Some(serde_json::json!({ "provider": "local", "dir": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");

    // Enqueue exactly as the API route does: the job's `workspace_id` column
    // carries the scope, and the payload is the minimal `{ connection_id }`
    // shape (no `workspace_id` field) — the previously-failing wire contract.
    let payload = serde_json::json!({ "connection_id": conn.id });
    let job = store
        .job_queue()
        .enqueue(Some(ws.id), "sync_calendar", payload, None)
        .await
        .expect("enqueue");

    // Drain the queue until *this* job reaches a terminal state. Scoping the
    // assertion to our own job id (rather than assuming the next `poll_once`
    // claims it) keeps the test robust if the shared queue holds other jobs.
    let worker = catalerum_ingest::SyncWorker::new(store.clone());
    let mut done = None;
    for _ in 0..16 {
        let row = store.job_queue().get(job.id).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            done = Some(row);
            break;
        }
        // No job left to claim and ours is still not terminal -> nothing more
        // will advance it; stop and let the assertion below report the state.
        if !worker.poll_once().await.expect("poll_once") {
            done = Some(store.job_queue().get(job.id).await.expect("get job"));
            break;
        }
    }
    let done = done.expect("job reached a terminal/observed state");

    // The job ran to completion (no "missing field `workspace_id`") and the
    // event landed via the real worker -> provider -> store path.
    assert_eq!(
        done.status().unwrap(),
        JobStatus::Done,
        "API-shaped job must complete, not fail; last_error = {:?}",
        done.last_error
    );

    let events = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            DateRange::default(),
            catalerum_store::DEFAULT_EVENT_LIMIT,
        )
        .await
        .expect("events");
    assert_eq!(
        events.len(),
        1,
        "the .ics ingested end-to-end via the API seam"
    );
    assert_eq!(events[0].uid, "only@catalerum");

    // cleanup
    store
        .connections()
        .delete(ws.id, conn.id)
        .await
        .expect("delete conn");
    store.workspaces().delete(ws.id).await.expect("delete ws");
}

#[tokio::test]
async fn worker_fails_unknown_kind_job_with_retry() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping worker_fails_unknown_kind_job_with_retry: no test DB");
        return;
    };
    let _guard = queue_lock().lock().await;

    let store = common::isolated_store(&url).await;

    // Enqueue a job of a kind this worker does not handle.
    let job = store
        .job_queue()
        .enqueue(None, "not_a_real_kind", serde_json::json!({}), None)
        .await
        .expect("enqueue");

    // Drive the worker until *our* unknown-kind job has been attempted and
    // re-queued `pending` with its error (scoped to the job id; the queue may
    // hold other tests' leftover jobs even while serialized).
    let worker = catalerum_ingest::SyncWorker::new(store.clone());
    let mut after = None;
    for _ in 0..50 {
        let row = store.job_queue().get(job.id).await.expect("get");
        // First failure (attempts < max) re-queues to `pending` with the error.
        if row.attempts >= 1
            && matches!(
                row.status().unwrap(),
                JobStatus::Pending | JobStatus::Failed
            )
        {
            after = Some(row);
            break;
        }
        if !worker.poll_once().await.expect("poll") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let after = after.expect("unknown-kind job observed after an attempt");

    assert_eq!(after.status().unwrap(), JobStatus::Pending);
    assert!(after.attempts >= 1, "the job was attempted");
    assert!(after
        .last_error
        .as_deref()
        .unwrap_or("")
        .contains("unknown job kind"));
}

/// The reconciler (SOUL §6.2): a job whose worker crashed mid-run is stuck
/// `running` with a held lease and would never make progress. `reclaim_stale`
/// must re-drive it once the lease is older than the visibility timeout — back
/// to `pending`, lease released, the reclaim recorded — so a fresh worker runs
/// it to completion. A crash loses throughput, never work.
#[tokio::test]
async fn reconciler_reclaims_a_stale_running_job() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping reconciler_reclaims_a_stale_running_job: no test DB");
        return;
    };
    let _guard = queue_lock().lock().await;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("cal.ics"), ICS).expect("fixture");

    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("reconcile", &format!("reconcile-{}", uuid::Uuid::new_v4()))
        .await
        .expect("workspace");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "fixture",
            None,
            Some(serde_json::json!({ "provider": "local", "dir": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");

    let job_id = catalerum_ingest::enqueue_sync(&store, ws.id, conn.id)
        .await
        .expect("enqueue");

    // Simulate a worker that claimed this job and then crashed an hour ago:
    // `running`, lease held, `locked_at` well past any visibility timeout. We set
    // it directly (not via `dequeue_one`, which claims the globally-oldest job)
    // so the scenario is isolated to *our* job and never disturbs a concurrent
    // test's in-flight lease.
    sqlx::query(
        "UPDATE job_queue
         SET status = 'running', attempts = 1, locked_by = 'dead-worker',
             locked_at = now() - interval '1 hour'
         WHERE id = $1",
    )
    .bind(job_id)
    .execute(store.pool())
    .await
    .expect("simulate crashed claim");

    // Reconcile with a 1-minute visibility timeout: only our hour-old lease is
    // stale, so concurrent tests' fresh leases are left untouched (isolation).
    let reclaimed = store
        .job_queue()
        .reclaim_stale(Duration::from_secs(60), 5)
        .await
        .expect("reclaim_stale");
    assert!(reclaimed >= 1, "the stale job is reclaimed");

    // Back to pending, lease released, reclaim recorded — and the crashed
    // attempt is preserved (so a job that reliably crashes its worker still
    // burns down its attempt budget rather than being reclaimed forever).
    let after = store.job_queue().get(job_id).await.expect("get");
    assert_eq!(after.status().unwrap(), JobStatus::Pending);
    assert!(after.locked_at.is_none(), "lease released");
    assert!(after.locked_by.is_none(), "lease holder cleared");
    assert_eq!(after.attempts, 1, "the crashed attempt is preserved");
    assert!(
        after
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("reclaimed"),
        "the reclaim is recorded in last_error: {:?}",
        after.last_error
    );

    // A fresh worker now re-drives the reclaimed job to completion (throughput
    // recovered). Scope the wait to our own job id since the queue is shared.
    let worker = catalerum_ingest::SyncWorker::new(store.clone());
    let mut done = None;
    for _ in 0..16 {
        let row = store.job_queue().get(job_id).await.expect("get");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            done = Some(row);
            break;
        }
        if !worker.poll_once().await.expect("poll_once") {
            done = Some(store.job_queue().get(job_id).await.expect("get"));
            break;
        }
    }
    let done = done.expect("job observed terminal");
    assert_eq!(
        done.status().unwrap(),
        JobStatus::Done,
        "reclaimed job runs to completion; last_error = {:?}",
        done.last_error
    );

    // cleanup
    store
        .connections()
        .delete(ws.id, conn.id)
        .await
        .expect("delete conn");
    store.workspaces().delete(ws.id).await.expect("delete ws");
}
