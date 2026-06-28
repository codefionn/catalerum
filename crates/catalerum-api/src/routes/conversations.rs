//! Conversations REST (SOUL §12).
//!
//! All routes are workspace-scoped to the authenticated principal's workspace —
//! the client never names a workspace; cross-workspace reach is impossible by
//! construction (SOUL §18). They are **capability-gated** ([`Auth::require`],
//! SOUL §19): listing/reading needs `conversation:read` (every role), creating a
//! conversation needs `conversation:write` (a Viewer is `403 Forbidden`).
//! Conversations and their messages are persisted via `catalerum-store`.
//!
//! Routes:
//! - `POST   /conversations`                      create a conversation
//! - `GET    /conversations`                      list the workspace's conversations (newest first)
//! - `GET    /conversations/{id}`                 fetch one conversation
//! - `PUT    /conversations/{id}`                 rename a conversation (`{title}`)
//! - `DELETE /conversations/{id}`                 delete a conversation (+ its messages, `204`)
//! - `GET    /conversations/{id}/messages`        list messages (oldest first, replay order)
//! - `GET    /conversations/{id}/questions`       list `ask_user` forms + their answers (oldest first)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_core::capability::Action;
use catalerum_core::model::{Conversation, Message, Origin, PendingApproval, PendingQuestion};
use catalerum_core::{AgentProfileId, ConversationId};
use catalerum_store::{StoreError, DEFAULT_MESSAGE_SEARCH_LIMIT};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the conversation routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/conversations", get(list).post(create))
        // `/search` is a literal segment, matched ahead of `/{id}` by axum.
        .route("/conversations/search", get(search))
        .route(
            "/conversations/{id}",
            get(get_one).put(rename).delete(remove),
        )
        .route("/conversations/{id}/profile", post(set_profile))
        .route("/conversations/{id}/model", post(set_model))
        .route("/conversations/{id}/reasoning", post(set_reasoning))
        .route("/conversations/{id}/messages", get(list_messages))
        .route(
            "/conversations/{id}/pending_question",
            get(pending_question),
        )
        .route("/conversations/{id}/questions", get(list_questions))
        .route(
            "/conversations/{id}/pending_approval",
            get(pending_approval),
        )
        .route("/conversations/{id}/active_turn", get(active_turn))
}

/// Body for `POST /conversations`. `title` is optional. `origin` defaults to
/// `web` and is constrained to the API origins.
#[derive(Debug, Default, Deserialize)]
pub struct CreateConversation {
    /// Optional client-generated idempotency id. Reusing it returns the existing
    /// conversation rather than minting a duplicate after a lost POST response.
    #[serde(default)]
    pub id: Option<ConversationId>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub origin: Option<Origin>,
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateConversation>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    let origin = body.origin.unwrap_or(Origin::Web);
    let conversations = state.store().conversations();
    let conv = match body.id {
        Some(id) => {
            conversations
                .create_with_id(ws, id, body.title.as_deref(), origin)
                .await?
        }
        None => {
            conversations
                .create(ws, body.title.as_deref(), origin)
                .await?
        }
    };
    Ok(Json(conv))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Conversation>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    let conversations = state.store().conversations().list_by_workspace(ws).await?;
    Ok(Json(conversations))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    let conv = state.store().conversations().get(ws, id).await?;
    Ok(Json(conv))
}

/// Body for `PUT /conversations/{id}` — rename a conversation.
#[derive(Debug, Deserialize)]
pub struct RenameConversation {
    pub title: String,
}

/// `PUT /conversations/{id}` — rename a conversation. Workspace-scoped (`404` for
/// a foreign/unknown id); rejects an empty title.
async fn rename(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
    Json(body): Json<RenameConversation>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request(
            "conversation title must not be empty",
        ));
    }
    state
        .store()
        .conversations()
        .rename_manual(ws, id, Some(title))
        .await?;
    let conv = state.store().conversations().get(ws, id).await?;
    Ok(Json(conv))
}

