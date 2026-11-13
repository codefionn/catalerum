use catalerum_core::id::{TerminalSessionId, WorkspaceId};
use catalerum_core::model::{
    ExecutorKind, SandboxState, TerminalSession, TerminalSessionStatus, WorkspaceSandboxRecord,
};
use std::time::Duration;
use chrono::{DateTime, Utc};
use crate::error::{Result, StoreError};
use crate::rows::{
    sandbox_state_to_text, terminal_session_status_to_text, TerminalSessionRow, WorkspaceSandboxRow,
};
use super::{cutoff_before, map, PgPool};

const TERMINAL_SESSION_COLS: &str = "id, workspace_id, backend, status, \
     host_dir, sync_prefix, pod_id, created_at, closed_at";

/// The fields to record a new [`TerminalSession`] (SOUL §20).
#[derive(Clone, Debug)]
pub struct NewTerminalSession {
    pub backend: ExecutorKind,
    pub host_dir: Option<String>,
    pub sync_prefix: Option<String>,
    /// The pod (process) that owns this session's node-local PTY (multi-pod HA,
    /// SOUL §16 M7). `None` only for a caller that predates pod ownership.
    pub pod_id: Option<String>,
}

/// CRUD for the `terminal_sessions` table (SOUL §20). Every query is
/// workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct TerminalSessionRepo {
    pool: PgPool,
}

