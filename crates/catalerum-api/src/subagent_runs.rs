//! Pod-local lifecycle management for background subagent tool runs.
//!
//! A run is owned by the workspace + principal that started it. The task keeps
//! running after its launching tool call returns, while the parent can inspect,
//! wait for, or cooperatively stop it through the tools registered here.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use catalerum_core::error::{Error, Result};
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry};
use catalerum_core::{AgentId, ConversationId, GrantId, UiDefinitionId, UserId, WorkspaceId};
use chrono::{DateTime, Utc};
use futures::FutureExt;
use serde_json::{json, Value as Json};
use tokio::sync::{watch, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const DEFAULT_MAX_CONCURRENT: usize = 8;
const DEFAULT_MAX_RETAINED: usize = 256;
const DEFAULT_WAIT_SECONDS: u64 = 30;
const MAX_WAIT_SECONDS: u64 = 600;
pub(crate) const SUBAGENT_CONTROL_TOOLS: [&str; 3] =
    ["monitor_subagent", "wait_subagent", "stop_subagent"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RunStatus {
    Queued,
    Running,
    Stopping,
    Completed,
    Failed,
    Stopped,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }

    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Stopped)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunOwner {
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    agent_id: Option<AgentId>,
    grant_id: Option<GrantId>,
    conversation_id: Option<ConversationId>,
    ui_id: Option<UiDefinitionId>,
}

impl RunOwner {
    fn from_context(ctx: &ToolContext) -> Result<Self> {
        let workspace_id = ctx
            .workspace_id
            .ok_or_else(|| Error::unauthorized("background subagent run requires a workspace"))?;
        Ok(Self {
            workspace_id,
            user_id: ctx.user_id,
            agent_id: ctx.agent_id,
            grant_id: ctx.grant_id,
            conversation_id: ctx.conversation_id,
            ui_id: ctx.ui_id,
        })
    }
}

#[derive(Clone, Debug)]
struct RunSnapshot {
    status: RunStatus,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    result: Option<Json>,
    error: Option<String>,
}

impl RunSnapshot {
    fn queued() -> Self {
        Self {
            status: RunStatus::Queued,
            started_at: None,
            finished_at: None,
            result: None,
            error: None,
        }
    }
}

struct RunEntry {
    id: Uuid,
    owner: RunOwner,
    kind: String,
    label: String,
    created_at: DateTime<Utc>,
    cancel: CancellationToken,
    state: watch::Sender<RunSnapshot>,
}

impl RunEntry {
    fn json(&self, include_result: bool) -> Json {
        let snapshot = self.state.borrow().clone();
        json!({
            "run_id": self.id,
            "kind": self.kind,
            "label": self.label,
            "status": snapshot.status.as_str(),
            "created_at": self.created_at,
            "started_at": snapshot.started_at,
            "finished_at": snapshot.finished_at,
            "result": include_result.then_some(snapshot.result).flatten(),
            "error": include_result.then_some(snapshot.error).flatten(),
        })
    }

    fn update_status(&self, status: RunStatus, started: bool) {
        let mut snapshot = self.state.borrow().clone();
        if (status == RunStatus::Running && snapshot.status != RunStatus::Queued)
            || (status == RunStatus::Stopping && snapshot.status.terminal())
        {
            return;
        }
        snapshot.status = status;
        if started && snapshot.started_at.is_none() {
            snapshot.started_at = Some(Utc::now());
        }
        self.state.send_replace(snapshot);
    }

    fn request_stop(&self) -> bool {
        let active = !self.state.borrow().status.terminal();
        if active {
            // Signal first, then publish `stopping`. If the worker wins the race
            // and publishes a terminal state, update_status will preserve it.
            self.cancel.cancel();
            self.update_status(RunStatus::Stopping, false);
        }
        active
    }

    fn finish(&self, status: RunStatus, result: Option<Json>, error: Option<String>) {
        let mut snapshot = self.state.borrow().clone();
        snapshot.status = status;
        snapshot.finished_at = Some(Utc::now());
        snapshot.result = result;
        snapshot.error = error;
        self.state.send_replace(snapshot);
    }
}

/// Shared, bounded registry of background subagent runs on this API pod.
#[derive(Clone)]
pub(crate) struct SubagentRunManager {
    runs: Arc<RwLock<HashMap<Uuid, Arc<RunEntry>>>>,
    permits: Arc<Semaphore>,
    max_retained: usize,
}

impl Default for SubagentRunManager {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_RETAINED)
    }
}

