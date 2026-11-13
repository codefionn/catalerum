use catalerum_core::id::{AutomationId, AutomationRunId, AutomationStepId, GrantId, WorkspaceId};
use catalerum_core::model::{Automation, AutomationRun, AutomationStep, RunStatus, StepStatus};
use sqlx::types::Json;
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{
    AutomationRow, AutomationRunRow, AutomationStepRow, run_status_to_text, step_status_to_text,
};
use super::{map, PgPool};

const AUTOMATION_COLS: &str = "id, workspace_id, name, enabled, triggers, condition, actions, \
     spec, grant_id, created_at, updated_at";

/// The fields to create or upsert an [`Automation`] (SOUL §11). The JSON `triggers`,
/// `condition`, `actions`, and `spec` are stored verbatim as JSONB — the
/// `catalerum-automation` engine owns their typed meaning.
#[derive(Clone, Debug)]
pub struct NewAutomation {
    /// Unique (per workspace) automation name.
    pub name: String,
    /// Whether the automation is active. Disabled automations persist but do not fire.
    pub enabled: bool,
    /// Trigger specs (CalendarEvent / Schedule / Webhook / …).
    pub triggers: Vec<serde_json::Value>,
    /// Optional predicate over store/graph/vectors; `None` fires unconditionally.
    pub condition: Option<serde_json::Value>,
    /// Ordered typed action specs (also the LLM's tools, §11).
    pub actions: Vec<serde_json::Value>,
    /// The full original authoring spec.
    pub spec: Option<serde_json::Value>,
    /// The §19 grant the automation runs under.
    pub grant_id: Option<GrantId>,
}

/// CRUD for the `automations` table (SOUL §11). Automations are named,
/// workspace-scoped definitions; the engine (`catalerum-automation`) reads them
/// to register triggers and dispatch runs.
#[derive(Clone, Debug)]
pub struct AutomationRepo {
    pool: PgPool,
}

