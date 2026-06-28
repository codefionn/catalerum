//! Terminal sessions REST/WS (SOUL §20).
//!
//! List a workspace's active **sessions** and tail a session's live output over a
//! **read-only** WebSocket (the terminal pane — the user watches; the agent drives
//! via the `*_terminal` tools). Terminals are always ephemeral — there is no
//! persistent workdir to manage.
//!
//! Workspace-scoped to the authenticated principal (SOUL §18). Terminals are a
//! protected, exec-domain surface (§19/§20): reading them gates on the `exec`
//! domain, which only Owner/Admin hold (via their `*` wildcard) — so a
//! Member/Viewer is `403`, matching how terminals themselves are deny-by-default.

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{any, get};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};

use catalerum_core::capability::Action;
use catalerum_core::model::TerminalSession;
use catalerum_core::{TerminalSessionId, WorkspaceId};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::terminal::TerminalManager;

/// Mount the terminal routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/terminals/sessions", get(list_sessions))
        .route("/terminals/sessions/{id}/output", any(session_output_ws))
}

async fn list_sessions(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<TerminalSession>>> {
    auth.require(Action::Read, "exec")?;
    let p = auth.principal();
    let sessions = state
        .store()
        .terminal_sessions()
        .list_active(p.workspace_id)
        .await?;
    Ok(Json(sessions))
}

/// `GET /terminals/sessions/{id}/output` — upgrade to a read-only WebSocket that
/// streams the session's live output bytes (the terminal pane). The agent drives
/// the session via tools; the client only watches (inbound frames are ignored
/// except a close).
async fn session_output_ws(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    auth.require(Action::Read, "exec")?;
    let p = auth.principal();
    let manager = state.terminal_manager().ok_or(ApiError::NotFound)?.clone();
    let session_id: TerminalSessionId = id
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request("invalid session id"))?;
    Ok(ws.on_upgrade(move |socket| stream_output(socket, manager, p.workspace_id, session_id)))
}

async fn stream_output(
    socket: WebSocket,
    manager: std::sync::Arc<TerminalManager>,
    workspace_id: WorkspaceId,
    session_id: TerminalSessionId,
) {
    let (mut sink, mut stream) = socket.split();
    let mut output = match manager.output(workspace_id, session_id).await {
        Ok(o) => o,
        Err(e) => {
            let _ = sink
                .send(WsMessage::Text(format!("error: {e}").into()))
                .await;
            return;
        }
    };
    loop {
        tokio::select! {
            chunk = output.next() => match chunk {
                Some(Ok(bytes)) => {
                    if sink.send(WsMessage::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            inbound = stream.next() => match inbound {
                Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }
}
