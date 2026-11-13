//! The `start_chat` automation action (SOUL §7/§11/§12): open a new chat thread
//! with a prompt as its first **user** message and let the ordinary chat turn
//! answer it.
//!
//! The action runner owns the parts it can do with the store (authority check,
//! param parsing, conversation + model/profile setup). Running the turn itself
//! needs the chat runtime (turn buffer, turn lock, active-turn registry), which
//! lives behind [`ChatTurnStarter`] so the runner stays testable without it.

use async_trait::async_trait;
use catalerum_automation::{Action, ActionOutcome};
use catalerum_core::capability::{Action as CapAction, Capability, Resource};
use catalerum_core::model::{Grant, Origin};
use catalerum_core::{ConversationId, GrantId, MessageId, UserId, WorkspaceId};
use serde_json::{json, Value};

use super::templating::render_params;
use super::{cap_allows, ToolActionRunner};

/// One chat turn to start headlessly, under an already-resolved authority.
pub(crate) struct ChatTurnRequest {
    pub(crate) workspace_id: WorkspaceId,
    /// The acting user (memories, profile, and LLM settings are theirs).
    pub(crate) user_id: UserId,
    /// The §19 grant the authority came from, if any (recorded on tool calls,
    /// and its `dry_run`/deletion still apply).
    pub(crate) grant_id: Option<GrantId>,
    /// The turn's authority. Replaces whatever the user's role would grant.
    pub(crate) capabilities: Vec<Capability>,
    pub(crate) conversation_id: ConversationId,
    pub(crate) prompt: String,
}

/// What a finished headless turn produced.
#[derive(Debug, Default)]
pub(crate) struct ChatTurnReport {
    pub(crate) message_id: Option<MessageId>,
    pub(crate) reply: String,
    pub(crate) stopped: bool,
    pub(crate) truncated: bool,
}

/// A started turn: its user message id is known up front; `done` resolves when
/// the turn ends. Dropping `done` leaves the turn running.
pub(crate) struct StartedChatTurn {
    pub(crate) user_message_id: MessageId,
    pub(crate) done: tokio::task::JoinHandle<Result<ChatTurnReport, String>>,
}

/// Runs a chat turn without a client socket. The production impl is
/// [`crate::routes::ws::AppChatStarter`].
#[async_trait]
pub(crate) trait ChatTurnStarter: Send + Sync {
    async fn start_turn(&self, request: ChatTurnRequest) -> Result<StartedChatTurn, String>;
}

/// The parsed, template-rendered `start_chat` params.
#[derive(Debug)]
struct StartChatParams {
    prompt: String,
    title: Option<String>,
    model: Option<String>,
    profile: Option<String>,
    wait: bool,
}

fn optional_string(
    params: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(v)) => {
            let v = v.trim();
            Ok((!v.is_empty()).then(|| v.to_string()))
        }
        Some(_) => Err(format!("start_chat: `{key}` must be a string")),
    }
}

fn parse_params(params: &serde_json::Map<String, Value>) -> Result<StartChatParams, String> {
    let prompt = optional_string(params, "prompt")?
        .ok_or_else(|| "start_chat: `prompt` is required and must not be empty".to_string())?;
    let wait = match params.get("wait") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        // A templated flag arrives as a string.
        Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("true") => true,
        Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("false") => false,
        Some(_) => return Err("start_chat: `wait` must be a boolean".to_string()),
    };
    Ok(StartChatParams {
        prompt,
        title: optional_string(params, "title")?,
        model: optional_string(params, "model")?,
        profile: optional_string(params, "profile")?,
        wait,
    })
}

