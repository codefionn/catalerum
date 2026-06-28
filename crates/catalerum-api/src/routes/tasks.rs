//! Kanban boards + tasks REST (SOUL §24) — the HTTP surface over the existing
//! `BoardRepo`/`TaskRepo` data layer (the same repos the `*_task` LLM tools and
//! the `TaskMoved` automation source already use).
//!
//! Workspace-scoped to the authenticated principal (SOUL §18): the client never
//! names a workspace, so cross-workspace reach is impossible by construction.
//! **Capability-gated (SOUL §19)** via [`Auth::require`]: reads require
//! `tasks:read` (every role); writes — create board / create task / move /
//! set-status — require `tasks:write` (a Viewer is `403 Forbidden`).
//!
//! Routes:
//! - `GET    /boards`               list the workspace's boards (with columns)
//! - `POST   /boards`               create a board (`{name, columns?}`; default columns if absent)
//! - `GET    /boards/{id}`          fetch one board
//! - `PUT    /boards/{id}`          rename a board (`{name}`)
//! - `DELETE /boards/{id}`          delete a board (+ its columns & tasks, `204`)
//! - `GET    /boards/{id}/tasks`    the board's tasks (column order)
//! - `POST   /boards/{id}/tasks`    create a task in a column (`{column_id, title, body_md?}`)
//! - `POST   /boards/{id}/columns`  append a column (`{name}`)
//! - `PUT    /columns/{id}`         rename a column (`{name}`)
//! - `DELETE /columns/{id}`         delete an **empty** column (400 when tasks remain)
//! - `POST   /tasks/{id}/move`      move a task (`{column_id, position?}`; `position` = final
//!   0-based index in the destination column, clamped, absent = end — a same-column
//!   move with a `position` is a within-column reorder)
//! - `POST   /tasks/{id}/status`    set a task's status (`{status}`)
//! - `PUT    /tasks/{id}`           edit a task's title + body (`{title, body_md?}`)
//! - `DELETE /tasks/{id}`           delete a task / card (`204`)
//!
//! Moving via this route **re-dispatches the §11 `TaskMoved` automation source**
//! (best-effort, only on a real cross-column transition) — the same bridge the
//! agent `move_task` tool fires — so a UI/REST move drives automations
//! identically; the move itself is durable even if the dispatch fails.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;

use catalerum_core::capability::Action;
use catalerum_core::model::{Board, Task, TaskStatus};
use catalerum_core::{BoardId, ColumnId, TaskId};

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the board + task routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/boards", get(list_boards).post(create_board))
        .route(
            "/boards/{id}",
            get(get_board).put(rename_board).delete(delete_board),
        )
        .route(
            "/boards/{id}/tasks",
            get(list_board_tasks).post(create_task),
        )
        .route("/boards/{id}/columns", post(add_column))
        .route("/columns/{id}", put(rename_column).delete(delete_column))
        .route("/tasks/{id}/move", post(move_task))
        .route("/tasks/{id}/status", post(set_task_status))
        .route("/tasks/{id}", put(update_task).delete(delete_task))
}

/// Body for `POST /boards`. `columns` is optional — an empty list uses the
/// store's default column set.
#[derive(Debug, Deserialize)]
pub struct CreateBoard {
    pub name: String,
    #[serde(default)]
    pub columns: Vec<String>,
}

/// Body for `PUT /boards/{id}` — rename a board.
#[derive(Debug, Deserialize)]
pub struct RenameBoard {
    pub name: String,
}

/// Body for `POST /boards/{id}/tasks`.
#[derive(Debug, Deserialize)]
pub struct CreateTask {
    pub column_id: ColumnId,
    pub title: String,
    #[serde(default)]
    pub body_md: String,
}

/// Body for `POST /tasks/{id}/move`. `position` is the task's final 0-based
/// index in the destination column (clamped); absent = the end.
#[derive(Debug, Deserialize)]
pub struct MoveTask {
    pub column_id: ColumnId,
    #[serde(default)]
    pub position: Option<i32>,
}

/// Body for `POST /boards/{id}/columns` — append a column.
#[derive(Debug, Deserialize)]
pub struct AddColumn {
    pub name: String,
}

/// Body for `PUT /columns/{id}` — rename a column.
#[derive(Debug, Deserialize)]
pub struct RenameColumn {
    pub name: String,
}

/// Body for `POST /tasks/{id}/status`.
#[derive(Debug, Deserialize)]
pub struct SetTaskStatus {
    pub status: TaskStatus,
}

