//! Memories + profile REST (SOUL §22) — the HTTP surface over the existing
//! `MemoryRepo`/`ProfileRepo` data layer (the same repos the `recall_memory` /
//! `update_profile` LLM tools use). Memories are durable, inspectable, editable
//! free-text facts; the profile is the per-user structured record injected into
//! the chat system prompt each turn.
//!
//! Workspace-scoped to the authenticated principal (SOUL §18) and
//! **capability-gated (SOUL §19)**: reads require `memory:read` / `profile:read`
//! (every role); writes require `memory:write` / `profile:write` (a Viewer is
//! `403 Forbidden`). Memory listing is **visibility-filtered** to the acting user
//! (workspace-scoped memories + the user's own private ones) by the repo, so a
//! member never sees another's private memory (§22).
//!
//! Routes:
//! - `GET    /memories?limit=N` — the visible memories, newest first
//! - `POST   /memories`         — create (`{scope, text}`; `user`-scope is private to the caller)
//! - `PUT    /memories/{id}`    — replace a memory's text
//! - `DELETE /memories/{id}`    — delete a memory
//! - `GET    /profile`          — the caller's profile (an empty one if unset)
//! - `PUT    /profile`          — merge fields into the caller's profile (JSONB `||`)

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{Map, Memory, MemoryScope, Profile};
use catalerum_core::MemoryId;
use catalerum_ingest::MemoryStoreStatus;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the memory + profile routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/memories", get(list_memories).post(create_memory))
        .route("/memories/{id}", put(update_memory).delete(delete_memory))
        .route("/profile", get(get_profile).put(update_profile))
}

/// Default + max number of memories `GET /memories` returns.
const MEMORIES_DEFAULT_LIMIT: i64 = 100;
const MEMORIES_MAX_LIMIT: i64 = 500;

/// Query for `GET /memories` — `?limit=N` (clamped).
#[derive(Debug, Default, Deserialize)]
pub struct MemoryQuery {
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Body for `POST /memories`.
#[derive(Debug, Deserialize)]
pub struct CreateMemory {
    /// `user` (private to the caller) or `workspace` (shared).
    pub scope: MemoryScope,
    /// The fact text.
    pub text: String,
}

/// Body for `PUT /memories/{id}`.
#[derive(Debug, Deserialize)]
pub struct UpdateMemory {
    /// The replacement text.
    pub text: String,
}

/// Resolve the effective memory-list limit, clamped to `[1, MAX]`.
fn memories_limit(requested: Option<i64>) -> i64 {
    requested
        .unwrap_or(MEMORIES_DEFAULT_LIMIT)
        .clamp(1, MEMORIES_MAX_LIMIT)
}

async fn list_memories(
    State(state): State<AppState>,
    auth: Auth,
    Query(q): Query<MemoryQuery>,
) -> ApiResult<Json<Vec<Memory>>> {
    let p = auth.principal();
    auth.require(Action::Read, "memory")?;
    let limit = memories_limit(q.limit);
    // Visibility-filtered to the acting user (workspace memories + the caller's
    // own private ones) by the repo — never another member's private memory (§22).
    let memories = state
        .store()
        .memories()
        .list_visible(p.workspace_id, Some(p.user_id), limit)
        .await?;
    Ok(Json(memories))
}

async fn create_memory(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateMemory>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let p = auth.principal();
    auth.require(Action::Write, "memory")?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("memory text must not be empty"));
    }
    // Route through the shared dedup seam (SOUL §29): a fact the workspace already
    // knows is not stored twice — it is deduplicated (existing row touched), and a
    // fact that extends a known one refines it. `user_id` is honored only for
    // `User` scope; the repo nulls it for `Workspace` scope (a shared memory is
    // never tied to a member).
    let outcome = state
        .store_memory_deduped(p.workspace_id, body.scope, Some(p.user_id), text)
        .await?;
    // A created resource is `201 Created`; a dedup/refine created nothing new, so
    // it is an idempotent `200 OK`. The body is the memory (unchanged shape) plus
    // an additive `status` field distinguishing stored / deduplicated / refined.
    let code = match outcome.status {
        MemoryStoreStatus::Stored => StatusCode::CREATED,
        MemoryStoreStatus::Deduplicated | MemoryStoreStatus::Refined => StatusCode::OK,
    };
    let mut value = serde_json::to_value(&outcome.memory).map_err(ApiError::internal)?;
    if let serde_json::Value::Object(map) = &mut value {
        map.insert(
            "status".to_string(),
            serde_json::json!(outcome.status.as_str()),
        );
    }
    Ok((code, Json(value)))
}

async fn update_memory(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<MemoryId>,
    Json(body): Json<UpdateMemory>,
) -> ApiResult<Json<Memory>> {
    let p = auth.principal();
    auth.require(Action::Write, "memory")?;
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("memory text must not be empty"));
    }
    let memory = state
        .store()
        .memories()
        .update_text(p.workspace_id, id, text)
        .await?;
    Ok(Json(memory))
}

async fn delete_memory(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<MemoryId>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "memory")?;
    state.store().memories().delete(p.workspace_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_profile(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Profile>> {
    let p = auth.principal();
    auth.require(Action::Read, "profile")?;
    // Sole-user cache (SOUL §29): served from the per-workspace snapshot in
    // single_user mode, a direct read otherwise. Byte-identical either way.
    let profile = state.cached_profile(p.workspace_id, p.user_id).await?;
    Ok(Json(profile))
}

async fn update_profile(
    State(state): State<AppState>,
    auth: Auth,
    Json(fields): Json<Map>,
) -> ApiResult<Json<Profile>> {
    let p = auth.principal();
    auth.require(Action::Write, "profile")?;
    // JSONB merge: incoming keys win, existing keys not present are preserved.
    let profile = state
        .store()
        .profiles()
        .merge(p.workspace_id, p.user_id, &fields)
        .await?;
    // Invalidate the sole-user personalization cache (SOUL §29).
    state.bump_personalization(p.workspace_id);
    Ok(Json(profile))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memories_limit_defaults_and_clamps() {
        assert_eq!(memories_limit(None), MEMORIES_DEFAULT_LIMIT);
        assert_eq!(memories_limit(Some(10)), 10);
        assert_eq!(memories_limit(Some(0)), 1);
        assert_eq!(memories_limit(Some(-5)), 1);
        assert_eq!(memories_limit(Some(99_999)), MEMORIES_MAX_LIMIT);
    }

    #[test]
    fn create_memory_parses_scope() {
        let u: CreateMemory =
            serde_json::from_str(r#"{"scope":"user","text":"likes tea"}"#).unwrap();
        assert_eq!(u.scope, MemoryScope::User);
        assert_eq!(u.text, "likes tea");
        let w: CreateMemory =
            serde_json::from_str(r#"{"scope":"workspace","text":"office in NYC"}"#).unwrap();
        assert_eq!(w.scope, MemoryScope::Workspace);
    }

    #[test]
    fn profile_fields_decode_as_map() {
        let m: Map = serde_json::from_str(r#"{"tz":"UTC","hours":"9-5"}"#).unwrap();
        assert_eq!(m.get("tz").and_then(|v| v.as_str()), Some("UTC"));
    }
}
