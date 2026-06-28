//! Durable agent-profile-run dispatch (SOUL §19/§25/§6.2).
//!
//! [`enqueue_run_profile`] writes a durable [`JOB_KIND_RUN_PROFILE`] job
//! ([`RunProfilePayload`]); [`dispatch_channel_to_profiles`] enqueues one per
//! [`AgentProfile`](catalerum_core::model::AgentProfile) *listening* on a channel —
//! the channel→profile bridge (SOUL §25), made **durable** (it survives a pod loss
//! mid-dispatch, unlike a fire-and-forget spawn). A worker holding an
//! [`AutomationContext`](crate::AutomationContext) claims the job and runs the
//! profile through the context's [`ActionRunner`] as a `RunProfile` action — so the
//! profile loop runs with the runner's §19 authority (the profile's own grant; the
//! channel path applies no further ceiling).
//!
//! **At-least-once, no retry on a logical failure.** Like `run_automation`, a job
//! re-driven by the §6.2 reconciler (a worker that crashed mid-run) starts the
//! profile **fresh** — non-idempotent side effects (a posted reply) can repeat. A
//! profile run that *fails* (model error, refused grant) is logged and the job
//! **completes** (no retry storm of expensive model calls); a crash *before*
//! completion is what the reclaim re-drives.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing::{debug, info, warn};

use catalerum_automation::{Action, ActionRunner};
use catalerum_core::id::WorkspaceId;
use catalerum_core::model::StepStatus;
use catalerum_store::Store;

use crate::error::Result;

/// The `job_queue.kind` token for an agent-profile-run job (SOUL §19/§25). Enqueue
/// with this kind (via [`enqueue_run_profile`]); a worker holding an
/// [`AutomationContext`](crate::AutomationContext) runs it.
pub const JOB_KIND_RUN_PROFILE: &str = "run_profile";

/// The JSON payload of a [`JOB_KIND_RUN_PROFILE`] job: which profile to run, the
/// optional input (the user turn), and an optional channel to deliver the reply on.
/// `workspace_id` is optional on the wire (the worker falls back to the job row).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunProfilePayload {
    /// The workspace that owns the profile. Optional: resolved from the job row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
    /// The agent profile to run (by name, workspace-scoped).
    pub profile: String,
    /// The user turn the profile runs on (e.g. an inbound channel message).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Deliver the profile's reply on this channel (the inbound channel that fired
    /// it). `None` runs the profile without a channel reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_channel: Option<String>,
}

/// Enqueue a durable [`JOB_KIND_RUN_PROFILE`] job (SOUL §19/§25). Returns the job id.
pub async fn enqueue_run_profile(
    store: &Store,
    workspace_id: WorkspaceId,
    profile: &str,
    input: Option<String>,
    reply_channel: Option<String>,
) -> Result<uuid::Uuid> {
    let payload = RunProfilePayload {
        workspace_id: Some(workspace_id),
        profile: profile.to_string(),
        input,
        reply_channel,
    };
    let job = store
        .job_queue()
        .enqueue(
            Some(workspace_id),
            JOB_KIND_RUN_PROFILE,
            serde_json::to_value(payload)?,
            None,
        )
        .await?;
    debug!(job = %job.id, %profile, "enqueued run_profile job");
    Ok(job.id)
}

/// Enqueue a durable `run_profile` job for **every profile listening on `channel`**
/// in `workspace_id` (SOUL §25): the channel→profile bridge. Each profile runs its
/// own scoped loop on the worker and replies back on the channel. Returns the
/// enqueued job ids (empty if no profile listens there).
pub async fn dispatch_channel_to_profiles(
    store: &Store,
    workspace_id: WorkspaceId,
    channel: &str,
    text: &str,
) -> Result<Vec<uuid::Uuid>> {
    // Fail closed on an **archived** workspace (SOUL §18), mirroring the automation
    // path's `dispatch_trigger_event`: an inbound channel message to an archived
    // workspace dispatches to nothing — no `run_profile` job is enqueued — so a
    // boot-spawned `ChannelListener` that never tore down on archive, or a
    // pre-archival bearer posting to `/channels/{name}/messages`, is inert. `get`
    // returns archived rows by design (restore/admin resolve them), so we test the
    // flag explicitly; a vanished workspace falls through to the empty listing.
    if let Ok(ws) = store.workspaces().get(workspace_id).await {
        if ws.archived_at.is_some() {
            debug!(%workspace_id, channel, "workspace archived; skipping channel→profile dispatch");
            return Ok(Vec::new());
        }
    }
    let profiles = store
        .agent_profiles()
        .list_by_channel(workspace_id, channel)
        .await?;
    let mut jobs = Vec::with_capacity(profiles.len());
    for profile in profiles {
        jobs.push(
            enqueue_run_profile(
                store,
                workspace_id,
                &profile.name,
                Some(text.to_string()),
                Some(channel.to_string()),
            )
            .await?,
        );
    }
    Ok(jobs)
}

