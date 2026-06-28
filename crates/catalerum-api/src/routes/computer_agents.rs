//! Computer-agent management REST + the agent-daemon WebSocket (SOUL §19/§20).
//!
//! A **computer agent** is a daemon a user installs on a server or desktop
//! (linux / macos / windows). It dials *out* to this endpoint over an
//! authenticated WebSocket and serves scoped file / search / exec / desktop
//! operations the LLM drives through the `computer_*` tools. This module is the
//! management + connection surface:
//!
//! - `GET    /computer-agents`          — list the workspace's enrolled agents (+ online)
//! - `POST   /computer-agents`          — enroll one; the bearer token is returned **once**
//! - `DELETE /computer-agents/{id}`     — revoke one (its token stops authenticating, its
//!   live connection is dropped; the row is retained for audit)
//! - `GET    /computer-agents/connect`  — the daemon's WebSocket (auth = the agent token)
//!
//! Controlling a whole host is a **protected** scope (SOUL §19, like the Local
//! executor). Enrolling / revoking therefore requires a workspace **administrator**
//! (Owner/Admin), and the management reads gate on the `computer` domain — which is
//! not a standard member domain, so a Member/Viewer is `403`. The daemon itself
//! authenticates with its own long-lived token (stored only as a SHA-256 hash, the
//! same opaque-token scheme as `sessions`), never a user session.

use std::sync::Arc;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{any, get};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::computer::{AgentToServer, ComputerCapabilities, ServerToAgent};
use catalerum_core::ComputerAgentId;
use catalerum_iam::token;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// How often the server pings a connected agent (and refreshes its `last_seen`).
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(25);

/// Mount the computer-agent routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/computer-agents", get(list).post(enroll))
        .route("/computer-agents/{id}", axum::routing::delete(revoke))
        .route("/computer-agents/connect", any(connect))
}

/// An enrolled agent as shown in a listing. Never carries the token (only a hash
/// is stored). `online` reflects a live connection **to this pod**.
#[derive(Debug, Serialize)]
pub struct ComputerAgentView {
    pub id: ComputerAgentId,
    pub name: String,
    /// Platform token (`linux`/`macos`/`windows`/`other`), or `null` before first
    /// connect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Whether a live connection exists on this pod right now.
    pub online: bool,
    /// The machine's last-announced capabilities (present once it has connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ComputerCapabilities>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Body for `POST /computer-agents`.
#[derive(Debug, Default, Deserialize)]
pub struct EnrollBody {
    /// Workspace-unique display name for the machine (e.g. `"build-server"`).
    #[serde(default)]
    pub name: String,
}

/// Response for `POST /computer-agents` — the raw enrollment token, shown **once**.
#[derive(Debug, Serialize)]
pub struct EnrolledAgent {
    pub id: ComputerAgentId,
    pub name: String,
    /// The bearer token the daemon authenticates with. Copy it now — it is stored
    /// only as a hash and never shown again.
    pub token: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn list(
    State(state): State<AppState>,
    auth: Auth,
) -> ApiResult<Json<Vec<ComputerAgentView>>> {
    auth.require(Action::Read, "computer")?;
    let ws = auth.principal().workspace_id;
    let agents = state
        .store()
        .computer_agents()
        .list_by_workspace(ws)
        .await?;
    let registry = state.computer_registry();
    let online = registry.online_in_workspace(ws).await;
    let online_ids: std::collections::HashSet<ComputerAgentId> =
        online.iter().map(|o| o.id).collect();

    let views = agents
        .into_iter()
        .map(|a| ComputerAgentView {
            online: online_ids.contains(&a.id),
            platform: a.platform.map(|p| p.label().to_lowercase()),
            capabilities: a.capabilities,
            id: a.id,
            name: a.name,
            created_at: a.created_at,
            last_seen_at: a.last_seen_at,
            revoked_at: a.revoked_at,
        })
        .collect();
    Ok(Json(views))
}

async fn enroll(
    State(state): State<AppState>,
    auth: Auth,
    body: Option<Json<EnrollBody>>,
) -> ApiResult<(StatusCode, Json<EnrolledAgent>)> {
    auth.require(Action::Write, "computer")?;
    // Enrolling a host-control daemon is a protected, workspace-operational action.
    auth.require_workspace_admin()?;
    let p = auth.principal();
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("a name is required"));
    }

    // Mint an opaque bearer token; the store only ever sees its SHA-256 hash
    // (SOUL §13), the same scheme as sessions / login tokens.
    let raw = token::generate();
    let token_hash = token::hash_token(&raw);
    let agent = state
        .store()
        .computer_agents()
        .create(p.workspace_id, p.user_id, name, &token_hash)
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(EnrolledAgent {
            id: agent.id,
            name: agent.name,
            token: raw,
            created_at: agent.created_at,
        }),
    ))
}