/// Body for `PUT /tasks/{id}` — edit a card's title + markdown body. Status,
/// column, and position are unchanged (use `/move` and `/status` for those).
#[derive(Debug, Deserialize)]
pub struct EditTask {
    pub title: String,
    #[serde(default)]
    pub body_md: String,
}

async fn list_boards(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Board>>> {
    let p = auth.principal();
    auth.require(Action::Read, "tasks")?;
    let boards = state
        .store()
        .boards()
        .list_by_workspace(p.workspace_id)
        .await?;
    Ok(Json(boards))
}

async fn create_board(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateBoard>,
) -> ApiResult<(StatusCode, Json<Board>)> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("board name must not be empty"));
    }
    // Trim + drop empty column names; an empty result lets the repo apply its
    // default column set.
    let columns: Vec<&str> = body
        .columns
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect();
    let board = state
        .store()
        .boards()
        .create(p.workspace_id, name, &columns)
        .await?;
    Ok((StatusCode::CREATED, Json(board)))
}

async fn get_board(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<BoardId>,
) -> ApiResult<Json<Board>> {
    let p = auth.principal();
    auth.require(Action::Read, "tasks")?;
    let board = state.store().boards().get(p.workspace_id, id).await?;
    Ok(Json(board))
}

/// `PUT /boards/{id}` — rename a board. Gated `tasks:write`; rejects an empty
/// name; `404` for a foreign/unknown id.
async fn rename_board(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<BoardId>,
    Json(body): Json<RenameBoard>,
) -> ApiResult<Json<Board>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("board name must not be empty"));
    }
    let board = state
        .store()
        .boards()
        .rename(p.workspace_id, id, name)
        .await?;
    Ok(Json(board))
}

/// `DELETE /boards/{id}` — delete a board and (via `ON DELETE CASCADE`) all its
/// columns + tasks. Gated `tasks:write` (managing your own board, symmetric with
/// create); `404` for a foreign/unknown id. Returns `204`.
async fn delete_board(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<BoardId>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    state.store().boards().delete(p.workspace_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_board_tasks(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<BoardId>,
) -> ApiResult<Json<Vec<Task>>> {
    let p = auth.principal();
    auth.require(Action::Read, "tasks")?;
    // Confirm the board exists in this workspace (a 404 for a foreign/unknown id,
    // not an empty list that hides the distinction).
    state.store().boards().get(p.workspace_id, id).await?;
    // The workspace-wide read is board→column→ordinal ordered; filter to this
    // board, preserving that column-then-position order.
    let tasks: Vec<Task> = state
        .store()
        .tasks()
        .list_by_workspace(p.workspace_id)
        .await?
        .into_iter()
        .filter(|t| t.board_id == id)
        .collect();
    Ok(Json(tasks))
}

async fn create_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(board_id): Path<BoardId>,
    Json(body): Json<CreateTask>,
) -> ApiResult<(StatusCode, Json<Task>)> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("task title must not be empty"));
    }
    // The repo verifies the column belongs to this board in this workspace (§18),
    // returning NotFound otherwise — so a cross-board/tenant column is rejected.
    let task = state
        .store()
        .tasks()
        .create(
            p.workspace_id,
            board_id,
            body.column_id,
            title,
            &body.body_md,
            None,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(task)))
}

/// `POST /boards/{id}/columns` — append a column to a board. Gated
/// `tasks:write`; rejects an empty name; `404` for a foreign/unknown board.
async fn add_column(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<BoardId>,
    Json(body): Json<AddColumn>,
) -> ApiResult<(StatusCode, Json<Board>)> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("column name must not be empty"));
    }
    let board = state
        .store()
        .boards()
        .add_column(p.workspace_id, id, name)
        .await?;
    Ok((StatusCode::CREATED, Json(board)))
}

/// `PUT /columns/{id}` — rename a column. Gated `tasks:write`; rejects an empty
/// name; `404` for a foreign/unknown id. Returns the updated board.
async fn rename_column(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ColumnId>,
    Json(body): Json<RenameColumn>,
) -> ApiResult<Json<Board>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("column name must not be empty"));
    }
    let board = state
        .store()
        .boards()
        .rename_column(p.workspace_id, id, name)
        .await?;
    Ok(Json(board))
}

/// `DELETE /columns/{id}` — delete an **empty** column; the repo refuses
/// (`400`) when tasks still sit in it or it is the board's only column. Gated
/// `tasks:write`; `404` for a foreign/unknown id. Returns the updated board.
async fn delete_column(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ColumnId>,
) -> ApiResult<Json<Board>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let board = state
        .store()
        .boards()
        .delete_column(p.workspace_id, id)
        .await?;
    Ok(Json(board))
}

