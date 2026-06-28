//! Storage-neutral automation-engine facade and persistence ports.
//!
//! The automation backend is split into three small layers:
//!
//! - [`AutomationEngine`] owns execution orchestration.
//! - [`ExecutionState`] is the durable run journal and authority-snapshot port.
//! - [`ActionRunner`](crate::ActionRunner) and [`CodeRunner`](crate::CodeRunner) are
//!   runtime adapter ports for side effects and sandboxed code.
//!
//! Neither the engine nor the port names SQL, HTTP, the web editor, or a concrete
//! provider. The feature-gated Postgres adapter lives at the crate edge; API and
//! web code remain a control plane that only authors definitions and reads run
//! projections.

use async_trait::async_trait;
use serde_json::Value;
use uuid::Uuid;

use catalerum_core::model::{Automation, AutomationStep, Grant, RunStatus, StepStatus};
use catalerum_core::{AutomationRun, AutomationRunId, AutomationStepId, GrantId, WorkspaceId};

use crate::executor::{execute_with_state, ActionRunner, CodeRunner};

/// A backend-engine failure at one of its durable state boundaries.
#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    /// The run journal or authority snapshot could not be read/written.
    #[error("automation execution state: {0}")]
    State(String),

    /// An adapter error retained as an opaque source so a compatibility adapter
    /// can recover its concrete error without coupling the engine to that type.
    #[error("automation execution backend: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl ExecutionError {
    /// Convert an adapter-specific error without exposing that adapter's type to
    /// the storage-neutral execution core.
    #[must_use]
    pub fn state(error: impl std::fmt::Display) -> Self {
        Self::State(error.to_string())
    }

    /// Retain an adapter's concrete error behind a standard error boundary.
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// Durable state required by the execution engine.
///
/// This deliberately excludes automation-definition CRUD, trigger matching, and
/// job dispatch. Those belong to the control/dispatch planes. The engine receives
/// one immutable [`Automation`] snapshot and only journals its run plus resolves
/// the snapshotted grant.
#[async_trait]
pub trait ExecutionState: Send + Sync {
    async fn find_active_run_by_job(
        &self,
        workspace_id: WorkspaceId,
        job_id: Uuid,
    ) -> Result<Option<AutomationRunId>, ExecutionError>;

    async fn start_run(
        &self,
        workspace_id: WorkspaceId,
        automation: &Automation,
        trigger: Option<Value>,
        job_id: Option<Uuid>,
    ) -> Result<AutomationRun, ExecutionError>;

    async fn get_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<AutomationRun, ExecutionError>;

    async fn list_steps(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
    ) -> Result<Vec<AutomationStep>, ExecutionError>;

    async fn add_step(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        ordinal: i32,
        action: Value,
    ) -> Result<AutomationStep, ExecutionError>;

    async fn finish_step(
        &self,
        workspace_id: WorkspaceId,
        step_id: AutomationStepId,
        status: StepStatus,
        output: Option<Value>,
        error: Option<&str>,
    ) -> Result<AutomationStep, ExecutionError>;

    async fn finish_run(
        &self,
        workspace_id: WorkspaceId,
        run_id: AutomationRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<AutomationRun, ExecutionError>;

    async fn resolve_grant(
        &self,
        workspace_id: WorkspaceId,
        grant_id: GrantId,
    ) -> Result<Grant, ExecutionError>;
}

/// The storage-neutral backend execution service.
///
/// It is intentionally borrowed and cheap to construct, so a worker can bind one
/// request to its state/action/code adapters without those adapters knowing about
/// queues, HTTP, or frontend models.
pub struct AutomationEngine<'a> {
    state: &'a dyn ExecutionState,
    actions: &'a dyn ActionRunner,
    code: &'a dyn CodeRunner,
}

impl<'a> AutomationEngine<'a> {
    #[must_use]
    pub fn new(
        state: &'a dyn ExecutionState,
        actions: &'a dyn ActionRunner,
        code: &'a dyn CodeRunner,
    ) -> Self {
        Self {
            state,
            actions,
            code,
        }
    }

    /// Execute a fresh, direct run.
    pub async fn execute(
        &self,
        workspace_id: WorkspaceId,
        automation: &Automation,
        trigger: Option<Value>,
    ) -> Result<AutomationRun, ExecutionError> {
        self.execute_for_job(workspace_id, automation, trigger, None)
            .await
    }

