use catalerum_core::id::WorkspaceId;
use std::time::Duration;
use chrono::{DateTime, Utc};
use sqlx::types::Json;
use uuid::Uuid;
use crate::error::Result;
use crate::rows::{JobRow, JobStatus};
use super::{cutoff_before, map, PgPool};

// ===========================================================================
// Job queue
// ===========================================================================

/// The full set of `job_queue` columns, in row order.
const JOB_COLS: &str = "id, workspace_id, kind, payload, status, attempts, run_after, \
     locked_at, locked_by, last_error, created_at, updated_at";

/// CRUD + lease semantics for the durable `job_queue` (SOUL §6.2).
///
/// All async work is enqueued here first; a worker claims one runnable job via
/// `SELECT … FOR UPDATE SKIP LOCKED` so concurrent workers never grab the same
/// row. A claimed job is `complete`d on success or `fail`ed with exponential
/// backoff (re-`pending` until `max_attempts`, then terminal `failed`). The
/// single-pod dev path uses exactly this `FOR UPDATE SKIP LOCKED` queue (Valkey
/// disabled, SOUL §6.2). A worker that dies mid-job leaves its row `running`
/// forever; [`Self::reclaim_stale`] is the reconciler that re-drives such
/// orphaned leases past a visibility timeout, so a crash loses throughput, never
/// work (SOUL §6.2).
#[derive(Clone, Debug)]
pub struct JobQueueRepo {
    pool: PgPool,
}