impl AutomationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an automation. [`StoreError::Conflict`] if `name` already exists in
    /// the workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        automation: &NewAutomation,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO automations
                 (id, workspace_id, name, enabled, triggers, condition, actions, spec, grant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(AutomationId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&automation.name)
        .bind(automation.enabled)
        .bind(Json(automation.triggers.clone()))
        .bind(automation.condition.as_ref().map(Json))
        .bind(Json(automation.actions.clone()))
        .bind(automation.spec.as_ref().map(Json))
        .bind(automation.grant_id.map(GrantId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert an automation by `(workspace_id, name)` — create it, or
    /// replace its definition (keeping the stable id, §11).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        automation: &NewAutomation,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO automations
                 (id, workspace_id, name, enabled, triggers, condition, actions, spec, grant_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 enabled = EXCLUDED.enabled,
                 triggers = EXCLUDED.triggers,
                 condition = EXCLUDED.condition,
                 actions = EXCLUDED.actions,
                 spec = EXCLUDED.spec,
                 grant_id = EXCLUDED.grant_id,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(AutomationId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&automation.name)
        .bind(automation.enabled)
        .bind(Json(automation.triggers.clone()))
        .bind(automation.condition.as_ref().map(Json))
        .bind(Json(automation.actions.clone()))
        .bind(automation.spec.as_ref().map(Json))
        .bind(automation.grant_id.map(GrantId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an automation by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: AutomationId) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch an automation by name, workspace-scoped, or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Automation>> {
        let row: Option<AutomationRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Automation::from))
    }

    /// List a workspace's automations, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Automation>> {
        let rows: Vec<AutomationRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {AUTOMATION_COLS} FROM automations WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Automation::from).collect())
    }

    /// Enable or disable an automation, workspace-scoped. Returns the updated row;
    /// [`StoreError::NotFound`] if absent. The cheap toggle the engine and UI use
    /// to pause/resume without rewriting the definition.
    pub async fn set_enabled(
        &self,
        workspace_id: WorkspaceId,
        id: AutomationId,
        enabled: bool,
    ) -> Result<Automation> {
        let row: AutomationRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE automations SET enabled = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {AUTOMATION_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(enabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        Ok(row.into())
    }

    /// Delete an automation, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: AutomationId) -> Result<()> {
        let res = sqlx::query("DELETE FROM automations WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}

const RUN_COLS: &str =
    "id, workspace_id, automation_id, status, grant_id, trigger, error, started_at, finished_at";
const STEP_COLS: &str =
    "id, run_id, workspace_id, ordinal, action, status, output, error, started_at, finished_at";

/// Durable run/step state for automations (SOUL §11). The engine opens a run when
/// a matched trigger fires ([`start_run`](AutomationRunRepo::start_run)), records
/// each action as a step, and finalizes both — the audit trail that survives
/// crashes and drives reconciliation. Every method is workspace-scoped (§18), and
/// run/step inserts are guarded so a run can only be opened for an automation in
/// the caller's workspace, and a step only appended to a run in it.
#[derive(Clone, Debug)]
pub struct AutomationRunRepo {
    pool: PgPool,
}

impl AutomationRunRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Open a `Running` run for `automation_id` (SOUL §11). [`StoreError::NotFound`]
    /// if the automation does not exist in `workspace_id` (the tenancy guard).
    ///
    /// `grant_id` is the §19 grant the run executes under, snapshotted onto the run
    /// for audit (which authority was in force for this execution); `None` for a run
    /// under default base authority. It is recorded as a plain value (no FK) so the
    /// audit fact survives the grant's later deletion.
    pub async fn start_run(
        &self,
        workspace_id: WorkspaceId,
        automation_id: AutomationId,
        grant_id: Option<GrantId>,
        trigger: Option<serde_json::Value>,
        job_id: Option<Uuid>,
    ) -> Result<AutomationRun> {
        let row: Option<AutomationRunRow> = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO automation_runs \
                 (id, workspace_id, automation_id, status, grant_id, trigger, job_id)
             SELECT $1, $2, $3, $4, $5, $6, $7
             WHERE EXISTS (SELECT 1 FROM automations WHERE id = $3 AND workspace_id = $2)
             RETURNING {RUN_COLS}"
        ))
        .bind(AutomationRunId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(automation_id.into_uuid())
        .bind(run_status_to_text(RunStatus::Running))
        .bind(grant_id.map(GrantId::into_uuid))
        .bind(trigger.as_ref().map(Json))
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// The active (still-`running`) run spawned by `job_id`, if any (SOUL §5/§11).
    /// How a re-driven `run_automation` job finds the run it must **resume** rather
    /// than re-create after a crash. Newest first; `None` if the job has no live run
    /// (never started, or already finalized).
    pub async fn find_active_run_by_job(
        &self,
        workspace_id: WorkspaceId,
        job_id: Uuid,
    ) -> Result<Option<AutomationRunId>> {
        let row: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM automation_runs
             WHERE workspace_id = $1 AND job_id = $2 AND status = $3
             ORDER BY started_at DESC LIMIT 1",
        )
        .bind(workspace_id.into_uuid())
        .bind(job_id)
        .bind(run_status_to_text(RunStatus::Running))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(|(id,)| AutomationRunId::from_uuid(id)))
    }

    /// The newest run (any status) spawned by `job_id`, if one has started yet.
    /// How a caller that just enqueued a `run_automation` job (the `run_automation`
    /// tool's `wait_secs`) follows that exact run to its outcome.
    pub async fn latest_run_by_job(
        &self,
        workspace_id: WorkspaceId,
        job_id: Uuid,
    ) -> Result<Option<AutomationRun>> {
        let row: Option<AutomationRunRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {RUN_COLS} FROM automation_runs
             WHERE workspace_id = $1 AND job_id = $2
             ORDER BY started_at DESC, id DESC LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(AutomationRun::try_from).transpose()
    }

    /// Finalize a run with a terminal `status` (and optional failure `error`),
    /// stamping `finished_at`. [`StoreError::NotFound`] if absent.
    pub async fn finish_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<AutomationRun> {
        let row: Option<AutomationRunRow> = sqlx::query_as(assert_sql_safe!(
            "UPDATE automation_runs SET status = $3, error = $4, finished_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {RUN_COLS}"
        ))
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(run_status_to_text(status))
        .bind(error)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// Fetch a run by id, workspace-scoped.
    pub async fn get_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<AutomationRun> {
        let row: AutomationRunRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {RUN_COLS} FROM automation_runs WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List an automation's runs, most recent first, capped at `limit`.
    pub async fn list_runs(
        &self,
        workspace_id: WorkspaceId,
        automation_id: AutomationId,
        limit: i64,
    ) -> Result<Vec<AutomationRun>> {
        let rows: Vec<AutomationRunRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {RUN_COLS} FROM automation_runs
             WHERE workspace_id = $1 AND automation_id = $2
             ORDER BY started_at DESC, id DESC LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(automation_id.into_uuid())
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(AutomationRun::try_from).collect()
    }

    /// Append a `Running` step to a run (SOUL §11). [`StoreError::NotFound`] if the
    /// run does not exist in `workspace_id`; [`StoreError::Conflict`] if `ordinal`
    /// is already used in the run (the `(run_id, ordinal)` unique key).
    pub async fn add_step(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        ordinal: i32,
        action: serde_json::Value,
    ) -> Result<AutomationStep> {
        let row: Option<AutomationStepRow> = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO automation_steps (id, run_id, workspace_id, ordinal, action, status)
             SELECT $1, $2, $3, $4, $5, $6
             WHERE EXISTS (SELECT 1 FROM automation_runs WHERE id = $2 AND workspace_id = $3)
             RETURNING {STEP_COLS}"
        ))
        .bind(AutomationStepId::new().into_uuid())
        .bind(run_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(ordinal)
        .bind(Json(action))
        .bind(step_status_to_text(StepStatus::Running))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// Finalize a step with a terminal `status`, optional `output`/`error`,
    /// stamping `finished_at`. [`StoreError::NotFound`] if absent.
    pub async fn finish_step(
        &self,
        workspace_id: WorkspaceId,
        step_id: AutomationStepId,
        status: StepStatus,
        output: Option<serde_json::Value>,
        error: Option<&str>,
    ) -> Result<AutomationStep> {
        let row: Option<AutomationStepRow> = sqlx::query_as(assert_sql_safe!(
            "UPDATE automation_steps SET status = $3, output = $4, error = $5, finished_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {STEP_COLS}"
        ))
        .bind(step_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(step_status_to_text(status))
        .bind(output.as_ref().map(Json))
        .bind(error)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.ok_or(StoreError::NotFound)?.try_into()
    }

    /// List a run's steps in execution order, workspace-scoped.
    pub async fn list_steps(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<Vec<AutomationStep>> {
        let rows: Vec<AutomationStepRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {STEP_COLS} FROM automation_steps
             WHERE workspace_id = $1 AND run_id = $2
             ORDER BY ordinal ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(run_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(AutomationStep::try_from).collect()
    }
}