impl TerminalSessionRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new (active) session.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        session: &NewTerminalSession,
    ) -> Result<TerminalSession> {
        let row: TerminalSessionRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO terminal_sessions
                 (id, workspace_id, backend, host_dir, sync_prefix, pod_id)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {TERMINAL_SESSION_COLS}"
        ))
        .bind(TerminalSessionId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(session.backend.as_token())
        .bind(&session.host_dir)
        .bind(&session.sync_prefix)
        .bind(&session.pod_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a session by id, workspace-scoped, or `None`.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<Option<TerminalSession>> {
        let row: Option<TerminalSessionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions \
             WHERE workspace_id = $1 AND id = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(TryInto::try_into).transpose()
    }

    /// Update a session's lifecycle status, stamping `closed_at` when it leaves
    /// the active state. Workspace-scoped; [`StoreError::NotFound`] if absent.
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        status: TerminalSessionStatus,
    ) -> Result<TerminalSession> {
        let closed = !matches!(status, TerminalSessionStatus::Active);
        let row: TerminalSessionRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE terminal_sessions
                SET status = $3,
                    closed_at = CASE WHEN $4 THEN CURRENT_TIMESTAMP ELSE closed_at END
              WHERE workspace_id = $1 AND id = $2
             RETURNING {TERMINAL_SESSION_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(terminal_session_status_to_text(status))
        .bind(closed)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        row.try_into()
    }

    /// Record the object-storage key prefix a session was last persisted under.
    pub async fn set_sync_prefix(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        prefix: &str,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE terminal_sessions SET sync_prefix = $3 WHERE workspace_id = $1 AND id = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(prefix)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List a workspace's sessions, newest first.
    pub async fn list_by_workspace(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<TerminalSession>> {
        let rows: Vec<TerminalSessionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions WHERE workspace_id = $1 \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// List a workspace's **active** sessions, newest first.
    pub async fn list_active(&self, workspace_id: WorkspaceId) -> Result<Vec<TerminalSession>> {
        let rows: Vec<TerminalSessionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TERMINAL_SESSION_COLS} FROM terminal_sessions \
             WHERE workspace_id = $1 AND status = 'active' \
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Mark **every** still-`active` session as closed, stamping `closed_at`.
    /// A boot/maintenance operation and the one query here that is deliberately
    /// **not** workspace-scoped (§18): the interactive PTY / container / Pod
    /// handles all live in the `TerminalManager`'s process memory, so a process
    /// restart orphans every active row — none can survive. Returns how many
    /// rows were reconciled. Idempotent (a second call closes nothing).
    pub async fn close_all_active(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active'",
        )
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Boot reconcile **scoped to one owning pod** (multi-pod HA, SOUL §16 M7).
    /// Closes every still-`active` session that either belongs to `pod_id` (this
    /// restarting process — its in-memory PTYs died with the prior run) or has a
    /// NULL `pod_id` (a pre-upgrade orphan whose owner is unknowable; whichever
    /// pod boots first reclaims it, harmlessly). A peer pod's rows are left
    /// untouched so a rolling restart never marks another pod's live sessions
    /// closed. Returns how many rows were reconciled; idempotent.
    pub async fn close_all_active_for_pod(&self, pod_id: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active' AND (pod_id = $1 OR pod_id IS NULL)",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Stale-pod reclaim (pod-heartbeat follow-up, SOUL §20/§16 M7): close every
    /// still-`active` session whose owning pod is **provably dead** — it has a
    /// [`pod_heartbeats`](PodHeartbeatRepo) row that has gone stale (its
    /// `last_seen` is older than `grace`). Rows only: the dead pod's PTYs/containers
    /// died with it, so this just clears the phantom `active` row a permanently-dead
    /// pod leaves behind (the common case under a Deployment, where a replaced pod
    /// never returns under its old HOSTNAME).
    ///
    /// # The never-heartbeated safety rule
    /// This matches only pods that HAVE a stale heartbeat row (`pod_id IN (SELECT …
    /// WHERE last_seen < cutoff)`), **never** a `pod_id` with no heartbeat row at
    /// all. Two cases motivate this:
    /// * A **freshly-booted** pod writes its first heartbeat before it can create
    ///   any session (main.rs stamps it before boot reconcile / before serving), so
    ///   its rows never look stale — and even in the boot window before that write,
    ///   the absent-row rule protects them.
    /// * A pod running **pre-heartbeat code** during a rolling upgrade never writes
    ///   a heartbeat; its still-live rows must not be swept. Leaving never-heartbeated
    ///   pods to the legacy NULL / per-pod boot-reconcile path
    ///   ([`close_all_active_for_pod`](Self::close_all_active_for_pod)) is the
    ///   conservative choice.
    ///
    /// `grace` must be generously larger than the heartbeat interval (a
    /// paused-but-alive pod that resumes within `grace` keeps its rows). Runs on
    /// every pod but is naturally **idempotent**: it only matches `status =
    /// 'active'` rows, so a second run (or a concurrent peer's run) closes nothing —
    /// no bus lock needed. Returns how many rows were reclaimed.
    pub async fn reclaim_stale_for_dead_pods(&self, grace: Duration) -> Result<u64> {
        let cutoff = cutoff_before(grace);
        #[cfg(not(feature = "sqlite"))]
        let heartbeat_expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let heartbeat_expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(assert_sql_safe!(
            "UPDATE terminal_sessions \
                SET status = 'closed', closed_at = CURRENT_TIMESTAMP \
              WHERE status = 'active' \
                AND pod_id IS NOT NULL \
                AND pod_id IN ( \
                    SELECT pod_id FROM pod_heartbeats \
                     WHERE {heartbeat_expired} \
                )"
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Delete a session by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: TerminalSessionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM terminal_sessions WHERE workspace_id = $1 AND id = $2")
            .bind(workspace_id.into_uuid())
            .bind(id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

const WORKSPACE_SANDBOX_COLS: &str = "workspace_id, backend, image, status, container_ref, \
     volume_ref, pod_id, last_activity, created_at, updated_at";

/// The fields to upsert a [`WorkspaceSandboxRecord`] (SOUL §20).
#[derive(Clone, Debug)]
pub struct NewWorkspaceSandbox {
    pub backend: ExecutorKind,
    pub image: String,
    pub status: SandboxState,
    pub container_ref: Option<String>,
    pub volume_ref: Option<String>,
    /// The pod (process) provisioning this sandbox's node-local container/Pod
    /// (multi-pod HA, SOUL §16 M7). `None` only for a caller that predates it.
    pub pod_id: Option<String>,
}

/// CRUD for the `workspace_sandboxes` table (SOUL §20). Exactly one row per
/// workspace (`workspace_id` is the primary key); every query is
/// workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct WorkspaceSandboxRepo {
    pool: PgPool,
}

impl WorkspaceSandboxRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently upsert a workspace's sandbox record by `workspace_id`,
    /// refreshing `last_activity` and `updated_at`.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        sandbox: &NewWorkspaceSandbox,
    ) -> Result<WorkspaceSandboxRecord> {
        let row: WorkspaceSandboxRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO workspace_sandboxes
                 (workspace_id, backend, image, status, container_ref, volume_ref, pod_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (workspace_id) DO UPDATE SET
                 backend = EXCLUDED.backend,
                 image = EXCLUDED.image,
                 status = EXCLUDED.status,
                 container_ref = EXCLUDED.container_ref,
                 volume_ref = EXCLUDED.volume_ref,
                 pod_id = EXCLUDED.pod_id,
                 last_activity = CURRENT_TIMESTAMP,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {WORKSPACE_SANDBOX_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(sandbox.backend.as_token())
        .bind(&sandbox.image)
        .bind(sandbox_state_to_text(sandbox.status))
        .bind(&sandbox.container_ref)
        .bind(&sandbox.volume_ref)
        .bind(&sandbox.pod_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a workspace's sandbox record, or `None`.
    pub async fn get(&self, workspace_id: WorkspaceId) -> Result<Option<WorkspaceSandboxRecord>> {
        let row: Option<WorkspaceSandboxRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {WORKSPACE_SANDBOX_COLS} FROM workspace_sandboxes WHERE workspace_id = $1"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(TryInto::try_into).transpose()
    }

    /// Update a workspace sandbox's lifecycle status. [`StoreError::NotFound`] if
    /// no row exists.
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        status: SandboxState,
    ) -> Result<WorkspaceSandboxRecord> {
        let row: WorkspaceSandboxRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE workspace_sandboxes SET status = $2, updated_at = CURRENT_TIMESTAMP \
              WHERE workspace_id = $1 \
             RETURNING {WORKSPACE_SANDBOX_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(sandbox_state_to_text(status))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        row.try_into()
    }

    /// Bump a workspace sandbox's `last_activity` to now (drives idle reaping).
    /// [`StoreError::NotFound`] if no row exists.
    pub async fn touch_activity(&self, workspace_id: WorkspaceId) -> Result<()> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET last_activity = CURRENT_TIMESTAMP, updated_at = CURRENT_TIMESTAMP \
              WHERE workspace_id = $1",
        )
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List every sandbox record (all workspaces). For observability + the boot
    /// reconcile; not workspace-scoped by design.
    pub async fn list_all(&self) -> Result<Vec<WorkspaceSandboxRecord>> {
        let rows: Vec<WorkspaceSandboxRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {WORKSPACE_SANDBOX_COLS} FROM workspace_sandboxes ORDER BY created_at ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Delete a workspace's sandbox record. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId) -> Result<()> {
        let res = sqlx::query("DELETE FROM workspace_sandboxes WHERE workspace_id = $1")
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Mark **every** non-stopped sandbox row `stopped` — a boot reconcile (the
    /// live container/Pod handles live in the API process memory, so a restart
    /// orphans them; on podman the deterministic name lets `ensure` re-adopt).
    /// Returns how many rows were reconciled. Deliberately not workspace-scoped.
    pub async fn mark_all_stopped(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped'",
        )
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Boot reconcile **scoped to one owning pod** (multi-pod HA, SOUL §16 M7).
    /// Marks `stopped` every non-stopped sandbox row that either belongs to
    /// `pod_id` (this restarting process — its container/Pod handle died with the
    /// prior run; podman re-adopts by name on the next `ensure`) or has a NULL
    /// `pod_id` (a pre-upgrade orphan). A peer pod's rows are left untouched, so a
    /// rolling restart never stomps another pod's live sandbox. Returns how many
    /// rows were reconciled; idempotent.
    pub async fn mark_all_stopped_for_pod(&self, pod_id: &str) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped' AND (pod_id = $1 OR pod_id IS NULL)",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Stale-pod reclaim (pod-heartbeat follow-up, SOUL §20/§16 M7): mark `stopped`
    /// every non-stopped sandbox row whose owning pod is **provably dead** — it has
    /// a [`pod_heartbeats`](PodHeartbeatRepo) row that has gone stale (older than
    /// `grace`). The sandbox row is the api-layer bookkeeping; the dead pod's
    /// container/Pod handle died with it (on podman a deterministic name lets a
    /// future `ensure` re-adopt). Uses the same **never-heartbeated safety rule** as
    /// [`TerminalSessionRepo::reclaim_stale_for_dead_pods`] (see its docs): a
    /// `pod_id` with no heartbeat row is never swept. Naturally idempotent (only
    /// `status <> 'stopped'` rows match), so it needs no bus lock and is safe to run
    /// on every pod. Returns how many rows were reclaimed.
    pub async fn reclaim_stale_for_dead_pods(&self, grace: Duration) -> Result<u64> {
        let cutoff = cutoff_before(grace);
        #[cfg(not(feature = "sqlite"))]
        let heartbeat_expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let heartbeat_expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(assert_sql_safe!(
            "UPDATE workspace_sandboxes SET status = 'stopped', updated_at = CURRENT_TIMESTAMP \
              WHERE status <> 'stopped' \
                AND pod_id IS NOT NULL \
                AND pod_id IN ( \
                    SELECT pod_id FROM pod_heartbeats \
                     WHERE {heartbeat_expired} \
                )"
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}

/// CRUD for the `pod_heartbeats` table (pod-heartbeat follow-up, SOUL §20/§16 M7):
/// a per-process liveness signal that lets the stale-pod reclaim sweeps
/// ([`TerminalSessionRepo::reclaim_stale_for_dead_pods`] /
/// [`WorkspaceSandboxRepo::reclaim_stale_for_dead_pods`]) tell a permanently-dead
/// pod apart from a paused-but-alive one. Deliberately not workspace-scoped — a pod
/// spans every workspace it hosts sessions for.
#[derive(Clone, Debug)]
pub struct PodHeartbeatRepo {
    pool: PgPool,
}

impl PodHeartbeatRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Upsert this process's heartbeat, stamping `last_seen = CURRENT_TIMESTAMP`. Called on a
    /// short interval (~30 s) by every pod. Cheap (a single-row upsert on a tiny
    /// table); the caller logs-and-continues on error rather than crashing.
    pub async fn heartbeat(&self, pod_id: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO pod_heartbeats (pod_id, last_seen) VALUES ($1, CURRENT_TIMESTAMP) \
             ON CONFLICT (pod_id) DO UPDATE SET last_seen = CURRENT_TIMESTAMP",
        )
        .bind(pod_id)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// Fetch a pod's last-seen timestamp, or `None` if it has never heartbeated.
    /// For observability + tests.
    pub async fn last_seen(&self, pod_id: &str) -> Result<Option<DateTime<Utc>>> {
        let row: Option<(DateTime<Utc>,)> =
            sqlx::query_as("SELECT last_seen FROM pod_heartbeats WHERE pod_id = $1")
                .bind(pod_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(map)?;
        Ok(row.map(|(ts,)| ts))
    }

    /// Delete heartbeat rows not seen within `horizon`, so the table can't grow
    /// unbounded as pods come and go (e.g. a 7-day horizon). By the time a dead
    /// pod's row ages past the horizon its owned session/sandbox rows are long
    /// reclaimed, so pruning it is purely housekeeping. Returns rows deleted.
    pub async fn prune(&self, horizon: Duration) -> Result<u64> {
        let cutoff = cutoff_before(horizon);
        #[cfg(not(feature = "sqlite"))]
        let expired = "last_seen < $1";
        #[cfg(feature = "sqlite")]
        let expired = "julianday(last_seen) < julianday($1)";
        let res = sqlx::query(assert_sql_safe!(
            "DELETE FROM pod_heartbeats WHERE {expired}"
        ))
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}
