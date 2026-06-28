//! Postgres adapter for the storage-neutral automation engine.
//!
//! This is the only execution module that knows `catalerum-store`. Keeping the
//! adapter at the edge lets the engine run against an in-memory journal in tests
//! or a different durable backend without changing orchestration.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use catalerum_core::model::{Automation, AutomationStep, Grant, RunStatus, StepStatus};
use catalerum_core::{AutomationRun, AutomationRunId, AutomationStepId, GrantId, WorkspaceId};
use catalerum_store::{Store, StoreError};

use crate::engine::{AutomationEngine, ExecutionError, ExecutionState};
use crate::{ActionRunner, CodeRunner};

/// Adapter from the Postgres source of truth to the engine's narrow state port.
pub struct PostgresExecutionState<'a> {
    store: &'a Store,
}

impl<'a> PostgresExecutionState<'a> {
    #[must_use]
    pub fn new(store: &'a Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ExecutionState for PostgresExecutionState<'_> {
    async fn find_active_run_by_job(
        &self,
        workspace_id: WorkspaceId,
        job_id: Uuid,
    ) -> Result<Option<AutomationRunId>, ExecutionError> {
        self.store
            .automation_runs()
            .find_active_run_by_job(workspace_id, job_id)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn start_run(
        &self,
        workspace_id: WorkspaceId,
        automation: &Automation,
        trigger: Option<Value>,
        job_id: Option<Uuid>,
    ) -> Result<AutomationRun, ExecutionError> {
        self.store
            .automation_runs()
            .start_run(
                workspace_id,
                automation.id,
                automation.grant_id,
                trigger,
                job_id,
            )
            .await
            .map_err(ExecutionError::backend)
    }

    async fn get_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<AutomationRun, ExecutionError> {
        self.store
            .automation_runs()
            .get_run(workspace_id, run_id)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn list_steps(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<Vec<AutomationStep>, ExecutionError> {
        self.store
            .automation_runs()
            .list_steps(workspace_id, run_id)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn add_step(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        ordinal: i32,
        action: Value,
    ) -> Result<AutomationStep, ExecutionError> {
        self.store
            .automation_runs()
            .add_step(workspace_id, run_id, ordinal, action)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn finish_step(
        &self,
        workspace_id: WorkspaceId,
        step_id: AutomationStepId,
        status: StepStatus,
        output: Option<Value>,
        error: Option<&str>,
    ) -> Result<AutomationStep, ExecutionError> {
        self.store
            .automation_runs()
            .finish_step(workspace_id, step_id, status, output, error)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn finish_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<AutomationRun, ExecutionError> {
        self.store
            .automation_runs()
            .finish_run(workspace_id, run_id, status, error)
            .await
            .map_err(ExecutionError::backend)
    }

    async fn resolve_grant(
        &self,
        workspace_id: WorkspaceId,
        grant_id: GrantId,
    ) -> Result<Grant, ExecutionError> {
        self.store
            .grants()
            .get(workspace_id, grant_id)
            .await
            .map_err(ExecutionError::backend)
    }
}

/// Compatibility entry point for direct Postgres-backed callers.
pub async fn execute(
    store: &Store,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    automation: &Automation,
    trigger: Option<Value>,
) -> Result<AutomationRun, StoreError> {
    let state = PostgresExecutionState::new(store);
    AutomationEngine::new(&state, runner, code)
        .execute(workspace_id, automation, trigger)
        .await
        .map_err(to_store_error)
}

/// Compatibility entry point for resumable Postgres-backed job callers.
pub async fn execute_for_job(
    store: &Store,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    automation: &Automation,
    trigger: Option<Value>,
    job_id: Option<Uuid>,
) -> Result<AutomationRun, StoreError> {
    let state = PostgresExecutionState::new(store);
    AutomationEngine::new(&state, runner, code)
        .execute_for_job(workspace_id, automation, trigger, job_id)
        .await
        .map_err(to_store_error)
}

fn to_store_error(error: ExecutionError) -> StoreError {
    match error {
        ExecutionError::State(message) => StoreError::Invalid(message),
        ExecutionError::Backend(source) => match source.downcast::<StoreError>() {
            Ok(store) => *store,
            Err(source) => StoreError::Invalid(source.to_string()),
        },
    }
}
