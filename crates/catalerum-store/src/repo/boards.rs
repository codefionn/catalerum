use catalerum_core::id::{BoardId, ColumnId, TaskId, WorkspaceId};
use catalerum_core::model::{Author, Board, Task, TaskStatus};
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{
    author_to_parts, board_from_parts, BoardRow, ColumnRow, task_status_to_text, TaskRow,
};
use super::{map, PgPool};

// ===========================================================================
// Tasks & Kanban board (SOUL §24)
// ===========================================================================

const BOARD_COLS: &str = "id, workspace_id, name, created_at, updated_at";
const COLUMN_COLS: &str = "id, board_id, name, ordinal";
const TASK_COLS: &str = "id, workspace_id, board_id, column_id, title, body_md, \
    assignee_kind, assignee_id, ordinal, status, created_at, updated_at";

/// The default Kanban columns when a board is created without an explicit set
/// (SOUL §24).
pub const DEFAULT_COLUMNS: &[&str] = &["Backlog", "To-do", "Doing", "Done"];

/// CRUD for `boards` + `board_columns` (SOUL §24). A board owns an ordered set of
/// columns; tasks live in columns ([`TaskRepo`]).
#[derive(Clone, Debug)]
pub struct BoardRepo {
    pool: PgPool,
}

impl BoardRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a board with its columns (in order). An empty `columns` uses
    /// [`DEFAULT_COLUMNS`]. Returns the assembled [`Board`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        columns: &[&str],
    ) -> Result<Board> {
        let names: Vec<&str> = if columns.is_empty() {
            DEFAULT_COLUMNS.to_vec()
        } else {
            columns.to_vec()
        };
        let mut tx = self.pool.begin().await.map_err(map)?;
        let board: BoardRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO boards (id, workspace_id, name) VALUES ($1, $2, $3)
             RETURNING {BOARD_COLS}"
        ))
        .bind(BoardId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let mut column_rows = Vec::with_capacity(names.len());
        for (ordinal, col_name) in names.iter().enumerate() {
            let col: ColumnRow = sqlx::query_as(assert_sql_safe!(
                "INSERT INTO board_columns (id, workspace_id, board_id, name, ordinal)
                 VALUES ($1, $2, $3, $4, $5)
                 RETURNING {COLUMN_COLS}"
            ))
            .bind(ColumnId::new().into_uuid())
            .bind(workspace_id.into_uuid())
            .bind(board.id)
            .bind(*col_name)
            .bind(ordinal as i32)
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
            column_rows.push(col);
        }
        tx.commit().await.map_err(map)?;
        Ok(board_from_parts(board, column_rows))
    }

    /// Fetch a board with its columns (ordered), workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: BoardId) -> Result<Board> {
        let board: BoardRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {BOARD_COLS} FROM boards WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// List a workspace's boards (with columns), by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Board>> {
        let boards: Vec<BoardRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {BOARD_COLS} FROM boards WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        let mut out = Vec::with_capacity(boards.len());
        for b in boards {
            let columns = self.columns_of(BoardId::from_uuid(b.id)).await?;
            out.push(board_from_parts(b, columns));
        }
        Ok(out)
    }

    /// Append a column to a board (at the next ordinal), workspace-scoped.
    /// Returns the updated board. `NotFound` if no such board.
    pub async fn add_column(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        name: &str,
    ) -> Result<Board> {
        // Confirm the board exists in this workspace first (NotFound, not an FK error).
        let board: BoardRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {BOARD_COLS} FROM boards WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(board_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        sqlx::query(
            "INSERT INTO board_columns (id, workspace_id, board_id, name, ordinal)
             VALUES ($1, $2, $3, $4,
                     COALESCE((SELECT max(ordinal) + 1 FROM board_columns WHERE board_id = $3), 0))",
        )
        .bind(ColumnId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(board_id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// Rename a column (workspace-scoped). Returns its updated board.
    /// `NotFound` if no such column.
    pub async fn rename_column(
        &self,
        workspace_id: WorkspaceId,
        id: ColumnId,
        name: &str,
    ) -> Result<Board> {
        let col: ColumnRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE board_columns SET name = $3 WHERE id = $1 AND workspace_id = $2
             RETURNING {COLUMN_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        self.get(workspace_id, BoardId::from_uuid(col.board_id))
            .await
    }

    /// Delete an **empty** column (workspace-scoped). Returns the updated board.
    /// `Invalid` when tasks still sit in it (move or delete them first) or when
    /// it is the board's only column; `NotFound` if no such column.
    pub async fn delete_column(&self, workspace_id: WorkspaceId, id: ColumnId) -> Result<Board> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        #[cfg(not(feature = "sqlite"))]
        let row_lock = "FOR UPDATE";
        #[cfg(feature = "sqlite")]
        let row_lock = "";
        let col: ColumnRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {COLUMN_COLS} FROM board_columns
             WHERE id = $1 AND workspace_id = $2 {row_lock}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;
        let (tasks,): (i64,) = sqlx::query_as("SELECT count(*) FROM tasks WHERE column_id = $1")
            .bind(id.into_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(map)?;
        if tasks > 0 {
            return Err(StoreError::Invalid(format!(
                "column `{}` still has {tasks} task(s) — move or delete them first",
                col.name
            )));
        }
        let (siblings,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM board_columns WHERE board_id = $1")
                .bind(col.board_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(map)?;
        if siblings <= 1 {
            return Err(StoreError::Invalid(
                "a board needs at least one column".to_string(),
            ));
        }
        sqlx::query("DELETE FROM board_columns WHERE id = $1")
            .bind(id.into_uuid())
            .execute(&mut *tx)
            .await
            .map_err(map)?;
        tx.commit().await.map_err(map)?;
        self.get(workspace_id, BoardId::from_uuid(col.board_id))
            .await
    }

    /// Rename a board (workspace-scoped). Returns the updated board with its
    /// columns. `NotFound` if no such board.
    pub async fn rename(
        &self,
        workspace_id: WorkspaceId,
        id: BoardId,
        name: &str,
    ) -> Result<Board> {
        let board: BoardRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE boards SET name = $3 WHERE id = $1 AND workspace_id = $2 RETURNING {BOARD_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        let columns = self.columns_of(id).await?;
        Ok(board_from_parts(board, columns))
    }

    /// Delete a board by id (workspace-scoped); its columns + tasks cascade via
    /// the `0008` `ON DELETE CASCADE` FKs. `NotFound` if no such board.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: BoardId) -> Result<()> {
        let res = sqlx::query("DELETE FROM boards WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    async fn columns_of(&self, board_id: BoardId) -> Result<Vec<ColumnRow>> {
        sqlx::query_as(assert_sql_safe!(
            "SELECT {COLUMN_COLS} FROM board_columns WHERE board_id = $1 ORDER BY ordinal ASC"
        ))
        .bind(board_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)
    }
}

/// CRUD for the `tasks` table (SOUL §24). Tasks are created in a column and
/// worked one-by-one: pull `next`, do the work, `complete`.
#[derive(Clone, Debug)]
pub struct TaskRepo {
    pool: PgPool,
}

impl TaskRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a task at the end of `column_id` (next `ordinal`), status `open`.
    /// The column is verified to belong to `board_id` in `workspace_id` first.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        column_id: ColumnId,
        title: &str,
        body_md: &str,
        assignee: Option<Author>,
    ) -> Result<Task> {
        self.verify_column(workspace_id, board_id, column_id)
            .await?;
        let (assignee_kind, assignee_id) = match assignee {
            Some(a) => {
                let (k, i) = author_to_parts(a);
                (Some(k), Some(i))
            }
            None => (None, None),
        };
        let row: TaskRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO tasks
                 (id, workspace_id, board_id, column_id, title, body_md, assignee_kind,
                  assignee_id, ordinal, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8,
                     COALESCE((SELECT max(ordinal) + 1 FROM tasks WHERE column_id = $4), 0),
                     'open')
             RETURNING {TASK_COLS}"
        ))
        .bind(TaskId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(column_id.into_uuid())
        .bind(title)
        .bind(body_md)
        .bind(assignee_kind)
        .bind(assignee_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a task by id, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn get(&self, workspace_id: WorkspaceId, id: TaskId) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {TASK_COLS} FROM tasks WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Search tasks by the **content** of their title or body (SOUL §24/§6.1) —
    /// `query` matched as a **literal case-insensitive substring** (`strpos`, not
    /// `LIKE`, so a user's `%`/`_` are literal), workspace-scoped, most-recently-
    /// updated first, `LIMIT`-bounded (floored at 1). A blank query returns nothing
    /// (no "match everything"). The literal complement to `list_by_workspace`'s
    /// status/board filtering — drives the `search_tasks` agent tool.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Task>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<TaskRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1
               AND (strpos(lower(title), lower($2)) > 0 OR strpos(lower(body_md), lower($2)) > 0)
             ORDER BY updated_at DESC, id DESC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// Move a task to `to_column`, workspace-scoped. The target column must be
    /// in the same board. `position` is the task's final 0-based index among the
    /// column's tasks (clamped; `None` = the end) — so `Some(0)` puts it on top,
    /// and a same-column move with a `position` is a within-column reorder.
    /// Returns the updated task.
    pub async fn move_to_column(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        to_column: ColumnId,
        position: Option<i32>,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE tasks t SET
                 column_id = $3,
                 ordinal = COALESCE((SELECT max(ordinal) + 1 FROM tasks WHERE column_id = $3), 0),
                 updated_at = CURRENT_TIMESTAMP
             WHERE t.id = $1 AND t.workspace_id = $2
               AND EXISTS (SELECT 1 FROM board_columns c
                           WHERE c.id = $3 AND c.board_id = t.board_id)
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(to_column.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?
        .ok_or(StoreError::NotFound)?;
        match position {
            None => row.try_into(),
            Some(p) => self.place_at(workspace_id, id, to_column, p).await,
        }
    }

    /// Renumber `column_id` densely (0..n) with task `id` at the clamped
    /// `position`. The task must already sit in the column (`move_to_column`
    /// puts it there first). Row-locked so two concurrent placements can't
    /// interleave their renumbering.
    async fn place_at(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        column_id: ColumnId,
        position: i32,
    ) -> Result<Task> {
        let mut tx = self.pool.begin().await.map_err(map)?;
        #[cfg(not(feature = "sqlite"))]
        let row_lock = "FOR UPDATE";
        #[cfg(feature = "sqlite")]
        let row_lock = "";
        let siblings: Vec<(Uuid,)> = sqlx::query_as(assert_sql_safe!(
            "SELECT id FROM tasks
             WHERE workspace_id = $1 AND column_id = $2 AND id <> $3
             ORDER BY ordinal ASC, id ASC
             {row_lock}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .bind(id.into_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(map)?;
        let mut ordered: Vec<Uuid> = siblings.into_iter().map(|(u,)| u).collect();
        let pos = usize::try_from(position.max(0))
            .unwrap_or(0)
            .min(ordered.len());
        ordered.insert(pos, id.into_uuid());
        for (ordinal, task) in ordered.iter().enumerate() {
            sqlx::query("UPDATE tasks SET ordinal = $3 WHERE id = $1 AND workspace_id = $2")
                .bind(task)
                .bind(workspace_id.into_uuid())
                .bind(ordinal as i32)
                .execute(&mut *tx)
                .await
                .map_err(map)?;
        }
        tx.commit().await.map_err(map)?;
        self.get(workspace_id, id).await
    }

    /// The next task to work in a column: the lowest-`ordinal` task not yet
    /// `done` (SOUL §24). `None` if the column is empty/all done.
    pub async fn next_in_column(
        &self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
    ) -> Result<Option<Task>> {
        let row: Option<TaskRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1 AND column_id = $2 AND status <> 'done'
             ORDER BY ordinal ASC, id ASC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        row.map(Task::try_from).transpose()
    }

    /// Set a task's `status` (workspace-scoped). [`StoreError::NotFound`] if
    /// absent. Used by `complete` (status = `done`).
    pub async fn set_status(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        status: TaskStatus,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE tasks SET status = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(task_status_to_text(status))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Update a task's title + markdown body (workspace-scoped). `NotFound` if no
    /// such task. Status / column / ordinal are untouched — this is the card-edit
    /// counterpart to `set_status` / `move_to_column`.
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: TaskId,
        title: &str,
        body_md: &str,
    ) -> Result<Task> {
        let row: TaskRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE tasks SET title = $3, body_md = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {TASK_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(body_md)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a task by id (workspace-scoped). `NotFound` if no such task. No row
    /// references `tasks(id)`, so this is a plain single-row delete (no cascade).
    pub async fn delete(&self, workspace_id: WorkspaceId, id: TaskId) -> Result<()> {
        let res = sqlx::query("DELETE FROM tasks WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// List a column's tasks in order, workspace-scoped.
    pub async fn list_by_column(
        &self,
        workspace_id: WorkspaceId,
        column_id: ColumnId,
    ) -> Result<Vec<Task>> {
        let rows: Vec<TaskRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TASK_COLS} FROM tasks
             WHERE workspace_id = $1 AND column_id = $2
             ORDER BY ordinal ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(column_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// List **all** of a workspace's tasks, board-grouped then in-column order —
    /// the workspace-wide read backing `query_structured`'s task lookups (§6.5/§24).
    /// Ordered by `board_id`, then column `ordinal`, then task `ordinal` for a
    /// stable, board-by-board, top-of-column-first sequence.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Task>> {
        let rows: Vec<TaskRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {TASK_COLS} FROM tasks t
             WHERE t.workspace_id = $1
             ORDER BY t.board_id ASC,
                      (SELECT c.ordinal FROM board_columns c WHERE c.id = t.column_id) ASC,
                      t.ordinal ASC, t.id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Task::try_from).collect()
    }

    /// Verify `column_id` belongs to `board_id` within `workspace_id`, or
    /// [`StoreError::NotFound`] — so a task can't be created across the tenancy
    /// boundary or in a foreign board (§18).
    async fn verify_column(
        &self,
        workspace_id: WorkspaceId,
        board_id: BoardId,
        column_id: ColumnId,
    ) -> Result<()> {
        let found: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM board_columns
             WHERE id = $1 AND board_id = $2 AND workspace_id = $3",
        )
        .bind(column_id.into_uuid())
        .bind(board_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        if found.is_none() {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
