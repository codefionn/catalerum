//! Inbound channel messages (SOUL §25/§11) — the *receive* half of channels.
//!
//! `POST /channels/{channel}/inbound` accepts a message that arrived on `channel`
//! (relayed by a bot / a provider webhook) and dispatches a
//! `TriggerEvent::ChannelMessage { channel, text }`: every enabled automation whose
//! `{ "kind": "channel_message", "channel": "…" }` trigger matches gets a durable
//! `run_automation` job (the same `dispatch_trigger_event` bridge the webhook §25
//! and Kanban §24 sources use). The message `text` rides on the recorded trigger,
//! so an `LlmAgent` action **sees what was said** (§11) — which closes the
//! chat-from-messenger loop: inbound message → trigger → agent → `notify` a reply.
//!
//! **Authenticated + workspace-scoped (SOUL §18/§19):** reachable only by a
//! principal (a relay's workspace-bound service token), scoped to its workspace,
//! gated on `channel:write` (a Viewer can't inject messages) — same auth as every
//! surface, no backdoor (principle 15).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_automation::TriggerEvent;
use catalerum_core::capability::Action;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the inbound channel-message route.
pub fn router() -> Router<AppState> {
    Router::new().route("/channels/{channel}/inbound", post(inbound))
}

/// Body for `POST /channels/{channel}/inbound`.
#[derive(Debug, Deserialize)]
pub struct InboundMessage {
    /// The message text that arrived on the channel.
    pub text: String,
    /// Optional provider-native id of who sent it — carried on the trigger so an
    /// agent knows which participant spoke (multiplayer, SOUL §25). Not matched on.
    #[serde(default)]
    pub sender: Option<String>,
}

/// The result of an inbound message: how many automations matched + the enqueued
/// `run_automation` jobs, plus how many **agent profiles** listening on the
/// channel were dispatched (SOUL §19/§25).
#[derive(Debug, Serialize)]
pub struct InboundResult {
    pub matched: usize,
    pub jobs: Vec<uuid::Uuid>,
    /// Number of agent profiles listening on this channel that were run.
    #[serde(default)]
    pub profiles: usize,
}

async fn inbound(
    State(state): State<AppState>,
    auth: Auth,
    Path(channel): Path<String>,
    Json(body): Json<InboundMessage>,
) -> ApiResult<(StatusCode, Json<InboundResult>)> {
    let p = auth.principal();
    auth.require(Action::Write, "channel")?;
    let channel = channel.trim();
    if channel.is_empty() {
        return Err(ApiError::bad_request("channel name must not be empty"));
    }
    // Route the message to any agent profiles listening on this channel (SOUL
    // §19/§25): enqueue a **durable** `run_profile` job per listening profile — each
    // runs its own scoped §7 loop on the worker and replies back on the channel.
    // Independent of the automation path; survives a pod loss.
    let profiles = catalerum_ingest::dispatch_channel_to_profiles(
        state.store(),
        p.workspace_id,
        channel,
        &body.text,
    )
    .await
    .map_err(|e| ApiError::internal(format!("dispatching channel-message profiles: {e}")))?
    .len();
    let event = TriggerEvent::ChannelMessage {
        channel: channel.to_string(),
        text: Some(body.text),
        sender: body.sender,
    };
    let jobs = catalerum_ingest::dispatch_trigger_event(state.store(), p.workspace_id, &event)
        .await
        .map_err(|e| ApiError::internal(format!("dispatching channel-message automations: {e}")))?;
    Ok((
        StatusCode::ACCEPTED,
        Json(InboundResult {
            matched: jobs.len(),
            jobs,
            profiles,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_body_requires_text() {
        let ok: InboundMessage = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert_eq!(ok.text, "hi");
        assert!(serde_json::from_str::<InboundMessage>(r#"{}"#).is_err());
    }
}
