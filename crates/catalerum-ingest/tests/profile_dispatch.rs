//! Integration test: the channel→agent-profile bridge fails closed on an archived
//! workspace (SOUL §18), mirroring the automation path's `dispatch_trigger_event` /
//! `run_automation` archived guards (`automation_run.rs`). Without these, a
//! workspace archived after boot still runs agent-profile LLM loops + replies on
//! inbound channel messages (the boot-spawned `ChannelListener` never tears down on
//! archive; a pre-archival bearer can still POST `/channels/{name}/messages`).
//!
//! DB-gated like the other ingest tests: set `CATALERUM_TEST_DATABASE_URL` (or
//! `DATABASE_URL`) to run it; otherwise it skips and passes offline.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use catalerum_automation::{Action, ActionOutcome, ActionRunner};
use catalerum_core::WorkspaceId;
use catalerum_ingest::{dispatch_channel_to_profiles, run_profile_job, RunProfilePayload};
use catalerum_store::NewAgentProfile;
use serde_json::Value;

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// A runner that counts how many actions it was asked to run — enough to prove
/// whether `run_profile_job` drove the profile or skipped it.
#[derive(Clone, Default)]
struct CountingRunner {
    runs: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ActionRunner for CountingRunner {
    async fn run(
        &self,
        _workspace_id: WorkspaceId,
        _action: &Action,
        _trigger: Option<&Value>,
        _grant: Option<&catalerum_core::model::Grant>,
    ) -> ActionOutcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        ActionOutcome::succeeded(None)
    }
}

fn profile_on_channel(name: &str, channel: &str) -> NewAgentProfile {
    NewAgentProfile {
        name: name.to_string(),
        model: None,
        system_prompt: None,
        tools: vec![],
        skills: vec![],
        subagents: vec![],
        channels: vec![channel.to_string()],
        grant_id: None,
        guard: None,
    }
}

/// Fail-closed dispatch (SOUL §18): an inbound channel message at an **archived**
/// workspace dispatches to no profile — `dispatch_channel_to_profiles` enqueues no
/// `run_profile` job even though a profile listens on the channel. Mirrors
/// `dispatch_trigger_event_at_archived_workspace_matches_nothing`.
#[tokio::test]
async fn dispatch_channel_to_profiles_at_archived_workspace_matches_nothing() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping archived-channel-dispatch test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("archchan", &format!("archchan-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // A profile listening on the channel we'll message.
    store
        .agent_profiles()
        .create(ws.id, &profile_on_channel("bot", "discord"))
        .await
        .expect("profile");

    // While live, an inbound message dispatches to the listening profile (baseline).
    let live = dispatch_channel_to_profiles(&store, ws.id, "discord", "hi")
        .await
        .expect("dispatch");
    assert_eq!(live.len(), 1, "the listening profile matches while live");

    // Archive the workspace, then re-dispatch: the bridge fails closed — no jobs.
    store.workspaces().archive(ws.id).await.expect("archive");
    let archived = dispatch_channel_to_profiles(&store, ws.id, "discord", "hi")
        .await
        .expect("dispatch after archive");
    assert!(
        archived.is_empty(),
        "a channel message at an archived workspace dispatches to nothing"
    );
}

/// Fail-closed at claim time (SOUL §18): a `run_profile` job whose workspace was
/// archived after it was enqueued is **skipped** — `run_profile_job` never drives
/// the profile through its runner (the counter stays put) yet completes cleanly.
/// Mirrors `queued_run_at_archived_workspace_is_skipped`.
#[tokio::test]
async fn run_profile_job_at_archived_workspace_is_skipped() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping archived-profile-run test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = common::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("archprof", &format!("archprof-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");

    let runner = CountingRunner::default();
    let payload = RunProfilePayload {
        workspace_id: Some(ws.id),
        profile: "bot".into(),
        input: Some("hi".into()),
        reply_channel: Some("discord".into()),
    };

    // Live: the runner is invoked (the baseline the guard changes).
    run_profile_job(&store, &runner, ws.id, payload.clone())
        .await
        .expect("live run");
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        1,
        "the profile ran while the workspace was live"
    );

    // Archive, then re-run the SAME job: skipped — the runner is not invoked again,
    // and the job still completes (`Ok`), exactly like a skipped automation run.
    store.workspaces().archive(ws.id).await.expect("archive");
    run_profile_job(&store, &runner, ws.id, payload)
        .await
        .expect("skipped run still completes");
    assert_eq!(
        runner.runs.load(Ordering::SeqCst),
        1,
        "no profile run is driven for a job claimed after the workspace was archived"
    );
}