impl SubagentRunManager {
    #[must_use]
    pub(crate) fn new(max_concurrent: usize, max_retained: usize) -> Self {
        Self {
            runs: Arc::new(RwLock::new(HashMap::new())),
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
            max_retained: max_retained.max(1),
        }
    }

    pub(crate) async fn spawn<F, Fut>(
        &self,
        ctx: &ToolContext,
        kind: impl Into<String>,
        label: impl Into<String>,
        work: F,
    ) -> Result<Json>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Json>> + Send + 'static,
    {
        let owner = RunOwner::from_context(ctx)?;
        let id = Uuid::new_v4();
        let (state, _) = watch::channel(RunSnapshot::queued());
        let entry = Arc::new(RunEntry {
            id,
            owner,
            kind: kind.into(),
            label: label.into(),
            created_at: Utc::now(),
            cancel: CancellationToken::new(),
            state,
        });

        {
            let mut runs = self.runs.write().await;
            if runs.len() >= self.max_retained {
                let mut completed = runs
                    .values()
                    .filter(|run| run.state.borrow().status.terminal())
                    .map(|run| (run.created_at, run.id))
                    .collect::<Vec<_>>();
                completed.sort_unstable_by_key(|(created_at, _)| *created_at);
                let remove_count = runs.len() + 1 - self.max_retained;
                for (_, completed_id) in completed.into_iter().take(remove_count) {
                    runs.remove(&completed_id);
                }
            }
            if runs.len() >= self.max_retained {
                return Err(Error::other(format!(
                    "background subagent run limit ({}) reached; stop or wait for an active run",
                    self.max_retained
                )));
            }
            runs.insert(id, entry.clone());
        }

        let permits = self.permits.clone();
        let task_entry = entry.clone();
        tokio::spawn(async move {
            let permit = tokio::select! {
                () = task_entry.cancel.cancelled() => {
                    task_entry.finish(RunStatus::Stopped, None, None);
                    return;
                }
                permit = permits.acquire_owned() => permit,
            };
            let Ok(_permit) = permit else {
                task_entry.finish(
                    RunStatus::Failed,
                    None,
                    Some("background subagent executor shut down".into()),
                );
                return;
            };
            if task_entry.cancel.is_cancelled() {
                task_entry.finish(RunStatus::Stopped, None, None);
                return;
            }
            task_entry.update_status(RunStatus::Running, true);
            let result = AssertUnwindSafe(work(task_entry.cancel.clone()))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(value)) if task_entry.cancel.is_cancelled() => {
                    task_entry.finish(RunStatus::Stopped, Some(value), None);
                }
                Ok(Ok(value)) => task_entry.finish(RunStatus::Completed, Some(value), None),
                Ok(Err(error)) if task_entry.cancel.is_cancelled() => {
                    task_entry.finish(RunStatus::Stopped, None, Some(error.to_string()));
                }
                Ok(Err(error)) => {
                    task_entry.finish(RunStatus::Failed, None, Some(error.to_string()));
                }
                Err(payload) => {
                    let message = payload
                        .downcast_ref::<&str>()
                        .map(|value| (*value).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    task_entry.finish(
                        RunStatus::Failed,
                        None,
                        Some(format!("background subagent panicked: {message}")),
                    );
                }
            }
        });