/// Body for `POST /conversations/{id}/profile` — the chat "run as a profile"
/// picker (SOUL §19). An empty/absent `agent_profile_id` **unbinds** the profile.
#[derive(Debug, Default, Deserialize)]
pub struct SetConversationProfile {
    /// The agent profile id (UUID) to run this thread as; null/blank = unbind.
    #[serde(default)]
    pub agent_profile_id: Option<String>,
}

/// `POST /conversations/{id}/profile` — bind (or unbind) the agent profile a chat
/// thread runs as (SOUL §19/§12). Workspace-scoped; the bound profile must exist in
/// this workspace. The chat loop then runs as that profile under the user's own
/// authority (never escalating; enforced in the ws handler).
async fn set_profile(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
    Json(body): Json<SetConversationProfile>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    let profile_id = match body
        .agent_profile_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => {
            let pid: AgentProfileId = raw
                .parse()
                .map_err(|_| ApiError::bad_request("invalid agent_profile_id"))?;
            // Validate the profile is in this workspace (a clear 400 rather than an
            // opaque FK violation on the update).
            state
                .store()
                .agent_profiles()
                .get(ws, pid)
                .await
                .map_err(|e| match e {
                    StoreError::NotFound => {
                        ApiError::bad_request("agent profile not found in this workspace")
                    }
                    other => {
                        tracing::error!(error = %other, "resolving conversation profile");
                        ApiError::internal("resolving profile failed")
                    }
                })?;
            Some(pid)
        }
        None => None,
    };
    let conv = state
        .store()
        .conversations()
        .set_agent_profile(ws, id, profile_id)
        .await?;
    Ok(Json(conv))
}

/// Body for `POST /conversations/{id}/model` (SOUL §7/§12). A free-form gateway
/// model id to pin for this thread; null/blank = clear the override.
#[derive(Debug, Deserialize)]
pub struct SetConversationModel {
    #[serde(default)]
    pub model: Option<String>,
}

/// `POST /conversations/{id}/model` — pin (or clear) the model this thread's chat
/// loop thinks with (SOUL §7/§12). Workspace-scoped. The id is a free-form gateway
/// string (the gateway routes it), so — like the user's `chat_model` setting — it
/// is stored as given after trimming, not validated against a catalog. Blank clears.
async fn set_model(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
    Json(body): Json<SetConversationModel>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    let model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let conv = state
        .store()
        .conversations()
        .set_model(ws, id, model)
        .await?;
    Ok(Json(conv))
}

/// Body for `POST /conversations/{id}/reasoning` (SOUL §7/§12). A free-form gateway
/// reasoning-effort token (`low`/`medium`/`high`/`xhigh`/`max`) to request for this
/// thread's chat loop; null/blank = no reasoning requested (the provider default).
#[derive(Debug, Deserialize)]
pub struct SetConversationReasoning {
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// `POST /conversations/{id}/reasoning` — set (or clear) the reasoning ("thinking")
/// effort this thread's chat loop requests (the chat "thinking" picker, SOUL §7/§12).
/// Workspace-scoped. The effort is a free-form gateway string (the gateway passes it
/// through to the model), stored as given after trimming, not validated against a
/// catalog. Blank clears (no reasoning requested).
async fn set_reasoning(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
    Json(body): Json<SetConversationReasoning>,
) -> ApiResult<Json<Conversation>> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    let reasoning = body
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let conv = state
        .store()
        .conversations()
        .set_reasoning(ws, id, reasoning)
        .await?;
    Ok(Json(conv))
}

/// `DELETE /conversations/{id}` — delete a conversation and (via `ON DELETE
/// CASCADE`) its messages. Workspace-scoped; `404` for a foreign/unknown id.
async fn remove(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "conversation")?;
    let ws = auth.principal().workspace_id;
    state.store().conversations().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Query for `GET /conversations/search`: `q` is the substring to find, `limit`
/// caps results (clamped to `[1, DEFAULT_MESSAGE_SEARCH_LIMIT]`).
#[derive(Debug, Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    limit: Option<u32>,
}

/// One content-search hit: the matched message (flattened) plus the title of the
/// conversation it lives in, so the client can label + open the thread.
#[derive(Debug, Serialize)]
struct MessageHitView {
    #[serde(flatten)]
    message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_title: Option<String>,
}