async fn move_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<TaskId>,
    Json(body): Json<MoveTask>,
) -> ApiResult<Json<Task>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    // Capture the source column so we fire `TaskMoved` only on a real transition,
    // not a same-column re-order (§11/§24) — mirrors the `move_task` agent tool.
    let from_column = state
        .store()
        .tasks()
        .get(p.workspace_id, id)
        .await
        .ok()
        .map(|t| t.column_id);
    let task = state
        .store()
        .tasks()
        .move_to_column(p.workspace_id, id, body.column_id, body.position)
        .await?;
    // Fire any `TaskMoved` automations (SOUL §11/§24) **only when the task
    // actually entered a different column**: match by the board + destination-
    // column *names* and enqueue durable `run_automation` jobs. Best-effort — a
    // dispatch failure never fails the move itself (the same contract as the tool).
    if from_column != Some(body.column_id) {
        if let Ok(board) = state
            .store()
            .boards()
            .get(p.workspace_id, task.board_id)
            .await
        {
            if let Some(column) = board.columns.iter().find(|c| c.id == body.column_id) {
                let event = catalerum_automation::TriggerEvent::TaskMoved {
                    board: board.name.clone(),
                    to_column: column.name.clone(),
                };
                if let Err(e) =
                    catalerum_ingest::dispatch_trigger_event(state.store(), p.workspace_id, &event)
                        .await
                {
                    tracing::warn!(error = %e, "failed to dispatch TaskMoved automations (task still moved)");
                }
            }
        }
    }
    Ok(Json(task))
}

async fn set_task_status(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<TaskId>,
    Json(body): Json<SetTaskStatus>,
) -> ApiResult<Json<Task>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let task = state
        .store()
        .tasks()
        .set_status(p.workspace_id, id, body.status)
        .await?;
    Ok(Json(task))
}

/// `PUT /tasks/{id}` — edit a card's title + markdown body. Gated on
/// `tasks:write`; rejects an empty title; `404` if the task isn't in the caller's
/// workspace. Status/column/position are untouched.
async fn update_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<TaskId>,
    Json(body): Json<EditTask>,
) -> ApiResult<Json<Task>> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    let title = body.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("task title must not be empty"));
    }
    let task = state
        .store()
        .tasks()
        .update(p.workspace_id, id, title, &body.body_md)
        .await?;
    Ok(Json(task))
}

/// `DELETE /tasks/{id}` — remove a task (card) from its board. Gated on
/// `tasks:write` (deletion is a write, mirroring `DELETE /notes/{id}`); `404` if
/// the task isn't in the caller's workspace. Returns `204`.
async fn delete_task(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<TaskId>,
) -> ApiResult<StatusCode> {
    let p = auth.principal();
    auth.require(Action::Write, "tasks")?;
    state.store().tasks().delete(p.workspace_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_board_defaults_empty_columns() {
        let b: CreateBoard = serde_json::from_str(r#"{"name":"Roadmap"}"#).unwrap();
        assert_eq!(b.name, "Roadmap");
        assert!(b.columns.is_empty());
        let b2: CreateBoard = serde_json::from_str(r#"{"name":"x","columns":["A","B"]}"#).unwrap();
        assert_eq!(b2.columns, vec!["A".to_string(), "B".to_string()]);
    }

    #[test]
    fn create_task_body_md_defaults_empty() {
        let t: CreateTask = serde_json::from_str(
            r#"{"column_id":"00000000-0000-0000-0000-000000000000","title":"Ship it"}"#,
        )
        .unwrap();
        assert_eq!(t.title, "Ship it");
        assert!(t.body_md.is_empty());
    }

    #[test]
    fn move_task_position_defaults_none() {
        let m: MoveTask =
            serde_json::from_str(r#"{"column_id":"00000000-0000-0000-0000-000000000000"}"#)
                .unwrap();
        assert!(m.position.is_none());
        let m2: MoveTask = serde_json::from_str(
            r#"{"column_id":"00000000-0000-0000-0000-000000000000","position":0}"#,
        )
        .unwrap();
        assert_eq!(m2.position, Some(0));
    }

    #[test]
    fn set_task_status_parses_snake_case() {
        let s: SetTaskStatus = serde_json::from_str(r#"{"status":"in_progress"}"#).unwrap();
        assert_eq!(s.status, TaskStatus::InProgress);
        let s2: SetTaskStatus = serde_json::from_str(r#"{"status":"done"}"#).unwrap();
        assert_eq!(s2.status, TaskStatus::Done);
    }
}