        Ok(json!({
            "background": true,
            "run_id": id,
            "status": "queued",
            "kind": entry.kind,
            "label": entry.label,
            "control_tools": SUBAGENT_CONTROL_TOOLS,
        }))
    }

    async fn get(&self, ctx: &ToolContext, id: Uuid) -> Result<Arc<RunEntry>> {
        let owner = RunOwner::from_context(ctx)?;
        let run = self.runs.read().await.get(&id).cloned();
        match run {
            Some(run) if run.owner == owner => Ok(run),
            _ => Err(Error::NotFound),
        }
    }

    async fn list(&self, ctx: &ToolContext, kind: Option<&str>) -> Result<Vec<Json>> {
        let owner = RunOwner::from_context(ctx)?;
        let runs = self.runs.read().await;
        let mut matching = runs
            .values()
            .filter(|run| run.owner == owner && kind.is_none_or(|kind| run.kind == kind))
            .map(|run| (run.created_at, run.json(false)))
            .collect::<Vec<_>>();
        matching.sort_unstable_by_key(|(created_at, _)| std::cmp::Reverse(*created_at));
        Ok(matching.into_iter().map(|(_, value)| value).collect())
    }
}

fn run_id(args: &Json) -> Result<Uuid> {
    args.get("run_id")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::invalid("`run_id` is required"))?
        .parse()
        .map_err(|error| Error::invalid(format!("invalid `run_id`: {error}")))
}

struct MonitorSubagentTool {
    manager: SubagentRunManager,
}

#[async_trait]
impl Tool for MonitorSubagentTool {
    fn name(&self) -> &str {
        "monitor_subagent"
    }

    fn description(&self) -> &str {
        "Inspect one background subagent run, or list all runs created by this same workspace, principal, grant, and conversation. Completed results are returned only when a run_id is supplied."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string", "description": "Optional background run id. Omit to list owned runs." },
                "kind": { "type": "string", "enum": ["delegate", "computer_subagent", "terminal_subagent"], "description": "Optional list filter; ignored when run_id is supplied." }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        if args.get("run_id").is_some() {
            return Ok(self.manager.get(ctx, run_id(&args)?).await?.json(true));
        }
        let kind = args.get("kind").and_then(Json::as_str);
        let runs = self.manager.list(ctx, kind).await?;
        Ok(json!({ "runs": runs, "count": runs.len() }))
    }
}

struct WaitSubagentTool {
    manager: SubagentRunManager,
}

#[async_trait]
impl Tool for WaitSubagentTool {
    fn name(&self) -> &str {
        "wait_subagent"
    }

    fn description(&self) -> &str {
        "Wait a bounded time for an owned background subagent run to finish. A timeout is not an error: the current state is returned with timed_out=true."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "run_id": { "type": "string" },
                "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECONDS, "default": DEFAULT_WAIT_SECONDS }
            },
            "required": ["run_id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let run = self.manager.get(ctx, run_id(&args)?).await?;
        let wait_seconds = args
            .get("wait_seconds")
            .and_then(Json::as_u64)
            .unwrap_or(DEFAULT_WAIT_SECONDS)
            .min(MAX_WAIT_SECONDS);
        let mut state = run.state.subscribe();
        let timed_out = if state.borrow().status.terminal() {
            false
        } else {
            tokio::time::timeout(Duration::from_secs(wait_seconds), async {
                loop {
                    if state.changed().await.is_err() || state.borrow().status.terminal() {
                        break;
                    }
                }
            })
            .await
            .is_err()
        };
        let mut value = run.json(true);
        value["timed_out"] = json!(timed_out);
        Ok(value)
    }
}

struct StopSubagentTool {
    manager: SubagentRunManager,
}

#[async_trait]
impl Tool for StopSubagentTool {
    fn name(&self) -> &str {
        "stop_subagent"
    }