/// Run an enqueued `run_profile` job (SOUL §19/§25): build a `RunProfile` action
/// (+ a `ChannelMessage` trigger when replying to a channel, so the reply routes
/// back and the user turn is the message) and drive it through `runner` (its §19
/// authority). A profile-run *failure* is logged, not propagated, so the job
/// completes rather than retrying expensive model calls.
pub async fn run_profile_job(
    store: &Store,
    runner: &dyn ActionRunner,
    workspace_id: WorkspaceId,
    payload: RunProfilePayload,
) -> Result<()> {
    // Fail closed at claim time (SOUL §18), mirroring `run_automation`: if the
    // workspace was archived after this job was enqueued — the dispatch → durable-job
    // → worker window, which archiving (a bare `archived_at` stamp) does not drain —
    // skip the run rather than drive an agent-profile LLM loop / channel reply in an
    // archived workspace. Completes the job (`Ok(())`) so it settles without a stuck
    // retry, exactly like a skipped automation run.
    if let Ok(ws) = store.workspaces().get(workspace_id).await {
        if ws.archived_at.is_some() {
            debug!(
                profile = %payload.profile,
                workspace = %workspace_id,
                "workspace archived; skipping run_profile job"
            );
            return Ok(());
        }
    }
    // Build the `RunProfile` action by value (flattened params) via serde.
    let mut obj = Map::new();
    obj.insert("kind".into(), Value::String("run_profile".into()));
    obj.insert("profile".into(), Value::String(payload.profile.clone()));
    if let Some(input) = &payload.input {
        obj.insert("input".into(), Value::String(input.clone()));
    }
    let action: Action = serde_json::from_value(Value::Object(obj))?;
    // When replying to a channel, present the run as a channel message so the reply
    // routes back to that channel and the profile responds to the actual text.
    let trigger = payload.reply_channel.as_ref().map(|channel| {
        json!({
            "kind": "channel_message",
            "channel": channel,
            "text": payload.input.clone().unwrap_or_default(),
        })
    });
    // No automation ceiling: the channel/direct path runs the profile under its own
    // grant (the profile *is* the authority). An automation `RunProfile` action,
    // by contrast, passes its grant as the ceiling (see `action_runner`).
    let outcome = runner
        .run(workspace_id, &action, trigger.as_ref(), None)
        .await;
    if outcome.status == StepStatus::Failed {
        warn!(
            profile = %payload.profile,
            workspace = %workspace_id,
            error = outcome.error.as_deref().unwrap_or("unknown"),
            "run_profile job: profile run failed (job completes, no retry)"
        );
    } else {
        info!(profile = %payload.profile, workspace = %workspace_id, "run_profile done");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_kind_token_is_stable() {
        assert_eq!(JOB_KIND_RUN_PROFILE, "run_profile");
    }

    #[test]
    fn payload_round_trips_and_accepts_minimal_shape() {
        let ws = WorkspaceId::new();
        let p = RunProfilePayload {
            workspace_id: Some(ws),
            profile: "calbot".into(),
            input: Some("hi".into()),
            reply_channel: Some("discord".into()),
        };
        let json = serde_json::to_value(&p).unwrap();
        let back: RunProfilePayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.profile, "calbot");
        assert_eq!(back.reply_channel.as_deref(), Some("discord"));

        // The worker must accept a minimal `{ "profile": "…" }` shape (scope from the
        // job row, no input/channel), not reject it for missing fields.
        let minimal: RunProfilePayload =
            serde_json::from_value(json!({ "profile": "calbot" })).unwrap();
        assert_eq!(minimal.workspace_id, None);
        assert!(minimal.input.is_none());
        assert!(minimal.reply_channel.is_none());
    }
}