/// `GET /conversations/search?q=&limit=` — search message **content** across the
/// caller's workspace (`conversation:read`), newest match first, each hit carrying
/// its conversation title. A blank `q` returns `[]` (no "match everything").
async fn search(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<MessageHitView>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    let cap = u32::try_from(DEFAULT_MESSAGE_SEARCH_LIMIT).unwrap_or(50);
    let limit = i64::from(q.limit.map(|n| n.clamp(1, cap)).unwrap_or(cap));
    let hits = state
        .store()
        .messages()
        .search_in_workspace(ws, &q.q, limit)
        .await?;
    let views = hits
        .into_iter()
        .map(|h| MessageHitView {
            message: h.message,
            conversation_title: h.conversation_title,
        })
        .collect();
    Ok(Json(views))
}

async fn list_messages(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Vec<Message>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    // Verify the conversation belongs to the caller's workspace before exposing
    // its messages (messages are keyed by conversation, not workspace).
    state
        .store()
        .conversations()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let messages = state.store().messages().list_by_conversation(id).await?;
    Ok(Json(messages))
}

/// `GET /conversations/{id}/pending_question` — the thread's unresolved `ask_user`
/// question form, if any (SOUL §7/§12). The client fetches this on load / when
/// opening a conversation to re-render a question form that survived a reload or
/// reconnect; `null` when nothing is pending.
async fn pending_question(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Option<PendingQuestion>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    // Verify the conversation belongs to the caller's workspace first (404 otherwise).
    state
        .store()
        .conversations()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let pending = state
        .store()
        .pending_questions()
        .get_unresolved(ws, id)
        .await?;
    Ok(Json(pending))
}

/// `GET /conversations/{id}/questions` — every `ask_user` question form ever asked
/// in the thread, oldest first, resolved or not (SOUL §7/§12). Each entry carries
/// the questions plus the structured `answers` the user gave (absent while pending
/// or when the form was superseded unanswered). The client fetches this when
/// replaying a transcript so an answered form re-renders with the user's actual
/// picks — correlated to its `ask_user` tool call via the `pending_question_id`
/// in the call's result.
async fn list_questions(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Vec<PendingQuestion>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    // Verify the conversation belongs to the caller's workspace first (404 otherwise).
    state
        .store()
        .conversations()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let questions = state
        .store()
        .pending_questions()
        .list_for_conversation(ws, id)
        .await?;
    Ok(Json(questions))
}

/// `GET /conversations/{id}/pending_approval` — the thread's guard-deferred tool
/// call awaiting the user's Approve/Reject, if any (SOUL §19). The client fetches
/// this on load / when opening a conversation to re-render an approval prompt that
/// survived a reload / reconnect / restart; `null` when nothing is pending.
async fn pending_approval(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Option<PendingApproval>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    // Verify the conversation belongs to the caller's workspace first (404 otherwise).
    state
        .store()
        .conversations()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let pending = state
        .store()
        .pending_approvals()
        .get_unresolved(ws, id)
        .await?;
    Ok(Json(pending))
}

/// The in-flight turn a client should (re)attach to (SOUL §7/§12).
#[derive(Debug, Serialize, Deserialize)]
pub struct ActiveTurn {
    /// The anchoring user message id — the turn key the client attaches with.
    pub user_message_id: catalerum_core::MessageId,
}

/// `GET /conversations/{id}/active_turn` — the conversation's currently streaming
/// turn, if any, so a client opening (or reconnecting to) the thread can attach
/// to the live stream instead of only seeing persisted history. Backed by the
/// cross-pod active-turn registry (any pod answers, whichever runs the turn).
/// `null` when nothing is streaming right now.
async fn active_turn(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConversationId>,
) -> ApiResult<Json<Option<ActiveTurn>>> {
    auth.require(Action::Read, "conversation")?;
    let ws = auth.principal().workspace_id;
    // Verify the conversation belongs to the caller's workspace first (404 otherwise).
    state
        .store()
        .conversations()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let active = state
        .bus()
        .registry()
        .lookup(&crate::routes::ws::active_turn_key(id))
        .await
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<ActiveTurn>(&bytes).ok());
    Ok(Json(active))
}