    /// Execute or resume the run driven by `job_id`.
    pub async fn execute_for_job(
        &self,
        workspace_id: WorkspaceId,
        automation: &Automation,
        trigger: Option<Value>,
        job_id: Option<Uuid>,
    ) -> Result<AutomationRun, ExecutionError> {
        execute_with_state(
            self.state,
            self.actions,
            self.code,
            workspace_id,
            automation,
            trigger,
            job_id,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::Utc;
    use serde_json::json;

    use catalerum_core::{AutomationId, AutomationStepId};

    use super::*;
    use crate::{Action, ActionOutcome, FailCodeRunner};

    #[derive(Default)]
    struct MemoryState {
        runs: Mutex<Vec<AutomationRun>>,
        steps: Mutex<Vec<AutomationStep>>,
    }

    #[async_trait]
    impl ExecutionState for MemoryState {
        async fn find_active_run_by_job(
            &self,
            _workspace_id: WorkspaceId,
            _job_id: Uuid,
        ) -> Result<Option<AutomationRunId>, ExecutionError> {
            Ok(None)
        }

        async fn start_run(
            &self,
            workspace_id: WorkspaceId,
            automation: &Automation,
            trigger: Option<Value>,
            _job_id: Option<Uuid>,
        ) -> Result<AutomationRun, ExecutionError> {
            if automation.workspace_id != workspace_id {
                return Err(ExecutionError::state("workspace mismatch"));
            }
            let run = AutomationRun {
                id: AutomationRunId::new(),
                workspace_id,
                automation_id: automation.id,
                status: RunStatus::Running,
                grant_id: automation.grant_id,
                trigger,
                error: None,
                started_at: Utc::now(),
                finished_at: None,
            };
            self.runs.lock().unwrap().push(run.clone());
            Ok(run)
        }

        async fn get_run(
            &self,
            workspace_id: WorkspaceId,
            run_id: AutomationRunId,
        ) -> Result<AutomationRun, ExecutionError> {
            self.runs
                .lock()
                .unwrap()
                .iter()
                .find(|run| run.workspace_id == workspace_id && run.id == run_id)
                .cloned()
                .ok_or_else(|| ExecutionError::state("run not found"))
        }

        async fn list_steps(
            &self,
            workspace_id: WorkspaceId,
            run_id: AutomationRunId,
        ) -> Result<Vec<AutomationStep>, ExecutionError> {
            Ok(self
                .steps
                .lock()
                .unwrap()
                .iter()
                .filter(|step| step.workspace_id == workspace_id && step.run_id == run_id)
                .cloned()
                .collect())
        }

        async fn add_step(
            &self,
            workspace_id: WorkspaceId,
            run_id: AutomationRunId,
            ordinal: i32,
            action: Value,
        ) -> Result<AutomationStep, ExecutionError> {
            let step = AutomationStep {
                id: AutomationStepId::new(),
                run_id,
                workspace_id,
                ordinal,
                action,
                status: StepStatus::Running,
                output: None,
                error: None,
                started_at: Utc::now(),
                finished_at: None,
            };
            self.steps.lock().unwrap().push(step.clone());
            Ok(step)
        }

        async fn finish_step(
            &self,
            workspace_id: WorkspaceId,
            step_id: AutomationStepId,
            status: StepStatus,
            output: Option<Value>,
            error: Option<&str>,
        ) -> Result<AutomationStep, ExecutionError> {
            let mut steps = self.steps.lock().unwrap();
            let step = steps
                .iter_mut()
                .find(|step| step.workspace_id == workspace_id && step.id == step_id)
                .ok_or_else(|| ExecutionError::state("step not found"))?;
            step.status = status;
            step.output = output;
            step.error = error.map(str::to_owned);
            step.finished_at = Some(Utc::now());
            Ok(step.clone())
        }

        async fn finish_run(
            &self,
            workspace_id: WorkspaceId,
            run_id: AutomationRunId,
            status: RunStatus,
            error: Option<&str>,
        ) -> Result<AutomationRun, ExecutionError> {
            let mut runs = self.runs.lock().unwrap();
            let run = runs
                .iter_mut()
                .find(|run| run.workspace_id == workspace_id && run.id == run_id)
                .ok_or_else(|| ExecutionError::state("run not found"))?;
            run.status = status;
            run.error = error.map(str::to_owned);
            run.finished_at = Some(Utc::now());
            Ok(run.clone())
        }

        async fn resolve_grant(
            &self,
            _workspace_id: WorkspaceId,
            _grant_id: GrantId,
        ) -> Result<Grant, ExecutionError> {
            Err(ExecutionError::state("grant not found"))
        }
    }

    struct EchoAction;

    #[async_trait]
    impl ActionRunner for EchoAction {
        async fn run(
            &self,
            _workspace_id: WorkspaceId,
            action: &Action,
            trigger: Option<&Value>,
            _grant: Option<&Grant>,
        ) -> ActionOutcome {
            ActionOutcome::succeeded(Some(json!({
                "kind": format!("{:?}", action.kind),
                "trigger": trigger,
            })))
        }
    }

    #[tokio::test]
    async fn engine_executes_against_an_in_memory_state_port() {
        let workspace_id = WorkspaceId::new();
        let automation = Automation {
            id: AutomationId::new(),
            workspace_id,
            name: "storage-free".into(),
            enabled: true,
            triggers: vec![json!({ "kind": "trigger", "name": "go" })],
            condition: None,
            actions: vec![json!({ "kind": "summarize" })],
            spec: None,
            grant_id: None,
        };
        let state = MemoryState::default();
        let engine = AutomationEngine::new(&state, &EchoAction, &FailCodeRunner);

        let run = engine
            .execute(workspace_id, &automation, Some(json!({ "value": 42 })))
            .await
            .unwrap();

        assert_eq!(run.status, RunStatus::Succeeded);
        let steps = state.list_steps(workspace_id, run.id).await.unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, StepStatus::Succeeded);
        assert_eq!(steps[0].output.as_ref().unwrap()["trigger"]["value"], 42);
    }
}