impl JobQueueRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enqueue a job. `workspace_id` is `None` for global maintenance jobs;
    /// `run_after` defaults to `CURRENT_TIMESTAMP` (eligible immediately) when `None`.
    /// Returns the stored job row.
    pub async fn enqueue(
        &self,
        workspace_id: Option<WorkspaceId>,
        kind: &str,
        payload: serde_json::Value,
        run_after: Option<DateTime<Utc>>,
    ) -> Result<JobRow> {
        let id = Uuid::new_v4();
        let row: JobRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO job_queue (id, workspace_id, kind, payload, run_after)
             VALUES ($1, $2, $3, $4, COALESCE($5, CURRENT_TIMESTAMP))
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.map(WorkspaceId::into_uuid))
        .bind(kind)
        .bind(Json(payload))
        .bind(run_after)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Atomically claim the oldest runnable pending job and mark it `running`,
    /// leased to `worker`. Uses `FOR UPDATE SKIP LOCKED` so concurrent workers
    /// never claim the same row (SOUL §6.2). Returns `None` when the queue is
    /// empty / nothing is yet eligible (`run_after <= CURRENT_TIMESTAMP`).
    pub async fn dequeue_one(&self, worker: &str) -> Result<Option<JobRow>> {
        #[cfg(not(feature = "sqlite"))]
        let candidate_lock = "FOR UPDATE SKIP LOCKED";
        // SQLite serializes writers. The UPDATE-with-subquery is atomic and is
        // therefore the native equivalent for the single-node configuration.
        #[cfg(feature = "sqlite")]
        let candidate_lock = "";
        #[cfg(not(feature = "sqlite"))]
        let runnable = "run_after <= CURRENT_TIMESTAMP";
        #[cfg(feature = "sqlite")]
        let runnable = "julianday(run_after) <= julianday(CURRENT_TIMESTAMP)";
        let row: Option<JobRow> = sqlx::query_as(assert_sql_safe!(
            "UPDATE job_queue SET
                 status     = 'running',
                 attempts   = attempts + 1,
                 locked_at  = CURRENT_TIMESTAMP,
                 locked_by  = $1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = (
                 SELECT id FROM job_queue
                 WHERE status = 'pending' AND {runnable}
                 ORDER BY run_after ASC, created_at ASC
                 {candidate_lock}
                 LIMIT 1
             )
             RETURNING {JOB_COLS}"
        ))
        .bind(worker)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fetch a job by id (any status), or [`StoreError::NotFound`](crate::error::StoreError::NotFound).
    pub async fn get(&self, id: Uuid) -> Result<JobRow> {
        let row: JobRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {JOB_COLS} FROM job_queue WHERE id = $1"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Mark a running job `done` (terminal, success) and release its lease.
    /// Returns the updated row, or [`StoreError::NotFound`](crate::error::StoreError::NotFound) if the id is unknown.
    pub async fn complete(&self, id: Uuid) -> Result<JobRow> {
        let row: JobRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE job_queue SET
                 status     = 'done',
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fail a job, recording `error`. If `attempts < max_attempts` the job is
    /// re-queued (`pending`) with exponential backoff — `run_after = CURRENT_TIMESTAMP +
    /// backoff_base * 2^(attempts-1)` — so a transient failure retries later;
    /// otherwise it becomes terminal `failed`. The lease is released either way.
    /// Returns the updated row.
    pub async fn fail(
        &self,
        id: Uuid,
        error: &str,
        max_attempts: i32,
        backoff_base: Duration,
    ) -> Result<JobRow> {
        #[cfg(not(feature = "sqlite"))]
        let backoff_secs = backoff_base.as_secs_f64().max(0.0);
        #[cfg(not(feature = "sqlite"))]
        let row: JobRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE job_queue SET
                 status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                 run_after  = CASE WHEN attempts >= $2 THEN run_after
                                   ELSE CURRENT_TIMESTAMP + make_interval(
                                       secs => $4 * power(2, GREATEST(attempts - 1, 0)))
                              END,
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = $3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
             RETURNING {JOB_COLS}"
        ))
        .bind(id)
        .bind(max_attempts)
        .bind(error)
        .bind(backoff_secs)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        #[cfg(feature = "sqlite")]
        let row: JobRow = {
            let (attempts,): (i32,) =
                sqlx::query_as("SELECT attempts FROM job_queue WHERE id = $1")
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(map)?;
            let exponent = u32::try_from((attempts - 1).clamp(0, 31)).unwrap_or(0);
            let delay = backoff_base.saturating_mul(1_u32 << exponent);
            let retry_at = Utc::now()
                .checked_add_signed(
                    chrono::Duration::from_std(delay).unwrap_or(chrono::Duration::MAX),
                )
                .unwrap_or(DateTime::<Utc>::MAX_UTC);
            sqlx::query_as(assert_sql_safe!(
                "UPDATE job_queue SET
                     status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                     run_after  = CASE WHEN attempts >= $2 THEN run_after ELSE datetime($4) END,
                     locked_at  = NULL,
                     locked_by  = NULL,
                     last_error = $3,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1
                 RETURNING {JOB_COLS}"
            ))
            .bind(id)
            .bind(max_attempts)
            .bind(error)
            .bind(retry_at)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?
        };
        Ok(row)
    }

    /// Reconcile stale leases (SOUL §6.2): re-drive any job stuck `running` whose
    /// lease has expired — `locked_at < CURRENT_TIMESTAMP - visibility_timeout`. A worker that
    /// claims a job (so `dequeue_one` flipped it to `running` and bumped
    /// `attempts`) and then dies before [`Self::complete`]/[`Self::fail`] leaves
    /// the row `running` forever; this is the reconciler that re-drives it so a
    /// crash loses throughput, never work.
    ///
    /// A reclaimed job whose `attempts` is still below `max_attempts` returns to
    /// `pending`, eligible immediately (`run_after = CURRENT_TIMESTAMP`), lease released; one
    /// that has already exhausted its attempts becomes terminal `failed` (so a
    /// job that reliably crashes its worker cannot be reclaimed forever — the
    /// crashed claim already consumed an attempt). `last_error` records the
    /// reclaim. Returns the number of jobs reclaimed.
    ///
    /// The single `UPDATE` is atomic, so running this from several workers (or a
    /// dedicated reconciler) never double-drives a row. `visibility_timeout` must
    /// exceed the longest expected job runtime, or a still-running job is reclaimed
    /// and run twice (harmless only for idempotent jobs, SOUL §3.4).
    pub async fn reclaim_stale(
        &self,
        visibility_timeout: Duration,
        max_attempts: i32,
    ) -> Result<u64> {
        let cutoff = cutoff_before(visibility_timeout);
        #[cfg(not(feature = "sqlite"))]
        let expired = "locked_at < $1";
        #[cfg(feature = "sqlite")]
        let expired = "julianday(locked_at) < julianday($1)";
        let res = sqlx::query(assert_sql_safe!(
            "UPDATE job_queue SET
                 status     = CASE WHEN attempts >= $2 THEN 'failed' ELSE 'pending' END,
                 run_after  = CASE WHEN attempts >= $2 THEN run_after ELSE CURRENT_TIMESTAMP END,
                 locked_at  = NULL,
                 locked_by  = NULL,
                 last_error = $3,
                 updated_at = CURRENT_TIMESTAMP
             WHERE status = 'running'
               AND locked_at IS NOT NULL
               AND {expired}"
        ))
        .bind(cutoff)
        .bind(max_attempts)
        .bind("reclaimed: worker lease expired past visibility timeout")
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Count jobs in a given lifecycle status (across all workspaces). Handy for
    /// metrics / draining checks.
    pub async fn count_by_status(&self, status: JobStatus) -> Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM job_queue WHERE status = $1")
            .bind(status.as_str())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        Ok(count)
    }

    /// Garbage-collect terminal (`done`/`failed`) jobs older than `before`.
    /// Returns the number deleted.
    pub async fn delete_terminal_before(&self, before: DateTime<Utc>) -> Result<u64> {
        #[cfg(not(feature = "sqlite"))]
        let older = "updated_at < $1";
        #[cfg(feature = "sqlite")]
        let older = "julianday(updated_at) < julianday($1)";
        let res = sqlx::query(assert_sql_safe!(
            "DELETE FROM job_queue
             WHERE status IN ('done', 'failed') AND {older}"
        ))
        .bind(before)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}