async fn revoke(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "computer")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    let id: ComputerAgentId = id
        .trim()
        .parse()
        .map_err(|_| ApiError::bad_request("invalid computer-agent id"))?;
    state.store().computer_agents().revoke(ws, id).await?;
    // Drop any live connection immediately — locally, and (if the agent is held on
    // another pod) by forwarding a disconnect to its owner — so a revoked token
    // can't keep serving from any pod.
    state.computer_registry().disconnect_everywhere(id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Query params on the daemon's connect URL — the enrollment token (accepted as
/// `token` or `access_token`, mirroring the chat WS handshake).
#[derive(Debug, Default, Deserialize)]
struct ConnectParams {
    #[serde(default)]
    token: String,
    #[serde(default)]
    access_token: String,
}

/// `GET /computer-agents/connect?token=…` — the daemon's WebSocket. Authenticated
/// by the agent's own token (looked up by hash; a revoked/unknown token is `401`),
/// **not** a user session. On upgrade the connection is registered live and serves
/// [`ServerToAgent`] requests until it closes.
async fn connect(
    State(state): State<AppState>,
    Query(params): Query<ConnectParams>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let raw = if !params.token.trim().is_empty() {
        params.token
    } else {
        params.access_token
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ApiError::unauthorized("missing agent token"));
    }
    let token_hash = token::hash_token(raw);
    let agent = state
        .store()
        .computer_agents()
        .get_active_by_token_hash(&token_hash)
        .await
        .map_err(|_| ApiError::unauthorized("unknown or revoked agent token"))?;

    Ok(ws.on_upgrade(move |socket| handle_agent_socket(socket, state, agent)))
}

/// Read the next parseable [`AgentToServer`] frame, skipping ws ping/pong and any
/// unparseable text. `None` on close/EOF.
async fn next_agent_frame(
    stream: &mut futures::stream::SplitStream<WebSocket>,
) -> Option<AgentToServer> {
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            WsMessage::Text(t) => {
                if let Ok(frame) = serde_json::from_str::<AgentToServer>(&t) {
                    return Some(frame);
                }
                // Unparseable frame — skip and keep reading.
            }
            WsMessage::Binary(b) => {
                if let Ok(frame) = serde_json::from_slice::<AgentToServer>(&b) {
                    return Some(frame);
                }
            }
            WsMessage::Ping(_) | WsMessage::Pong(_) => {}
            WsMessage::Close(_) => return None,
        }
    }
    None
}

/// Drive one authenticated agent connection: register it live on `Hello`, resolve
/// its `Response`s to the waiting `computer_*` tool calls, and heartbeat it (which
/// also refreshes `last_seen`). Deregisters on close.
async fn handle_agent_socket(
    socket: WebSocket,
    state: AppState,
    agent: catalerum_store::ComputerAgent,
) {
    let (mut sink, mut stream) = socket.split();
    // Outbound frames (requests + pings) funnel through one channel so the tool
    // dispatcher and the heartbeat share the single writer.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ServerToAgent>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let txt = match serde_json::to_string(&frame) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(WsMessage::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    let registry = state.computer_registry();
    let agents = state.store().computer_agents();
    let mut hello_caps: Option<ComputerCapabilities> = None;
    // Set once the connection is registered (on `Hello`); notified if the agent is
    // revoked / replaced so we tear the socket down at once.
    let mut close: Option<Arc<tokio::sync::Notify>> = None;
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            // Revoked or superseded elsewhere → stop serving this socket.
            _ = async {
                match &close {
                    Some(n) => n.notified().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!(agent = %agent.id, "computer agent connection dropped (revoked or replaced)");
                break;
            }
            _ = heartbeat.tick() => {
                // Safety net for a revoke that happened on ANOTHER pod (its
                // ComputerDisconnect forward may have been missed): if the agent is
                // now revoked, stop serving. Checked first so a revoked agent is
                // never re-announced as online below.
                if let Ok(row) = agents.get(agent.workspace_id, agent.id).await {
                    if row.revoked_at.is_some() {
                        tracing::info!(agent = %agent.id, "computer agent revoked; closing connection");
                        break;
                    }
                }
                if tx.send(ServerToAgent::Ping).is_err() {
                    break;
                }
                // Keep `last_seen` fresh + refresh the cross-pod ownership key so
                // other pods keep routing ops here (best-effort).
                if let Some(caps) = &hello_caps {
                    let _ = agents.touch_seen(agent.id, caps).await;
                }
                registry.announce_ownership(agent.id).await;
            }
            frame = next_agent_frame(&mut stream) => match frame {
                Some(AgentToServer::Hello { capabilities }) => {
                    // Persist the announced capabilities + register the live conn.
                    let _ = agents.touch_seen(agent.id, &capabilities).await;
                    close = Some(
                        registry
                            .connect(
                                agent.id,
                                agent.workspace_id,
                                agent.name.clone(),
                                capabilities.clone(),
                                tx.clone(),
                            )
                            .await,
                    );
                    hello_caps = Some(capabilities);
                }
                Some(AgentToServer::Response(resp)) => {
                    registry.resolve_response(agent.id, resp).await;
                }
                Some(AgentToServer::Pong) => {}
                None => break,
            },
        }
    }

    registry.disconnect(agent.id).await;
    writer.abort();
}
