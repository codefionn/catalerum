//! Durable automation-run dispatch (SOUL §11/§6.2).
//!
//! [`enqueue_run_automation`] writes a durable [`JOB_KIND_RUN_AUTOMATION`] job
//! ([`RunAutomationPayload`]) — the way an event source (a matched trigger) hands
//! an automation off to run out-of-band, decoupled from the request that fired
//! it. A worker holding an [`AutomationContext`] claims the job, loads the
//! automation, and runs it via [`catalerum_automation::execute`] under the
//! context's [`ActionRunner`] (its §19 authority), recording durable run/step
//! state. A disabled or vanished automation is skipped (a pause/delete takes
//! effect even for an already-queued job), so neither becomes a stuck retry.
//!
//! **Not idempotent under at-least-once delivery.** Unlike the idempotent
//! `sync_calendar`/`ingest_note`/`project_note`/`extract_memories` kinds, a
//! `run_automation` job re-driven by the §6.2 reconciler (a worker that crashed
//! mid-run) starts a *fresh* run — [`execute`] does not resume — so already-run
//! non-idempotent actions (a created note/task, a sent notification) execute
//! again. The §11 **Valkey single-fire lock** + run resumption that close this
//! gap are a deferred slice; until then a crash mid-run can duplicate side
//! effects, not merely lose throughput.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

use catalerum_automation::{
    execute_for_job, matching_automations, ActionRunner, CodeRunner, FailCodeRunner, TriggerEvent,
};
use catalerum_core::id::{AutomationId, AutomationRunId, WorkspaceId};
use catalerum_store::{Store, StoreError};

use crate::error::Result;

/// The `job_queue.kind` token for an automation-run job (SOUL §11/§6.2). Enqueue
/// with this kind (via [`enqueue_run_automation`]); a worker holding an
/// [`AutomationContext`] runs it.
pub const JOB_KIND_RUN_AUTOMATION: &str = "run_automation";

/// The JSON payload of a [`JOB_KIND_RUN_AUTOMATION`] job: which automation to run
/// and the trigger that fired it. `workspace_id` is **optional** on the wire (the
/// worker falls back to the job row's `workspace_id` column), matching the other
/// job kinds' producer/consumer contract.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunAutomationPayload {
    /// The workspace that owns the automation. Optional: resolved from the job
    /// row when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The automation to run.
    pub automation_id: AutomationId,
    /// The event/trigger that fired the run (recorded on the run), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<Value>,
}

impl RunAutomationPayload {
    /// Build a payload carrying an explicit workspace scope.
    #[must_use]
    pub fn new(
        workspace_id: WorkspaceId,
        automation_id: AutomationId,
        trigger: Option<Value>,
    ) -> Self {
        Self {
            workspace_id: Some(workspace_id),
            automation_id,
            trigger,
        }
    }
}

/// The services a worker needs to run a [`JOB_KIND_RUN_AUTOMATION`] job: the
/// [`ActionRunner`] an automation's actions dispatch through (its §19 authority),
/// and the [`CodeRunner`] a node-graph's inline **Code / Condition** nodes run on
/// (SOUL §11 Phase B). Bundled like [`crate::EmbedContext`]; the binary injects a
/// concrete runner (e.g. a tool-backed, grant-scoped one) and a real
/// `CodeRunner` (`catalerum_script::ScriptCodeRunner`).
///
/// `code` defaults to [`FailCodeRunner`] — the Phase-A behaviour, where a Code /
/// Condition node fails with a clear message — so an existing construction (and a
/// graph without code nodes) is unaffected until a real runner is installed via
/// [`with_code_runner`](Self::with_code_runner).
#[derive(Clone)]
pub struct AutomationContext {
    runner: Arc<dyn ActionRunner>,
    code: Arc<dyn CodeRunner>,
}

impl AutomationContext {
    /// Bundle an [`ActionRunner`] for the worker to run automations through. The
    /// inline-code runtime defaults to [`FailCodeRunner`] (Phase-A: a Code /
    /// Condition node fails); install a real one with
    /// [`with_code_runner`](Self::with_code_runner).
    #[must_use]
    pub fn new(runner: Arc<dyn ActionRunner>) -> Self {
        Self {
            runner,
            code: Arc::new(FailCodeRunner),
        }
    }

    /// Install the [`CodeRunner`] a node-graph's inline Code / Condition nodes run
    /// on (SOUL §11 Phase B) — e.g. `catalerum_script::ScriptCodeRunner`. Without
    /// this, those nodes fail under the default [`FailCodeRunner`] (builder).
    #[must_use]
    pub fn with_code_runner(mut self, code: Arc<dyn CodeRunner>) -> Self {
        self.code = code;
        self
    }

    /// Borrow the runner.
    #[must_use]
    pub fn runner(&self) -> &Arc<dyn ActionRunner> {
        &self.runner
    }

    /// Borrow the inline-code runner (SOUL §11 Phase B).
    #[must_use]
    pub fn code(&self) -> &Arc<dyn CodeRunner> {
        &self.code
    }
}

impl std::fmt::Debug for AutomationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomationContext").finish_non_exhaustive()
    }
}