impl ToolActionRunner {
    /// Start a new chat thread with `prompt` as its first user message and run a
    /// normal chat turn on it under the automation's authority. With `wait`
    /// (default) the step ends when the reply is done and returns it; otherwise
    /// it returns the ids and the turn keeps streaming in the background.
    pub(super) async fn run_start_chat(
        &self,
        workspace_id: WorkspaceId,
        action: &Action,
        trigger: Option<&Value>,
        grant: Option<&Grant>,
    ) -> ActionOutcome {
        let ctx = match self.resolve_context(workspace_id, grant).await {
            Ok(ctx) => ctx,
            Err(e) => return ActionOutcome::failed(e),
        };
        if !cap_allows(
            &ctx,
            &Capability::new(CapAction::Write, Resource::domain("conversation")),
        ) {
            return ActionOutcome::failed(
                "start_chat denied: requires conversation:write authority",
            );
        }
        let params = match parse_params(&render_params(&action.params, trigger)) {
            Ok(p) => p,
            Err(e) => return ActionOutcome::failed(e),
        };

        if ctx.dry_run {
            return ActionOutcome::succeeded(Some(json!({
                "dry_run": true,
                "prompt": params.prompt,
                "title": params.title,
                "model": params.model,
                "profile": params.profile,
                "wait": params.wait,
            })));
        }
        let Some(store) = &self.store else {
            return ActionOutcome::failed("automation action `StartChat` needs an attached store");
        };
        let Some(starter) = &self.chat_starter else {
            return ActionOutcome::failed(
                "automation action `StartChat` needs the chat runtime; this runner has none",
            );
        };
        let Some(user_id) = ctx.user_id else {
            return ActionOutcome::failed("start_chat: the automation has no acting user");
        };

        // Resolve the profile before anything is written, so a typo leaves no
        // empty thread behind.
        let profile_id = match &params.profile {
            Some(name) => match store.agent_profiles().get_by_name(workspace_id, name).await {
                Ok(Some(p)) => Some(p.id),
                Ok(None) => {
                    return ActionOutcome::failed(format!(
                        "start_chat: no agent profile named `{name}`"
                    ))
                }
                Err(e) => return ActionOutcome::failed(format!("start_chat: {e}")),
            },
            None => None,
        };

        let conversations = store.conversations();
        let conversation = match conversations
            .create(workspace_id, params.title.as_deref(), Origin::Automation)
            .await
        {
            Ok(c) => c,
            Err(e) => return ActionOutcome::failed(format!("start_chat: {e}")),
        };
        let setup = async {
            // A given title is pinned so the post-turn auto-title keeps it.
            if params.title.is_some() {
                conversations
                    .rename_manual(workspace_id, conversation.id, params.title.as_deref())
                    .await?;
            }
            if params.model.is_some() {
                conversations
                    .set_model(workspace_id, conversation.id, params.model.as_deref())
                    .await?;
            }
            if profile_id.is_some() {
                conversations
                    .set_agent_profile(workspace_id, conversation.id, profile_id)
                    .await?;
            }
            Ok::<(), catalerum_store::StoreError>(())
        };
        if let Err(e) = setup.await {
            let _ = conversations.delete(workspace_id, conversation.id).await;
            return ActionOutcome::failed(format!("start_chat: {e}"));
        }

        let started = match starter
            .start_turn(ChatTurnRequest {
                workspace_id,
                user_id,
                grant_id: ctx.grant_id,
                capabilities: ctx.capabilities.clone().unwrap_or_default(),
                conversation_id: conversation.id,
                prompt: params.prompt.clone(),
            })
            .await
        {
            Ok(s) => s,
            Err(e) => {
                let _ = conversations.delete(workspace_id, conversation.id).await;
                return ActionOutcome::failed(format!("start_chat: {e}"));
            }
        };

        let mut output = json!({
            "conversation_id": conversation.id,
            "user_message_id": started.user_message_id,
            "title": params.title,
            "prompt": params.prompt,
        });
        if !params.wait {
            output["status"] = json!("running");
            return ActionOutcome::succeeded(Some(output));
        }
        match started.done.await {
            Ok(Ok(report)) => {
                output["status"] = json!(if report.stopped { "stopped" } else { "done" });
                output["message_id"] = json!(report.message_id);
                output["reply"] = json!(report.reply);
                output["truncated"] = json!(report.truncated);
                ActionOutcome::succeeded(Some(output))
            }
            // The thread stays: whatever the turn persisted before failing is
            // still useful, and the user can retry from it.
            Ok(Err(e)) => ActionOutcome::failed(format!(
                "start_chat: turn in conversation {} failed: {e}",
                conversation.id
            )),
            Err(e) => ActionOutcome::failed(format!(
                "start_chat: turn in conversation {} aborted: {e}",
                conversation.id
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn parses_defaults_and_trims() {
        let p = parse_params(&params(json!({ "prompt": "  hi  ", "title": " " }))).unwrap();
        assert_eq!(p.prompt, "hi");
        assert_eq!(p.title, None);
        assert!(p.wait);
        let p = parse_params(&params(json!({ "prompt": "x", "wait": "false" }))).unwrap();
        assert!(!p.wait);
    }

    #[test]
    fn rejects_missing_prompt_and_bad_types() {
        assert!(parse_params(&params(json!({ "prompt": "  " })))
            .unwrap_err()
            .contains("prompt"));
        assert!(parse_params(&params(json!({ "prompt": "x", "model": 3 })))
            .unwrap_err()
            .contains("model"));
        assert!(
            parse_params(&params(json!({ "prompt": "x", "wait": "maybe" })))
                .unwrap_err()
                .contains("wait")
        );
    }
}
