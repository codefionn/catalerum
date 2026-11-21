//! Chat turns started without a client socket (the automation `start_chat`
//! action, SOUL §7/§11/§12).
//!
//! The turn runs through the same [`spawn_turn`](super::spawn_turn) path a
//! socket uses: it registers as live, holds the conversation turn lock, and
//! streams into the replayable turn buffer. So a user who opens the thread while
//! the automation is still running attaches to the live stream like any
//! reconnecting client.

use async_trait::async_trait;
use catalerum_bus::TurnId;
use catalerum_core::model::Role;
use catalerum_core::MessageId;
use catalerum_iam::Principal;

use super::{spawn_turn, ClientFrame, TurnIntake};
use crate::action_runner::{ChatTurnReport, ChatTurnRequest, ChatTurnStarter, StartedChatTurn};
use crate::state::AppState;

/// The production [`ChatTurnStarter`]: runs turns against the live app state.
#[derive(Clone)]
pub(crate) struct AppChatStarter {
    state: AppState,
}

impl AppChatStarter {
    pub(crate) fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ChatTurnStarter for AppChatStarter {
    async fn start_turn(&self, request: ChatTurnRequest) -> Result<StartedChatTurn, String> {
        // The role is never consulted: the explicit capability override below is
        // the turn's whole authority. Member is the least surprising filler.
        let principal = Principal {
            user_id: request.user_id,
            workspace_id: request.workspace_id,
            role: Role::Member,
            grant_id: request.grant_id,
        };
        let user_message_id = MessageId::new();
        let turn_id = TurnId::new(request.conversation_id, user_message_id);
        let frame = ClientFrame {
            conversation_id: request.conversation_id,
            content: request.prompt,
            user_message_id: Some(user_message_id),
            regenerate_from: None,
            attachments: Vec::new(),
            skill: None,
            answers: Vec::new(),
            conversation_mode: false,
        };
        let run = spawn_turn(
            self.state.clone(),
            principal,
            frame,
            turn_id,
            TurnIntake::default(),
            Some(request.capabilities),
        );
        let done = tokio::spawn(async move {
            let report = run.await.map_err(|e| e.to_string())??;
            Ok(ChatTurnReport {
                message_id: report.final_message_id,
                reply: report.content,
                stopped: report.stopped,
                truncated: report.truncated,
                output_limited: report.output_limited,
            })
        });
        Ok(StartedChatTurn {
            user_message_id,
            done,
        })
    }
}