    fn description(&self) -> &str {
        "Cooperatively stop an owned queued or running background subagent. Use wait_subagent afterwards to observe terminal stopped state."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "run_id": { "type": "string" } },
            "required": ["run_id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let run = self.manager.get(ctx, run_id(&args)?).await?;
        let stop_requested = run.request_stop();
        let status = run.state.borrow().status;
        Ok(json!({
            "run_id": run.id,
            "stop_requested": stop_requested,
            "status": status.as_str(),
        }))
    }
}

pub(crate) fn register_subagent_run_tools(
    registry: &mut ToolRegistry,
    manager: SubagentRunManager,
) {
    registry.register(Arc::new(MonitorSubagentTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(WaitSubagentTool {
        manager: manager.clone(),
    }));
    registry.register(Arc::new(StopSubagentTool { manager }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ToolContext {
        ToolContext {
            workspace_id: Some(WorkspaceId::new()),
            user_id: Some(UserId::new()),
            ..ToolContext::default()
        }
    }

    #[tokio::test]
    async fn background_run_can_be_waited_for() {
        let manager = SubagentRunManager::new(1, 4);
        let ctx = context();
        let started = manager
            .spawn(&ctx, "computer_subagent", "test", |_| async {
                Ok(json!({ "answer": 42 }))
            })
            .await
            .unwrap();
        let id = started["run_id"].as_str().unwrap().parse().unwrap();
        let run = manager.get(&ctx, id).await.unwrap();
        let mut state = run.state.subscribe();
        while !state.borrow().status.terminal() {
            state.changed().await.unwrap();
        }
        assert_eq!(state.borrow().status, RunStatus::Completed);
        assert_eq!(run.json(true)["result"]["answer"], 42);
    }

    #[tokio::test]
    async fn stop_cancels_a_running_run() {
        let manager = SubagentRunManager::new(1, 4);
        let ctx = context();
        let started = manager
            .spawn(&ctx, "terminal_subagent", "test", |cancel| async move {
                cancel.cancelled().await;
                Ok(json!({ "partial": true }))
            })
            .await
            .unwrap();
        let id = started["run_id"].as_str().unwrap().parse().unwrap();
        let run = manager.get(&ctx, id).await.unwrap();
        run.cancel.cancel();
        let mut state = run.state.subscribe();
        while !state.borrow().status.terminal() {
            state.changed().await.unwrap();
        }
        assert_eq!(state.borrow().status, RunStatus::Stopped);
    }

    #[tokio::test]
    async fn another_principal_cannot_observe_a_run() {
        let manager = SubagentRunManager::new(1, 4);
        let mut ctx = context();
        ctx.grant_id = Some(GrantId::new());
        ctx.conversation_id = Some(ConversationId::new());
        let started = manager
            .spawn(&ctx, "computer_subagent", "test", |_| async {
                Ok(Json::Null)
            })
            .await
            .unwrap();
        let id = started["run_id"].as_str().unwrap().parse().unwrap();
        let mut other = ctx.clone();
        other.user_id = Some(UserId::new());
        assert!(matches!(
            manager.get(&other, id).await,
            Err(Error::NotFound)
        ));
        let mut other_grant = ctx.clone();
        other_grant.grant_id = Some(GrantId::new());
        assert!(matches!(
            manager.get(&other_grant, id).await,
            Err(Error::NotFound)
        ));
        let mut other_conversation = ctx.clone();
        other_conversation.conversation_id = Some(ConversationId::new());
        assert!(matches!(
            manager.get(&other_conversation, id).await,
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn stop_request_never_overwrites_a_terminal_state() {
        let (state, _) = watch::channel(RunSnapshot::queued());
        let run = RunEntry {
            id: Uuid::new_v4(),
            owner: RunOwner::from_context(&context()).unwrap(),
            kind: "computer_subagent".into(),
            label: "test".into(),
            created_at: Utc::now(),
            cancel: CancellationToken::new(),
            state,
        };
        run.finish(RunStatus::Completed, Some(json!({ "done": true })), None);
        assert!(!run.request_stop());
        assert_eq!(run.state.borrow().status, RunStatus::Completed);
    }
}
