//! Notes REST (SOUL §12, §21 — M3 markdown notes).
//!
//! All routes are workspace-scoped to the authenticated principal's workspace —
//! the client never names a workspace; cross-workspace reach is impossible by
//! construction (SOUL §18). They are **capability-gated** ([`Auth::require`],
//! SOUL §19): reads need `notes:read` (every role), writes need `notes:write`
//! (a Viewer is `403 Forbidden`). Notes are persisted via `catalerum-store`. A note
//! created over this surface is authored by the calling user
//! ([`Author::User`]); automations author notes as [`Author::Agent`] through the
//! same repository (SOUL §21).
//!
//! Routes:
//! - `POST   /notes`        create a note (`201`)
//! - `GET    /notes`        list this workspace's notes (most-recently-edited first)
//! - `GET    /notes/{id}`   fetch one note
//! - `PUT    /notes/{id}`   update a note's title / markdown / tags
//! - `DELETE /notes/{id}`   delete a note (`204`)

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{Author, Note};
use catalerum_core::NoteId;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the notes routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/notes", get(list).post(create))
        .route("/notes/{id}", get(get_one).put(update).delete(delete_note))
}

/// Body for `POST /notes`. Only `title` is required; `markdown` and `tags`
/// default to empty.
#[derive(Debug, Deserialize)]
pub struct CreateNote {
    /// Note title (must be non-empty after trimming).
    pub title: String,
    /// Markdown body. Defaults to empty.
    #[serde(default)]
    pub markdown: String,
    /// Free-text tags. Defaults to none.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Body for `PUT /notes/{id}`. A full replacement of the editable fields; the
/// note's author is immutable (SOUL §21).
#[derive(Debug, Deserialize)]
pub struct UpdateNote {
    /// New title (must be non-empty after trimming).
    pub title: String,
    /// New markdown body.
    #[serde(default)]
    pub markdown: String,
    /// New tag set (replaces the existing tags).
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Normalize a tag list: trim each, drop empties, de-duplicate while preserving
/// order. Keeps stored tags clean regardless of client input. Shared by the
/// notes REST routes and the LLM note tools (`crate::tools`) so both normalize
/// identically.
pub(crate) fn clean_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for tag in tags {
        let trimmed = tag.trim();
        if !trimmed.is_empty() && !out.iter().any(|t| t == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateNote>,
) -> ApiResult<(StatusCode, Json<Note>)> {
    auth.require(Action::Write, "notes")?;
    let principal = auth.principal();
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("note title must not be empty"));
    }
    let tags = clean_tags(body.tags);
    let note = state
        .store()
        .notes()
        .create(
            principal.workspace_id,
            Author::User {
                id: principal.user_id,
            },
            title,
            &body.markdown,
            &tags,
        )
        .await?;
    // Best-effort: (re-)embed the note into Qdrant (SOUL §6.4/§10/§21). Never
    // fails the write; a no-op unless `[qdrant].enabled`.
    state
        .enqueue_note_ingest(principal.workspace_id, note.id)
        .await;
    Ok((StatusCode::CREATED, Json(note)))
}

async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Note>>> {
    auth.require(Action::Read, "notes")?;
    let ws = auth.principal().workspace_id;
    // §18: bounded to the most-recent `DEFAULT_NOTE_LIMIT` so a huge note
    // collection can't balloon the payload (generous — normal use is unaffected).
    let notes = state
        .store()
        .notes()
        .list_by_workspace(ws, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await?;
    Ok(Json(notes))
}

async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<NoteId>,
) -> ApiResult<Json<Note>> {
    auth.require(Action::Read, "notes")?;
    let ws = auth.principal().workspace_id;
    let note = state.store().notes().get(ws, id).await?;
    Ok(Json(note))
}

async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<NoteId>,
    Json(body): Json<UpdateNote>,
) -> ApiResult<Json<Note>> {
    auth.require(Action::Write, "notes")?;
    let ws = auth.principal().workspace_id;
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("note title must not be empty"));
    }
    let tags = clean_tags(body.tags);
    let note = state
        .store()
        .notes()
        .update(ws, id, title, &body.markdown, &tags)
        .await?;
    // Re-embed the edited note (best-effort, no-op unless `[qdrant].enabled`).
    state.enqueue_note_ingest(ws, note.id).await;
    Ok(Json(note))
}

async fn delete_note(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<NoteId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "notes")?;
    let ws = auth.principal().workspace_id;
    state.store().notes().delete(ws, id).await?;
    // Reconcile the projection: the worker finds the note gone and purges its
    // vectors/chunks/document (best-effort, no-op unless `[qdrant].enabled`).
    state.enqueue_note_ingest(ws, id).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_note_defaults_markdown_and_tags() {
        let body: CreateNote = serde_json::from_str(r#"{"title":"Groceries"}"#).unwrap();
        assert_eq!(body.title, "Groceries");
        assert!(body.markdown.is_empty());
        assert!(body.tags.is_empty());
    }

    #[test]
    fn update_note_parses_full_shape() {
        let body: UpdateNote = serde_json::from_str(
            r##"{"title":"Plan","markdown":"# H","tags":["work","work","  "]}"##,
        )
        .unwrap();
        assert_eq!(body.title, "Plan");
        assert_eq!(body.markdown, "# H");
        // Raw tags carry duplicates / blanks; cleaning happens in the handler.
        assert_eq!(body.tags.len(), 3);
    }

    #[test]
    fn clean_tags_trims_dedups_and_drops_empties() {
        let cleaned = clean_tags(vec![
            "  work ".to_string(),
            "work".to_string(),
            String::new(),
            "  ".to_string(),
            "ideas".to_string(),
        ]);
        assert_eq!(cleaned, vec!["work".to_string(), "ideas".to_string()]);
    }
}