/// Enqueue a durable [`JOB_KIND_RUN_AUTOMATION`] job for `automation_id` (SOUL
/// §11/§6.2) — how a matched trigger hands an automation off to run. The worker
/// claims it on its next poll. Returns the enqueued job's id.
pub async fn enqueue_run_automation(
    store: &Store,
    workspace_id: WorkspaceId,
    automation_id: AutomationId,
    trigger: Option<Value>,
) -> Result<uuid::Uuid> {
    let payload = RunAutomationPayload::new(workspace_id, automation_id, trigger);
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_RUN_AUTOMATION,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %automation_id, "enqueued run_automation job");
    Ok(job.id)
}

/// Run an enqueued automation (SOUL §11): load it (workspace-scoped) and, unless
/// disabled, drive its actions via `runner`, recording durable run/step state.
/// A disabled or already-deleted automation is **skipped** (`Ok(None)`) so a
/// pause/delete settles a queued job rather than failing it forever.
///
/// `job_id` is the driving `run_automation` job. It's recorded on the run so a
/// re-drive of the **same** job (a worker that crashed mid-run, re-leased by the
/// §6.2 reconciler) **resumes** that run rather than starting a fresh one and
/// re-executing completed actions (SOUL §5; see [`execute_for_job`]).
pub async fn run_automation(
    store: &Store,
    runner: &dyn ActionRunner,
    code: &dyn CodeRunner,
    workspace_id: WorkspaceId,
    automation_id: AutomationId,
    trigger: Option<Value>,
    job_id: uuid::Uuid,
) -> Result<Option<AutomationRunId>> {
    // Fail closed at claim time (SOUL §18): if the workspace was archived after
    // this job was enqueued — the dispatch → durable-job → worker window, which
    // exists because archiving only stamps `archived_at` and does not drain
    // already-queued jobs — skip the run rather than execute an automation in an
    // archived workspace. Settles the queued job like a paused/deleted automation
    // (`Ok(None)`), so it completes without a stuck retry.
    if let Ok(ws) = store.workspaces().get(workspace_id).await {
        if ws.archived_at.is_some() {
            return Ok(None);
        }
    }
    let automation = match store.automations().get(workspace_id, automation_id).await {
        Ok(a) => a,
        // The automation was deleted after the job was enqueued — nothing to run.
        Err(StoreError::NotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !automation.enabled {
        return Ok(None);
    }
    let run = execute_for_job(
        store,
        runner,
        code,
        workspace_id,
        &automation,
        trigger,
        Some(job_id),
    )
    .await?;
    Ok(Some(run.id))
}

/// Dispatch a real-world [`TriggerEvent`] (SOUL §11): enqueue a durable
/// `run_automation` job for **every enabled automation in `workspace_id` whose
/// trigger matches** the event. Returns the enqueued job ids. The event is
/// recorded as each run's trigger payload. This is the bridge an event source (a
/// Kanban move §24, a webhook) calls — match here, run later on the worker.
///
/// Best-effort by convention at the call site: a caller fires-and-logs so a
/// failed enqueue never fails the originating action.
pub async fn dispatch_trigger_event(
    store: &Store,
    workspace_id: WorkspaceId,
    event: &TriggerEvent,
) -> Result<Vec<uuid::Uuid>> {
    // Fail closed on an **archived** workspace (SOUL §18): a fire targeting one
    // matches nothing — no `run_automation` job is enqueued, so a webhook /
    // public-trigger / trigger-link / Kanban / storage / channel fire at an
    // archived workspace is inert. This is the shared-bridge chokepoint every
    // by-id event source funnels through, so one check fail-closes all of them.
    // `get` returns archived rows by design (restore/admin resolve them), so we
    // test the flag explicitly; a vanished workspace falls through and still
    // matches nothing via the empty listing below.
    if let Ok(ws) = store.workspaces().get(workspace_id).await {
        if ws.archived_at.is_some() {
            return Ok(Vec::new());
        }
    }
    let automations = store.automations().list_by_workspace(workspace_id).await?;
    let matched = matching_automations(&automations, event);
    if matched.is_empty() {
        return Ok(Vec::new());
    }
    let trigger = serde_json::to_value(event)?;
    let mut jobs = Vec::with_capacity(matched.len());
    for automation in matched {
        jobs.push(
            enqueue_run_automation(store, workspace_id, automation.id, Some(trigger.clone()))
                .await?,
        );
    }
    Ok(jobs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_RUN_AUTOMATION, "run_automation");
    }

    #[test]
    fn payload_round_trips_and_accepts_minimal_shape() {
        let ws = WorkspaceId::new();
        let id = AutomationId::new();
        let p = RunAutomationPayload::new(ws, id, Some(serde_json::json!({ "kind": "webhook" })));
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("workspace_id").is_some());
        let back: RunAutomationPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.automation_id, id);
        assert_eq!(back.workspace_id, Some(ws));

        // The worker must accept a minimal `{ "automation_id": "…" }` shape (scope
        // resolved from the job row), not reject it for a missing field.
        let minimal: RunAutomationPayload =
            serde_json::from_value(serde_json::json!({ "automation_id": id })).unwrap();
        assert_eq!(minimal.workspace_id, None);
        assert!(minimal.trigger.is_none());
    }
}
