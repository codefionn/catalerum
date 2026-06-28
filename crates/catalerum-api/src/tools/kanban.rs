//! Board/task tools (SOUL §24).

use super::*;

/// `read_task` (SOUL §7/§24) — read a Kanban task's full detail by id; the read
/// twin of `query_structured`'s task summaries (which omit the body). Thin
/// `TaskRepo`/`BoardRepo` client, gated on `tasks:read`.
pub(crate) struct ReadTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReadTaskTool {
    fn name(&self) -> &str {
        "kanban_read_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "tasks")
    }
    fn description(&self) -> &str {
        "Read a Kanban task's full detail by its id (the `id` from query_structured \
         open_tasks/tasks_by_status/tasks_by_board or kanban_next_task): its title, full \
         **description (body)**, status, board/column, and assignee. Use to read a \
         task's body — query_structured returns only summaries (no body)."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Task id (a UUID from query_structured)." }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id: TaskId = parse_id(&args, "id")?;
        // Confirm the task is in THIS workspace (NotFound never leaks another tenant's task).
        let task = self.store.tasks().get(ws, id).await?;
        // Resolve board/column names so the result reads like a task summary + body
        // (mirrors how query_structured indexes column_id → names).
        let boards = self
            .store
            .boards()
            .list_by_workspace(ws)
            .await
            .map_err(query_err)?;
        let mut col_index: std::collections::HashMap<ColumnId, (String, String)> =
            std::collections::HashMap::new();
        for b in &boards {
            for c in &b.columns {
                col_index.insert(c.id, (b.name.clone(), c.name.clone()));
            }
        }
        let body = task.body_md.clone();
        let mut detail = task_summary(task, &col_index);
        if let Json::Object(map) = &mut detail {
            map.insert("body".into(), json!(body));
        }
        Ok(detail)
    }
}

/// `search_tasks` (SOUL §7/§24) — literal full-text search over Kanban tasks'
/// title + body; the content-search complement to `query_structured`'s
/// status/board *filtering*. Thin `TaskRepo`/`BoardRepo` client, gated on
/// `tasks:read`, workspace-scoped.
pub(crate) struct SearchTasksTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchTasksTool {
    fn name(&self) -> &str {
        "kanban_search_tasks"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "tasks")
    }
    fn description(&self) -> &str {
        "Find Kanban tasks by the text in their title or description — a literal, \
         case-insensitive substring search. Use it to locate a task by what it says \
         (e.g. \"the task about the migration\"); use query_structured to list tasks \
         by status or board. Each hit gives the task id, title, status, board/column, \
         assignee, and a match-centred snippet of the body; most-recently-updated \
         first."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Exact text to find in a task's title or body (case-insensitive substring)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;
        let tasks = self
            .store
            .tasks()
            .search_in_workspace(ws, &query, limit)
            .await
            .map_err(|e| Error::provider(format!("task search failed: {e}")))?;
        // Resolve board/column names once (mirrors query_structured / read_task).
        let boards = self
            .store
            .boards()
            .list_by_workspace(ws)
            .await
            .map_err(query_err)?;
        let mut col_index: std::collections::HashMap<ColumnId, (String, String)> =
            std::collections::HashMap::new();
        for b in &boards {
            for c in &b.columns {
                col_index.insert(c.id, (b.name.clone(), c.name.clone()));
            }
        }
        let results: Vec<Json> = tasks
            .into_iter()
            .map(|t| {
                let snippet = match_snippet(&t.body_md, &query, MESSAGE_SNIPPET_CHARS);
                let mut summary = task_summary(t, &col_index);
                if let Json::Object(map) = &mut summary {
                    map.insert("snippet".into(), json!(snippet));
                }
                summary
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}

/// A compact task view for tool results: the task plus its board + column names
/// (resolved via `col_index`), so the model sees *where* a task sits, not ids.
pub(crate) fn task_summary(
    t: catalerum_core::Task,
    col_index: &std::collections::HashMap<ColumnId, (String, String)>,
) -> Json {
    let (board, column) = col_index
        .get(&t.column_id)
        .cloned()
        .unwrap_or_else(|| (String::new(), String::new()));
    json!({
        "id": t.id,
        "title": t.title,
        "status": t.status,
        "board": board,
        "column": column,
        "assignee": t.assignee,
    })
}

/// `create_task` — add a card to a Kanban column (SOUL §24). Defaults to the
/// board's first column when `column_id` is omitted.
/// Resolve the Kanban board a tool's args name: `board_id` (a UUID,
/// authoritative when both are given) or `board` (a case-insensitive name) —
/// so the model can address a board the way a user talks about it, without a
/// `query_structured` round-trip first. An unknown name errors **with the
/// workspace's board names** so the model can self-correct in one step; a
/// duplicate name errors asking for the id.
pub(crate) async fn resolve_board_arg(
    store: &Store,
    ws: WorkspaceId,
    args: &Json,
) -> Result<Board> {
    if args.get("board_id").and_then(Json::as_str).is_some() {
        let id: BoardId = parse_id(args, "board_id")?;
        return Ok(store.boards().get(ws, id).await?);
    }
    let Some(name) = opt_str_some(args, "board") else {
        return Err(Error::invalid(
            "pass `board` (a board name) or `board_id` (a UUID)",
        ));
    };
    let boards = store
        .boards()
        .list_by_workspace(ws)
        .await
        .map_err(query_err)?;
    let mut hits: Vec<Board> = boards
        .iter()
        .filter(|b| b.name.eq_ignore_ascii_case(&name))
        .cloned()
        .collect();
    match hits.len() {
        1 => Ok(hits.remove(0)),
        0 => {
            let known: Vec<&str> = boards.iter().map(|b| b.name.as_str()).take(25).collect();
            Err(Error::invalid(if known.is_empty() {
                format!("no board named `{name}` — there are no boards yet (kanban_create_board makes one)")
            } else {
                format!("no board named `{name}` (boards: {})", known.join(", "))
            }))
        }
        _ => Err(Error::invalid(format!(
            "several boards are named `{name}` — pass `board_id` instead (ids: {})",
            hits.iter()
                .map(|b| b.id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Resolve a column within `board` from a tool's `column_id` / `column`
/// (case-insensitive name) args; `Ok(None)` when neither is given. A name not
/// on the board errors **with the board's column names** (same self-correction
/// contract as [`resolve_board_arg`]).
pub(crate) fn resolve_column_arg(board: &Board, args: &Json) -> Result<Option<ColumnId>> {
    if args.get("column_id").and_then(Json::as_str).is_some() {
        return Ok(Some(parse_id(args, "column_id")?));
    }
    let Some(name) = opt_str_some(args, "column") else {
        return Ok(None);
    };
    board
        .columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(&name))
        .map(|c| Some(c.id))
        .ok_or_else(|| {
            Error::invalid(format!(
                "no column named `{name}` on board `{}` (columns: {})",
                board.name,
                board
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// `kanban_create_board` — create a Kanban board with an optional custom column
/// set (SOUL §24): the write twin of `query_structured`'s `boards` read and the
/// agent-facing side of `POST /boards`. Gated `tasks:write`.
pub(crate) struct CreateBoardTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateBoardTool {
    fn name(&self) -> &str {
        "kanban_create_board"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Create a Kanban board. Optionally pass `columns` (ordered column names; \
         default: Backlog, To-do, Doing, Done). Returns the board with its columns \
         (names + ids) — kanban_create_task / kanban_move_task can then address it \
         by name."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Board name (required, non-empty)." },
                "columns": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Ordered column names (optional; defaults to Backlog, To-do, Doing, Done)."
                }
            },
            "required": ["name"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let name = required_str(&args, "name")?;
        let columns = opt_str_vec(&args, "columns");
        let cols: Vec<&str> = columns.iter().map(String::as_str).collect();
        let board = self.store.boards().create(ws, &name, &cols).await?;
        Ok(serde_json::to_value(board)?)
    }
}

pub(crate) struct CreateTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateTaskTool {
    fn name(&self) -> &str {
        "kanban_create_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Create a task (card) on a Kanban board. Name the board with `board` (its \
         name, e.g. \"Sprint\") or `board_id`; pick the column with `column` (its \
         name, e.g. \"Doing\") or `column_id`, or omit both for the board's first \
         column. `body` is optional markdown detail."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "board": { "type": "string", "description": "Board name (case-insensitive). Either this or board_id." },
                "board_id": { "type": "string", "description": "Board id (a UUID); wins over `board` when both are given." },
                "column": { "type": "string", "description": "Column name on that board (optional; defaults to the first column)." },
                "column_id": { "type": "string", "description": "Column id (optional alternative to `column`)." },
                "title": { "type": "string", "description": "Task title (required, non-empty)." },
                "body": { "type": "string", "description": "Markdown body (optional)." }
            },
            "required": ["title"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let title = required_str(&args, "title")?;
        let body = opt_str(&args, "body");
        let board = resolve_board_arg(&self.store, ws, &args).await?;
        let column_id = match resolve_column_arg(&board, &args)? {
            Some(id) => id,
            None => board
                .columns
                .first()
                .map(|c| c.id)
                .ok_or_else(|| Error::invalid("board has no columns"))?,
        };
        let task = self
            .store
            .tasks()
            .create(ws, board.id, column_id, &title, &body, None)
            .await?;
        Ok(serde_json::to_value(task)?)
    }
}

/// `move_task` — move a task to another column on its board (SOUL §24).
pub(crate) struct MoveTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for MoveTaskTool {
    fn name(&self) -> &str {
        "kanban_move_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Move a task to a different column on its board — name the destination with \
         `column` (e.g. \"Doing\") or `column_id`. Optional `position` places the \
         card at that 0-based index in the column (0 = top; omitted = bottom); a \
         same-column move with a `position` reorders the card within its column."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task to move." },
                "column": { "type": "string", "description": "Destination column name on the task's board (case-insensitive). Either this or column_id." },
                "column_id": { "type": "string", "description": "Destination column id; wins over `column` when both are given." },
                "position": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Final 0-based index in the destination column (0 = top; clamped; omitted = bottom)."
                }
            },
            "required": ["task_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let task_id: TaskId = parse_id(&args, "task_id")?;
        // The task's board scopes the destination-column (name) lookup, tells us
        // the source column (so `TaskMoved` fires only on a real transition, not
        // a same-column re-order, §11/§24), and carries the names the dispatch
        // matches on.
        let before = self.store.tasks().get(ws, task_id).await?;
        let board = self.store.boards().get(ws, before.board_id).await?;
        let column_id = resolve_column_arg(&board, &args)?.ok_or_else(|| {
            Error::invalid("pass `column` (a column name on the task's board) or `column_id`")
        })?;
        let position = args
            .get("position")
            .and_then(Json::as_i64)
            .map(|p| p.clamp(0, i64::from(i32::MAX)) as i32);
        let task = self
            .store
            .tasks()
            .move_to_column(ws, task_id, column_id, position)
            .await?;
        // Fire any `TaskMoved` automations (SOUL §11/§24) **only when the task
        // actually entered a different column**: match by the board +
        // destination-column *names* and enqueue durable `run_automation` jobs.
        // Best-effort — a dispatch failure never fails the move itself.
        if before.column_id != column_id {
            if let Some(column) = board.columns.iter().find(|c| c.id == column_id) {
                let event = catalerum_automation::TriggerEvent::TaskMoved {
                    board: board.name.clone(),
                    to_column: column.name.clone(),
                };
                if let Err(e) =
                    catalerum_ingest::dispatch_trigger_event(&self.store, ws, &event).await
                {
                    tracing::warn!(error = %e, "failed to dispatch TaskMoved automations (task still moved)");
                }
            }
        }
        Ok(serde_json::to_value(task)?)
    }
}

/// `complete_task` — mark a task done (SOUL §24).
pub(crate) struct CompleteTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CompleteTaskTool {
    fn name(&self) -> &str {
        "kanban_complete_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Mark a task as done."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string", "description": "Task to complete." } },
            "required": ["task_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let task_id: TaskId = parse_id(&args, "task_id")?;
        let task = self
            .store
            .tasks()
            .set_status(ws, task_id, catalerum_core::model::TaskStatus::Done)
            .await?;
        Ok(serde_json::to_value(task)?)
    }
}

/// `set_task_status` — set a task's lifecycle status to any of open /
/// in_progress / blocked / done (SOUL §24). The general form behind the
/// `POST /tasks/{id}/status` route; `complete_task` is the `done` shorthand.
/// Lets the agent reflect a task as started or blocked, not only finished.
pub(crate) struct SetTaskStatusTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SetTaskStatusTool {
    fn name(&self) -> &str {
        "kanban_set_task_status"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Set a task's status: 'open', 'in_progress', 'blocked', or 'done'. Use this \
         to mark a task started or blocked; 'kanban_complete_task' is the 'done' shortcut."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task to update." },
                "status": {
                    "type": "string",
                    "enum": ["open", "in_progress", "blocked", "done"],
                    "description": "The new lifecycle status."
                }
            },
            "required": ["task_id", "status"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let task_id: TaskId = parse_id(&args, "task_id")?;
        let status = parse_task_status(&required_str(&args, "status")?)?;
        let task = self.store.tasks().set_status(ws, task_id, status).await?;
        Ok(serde_json::to_value(task)?)
    }
}

/// `delete_task` — remove a task (card) from its board by id (SOUL §24). The
/// agent-facing side of `DELETE /tasks/{id}`, gated on `tasks:write` (deletion is
/// a write, like `delete_note`). Use to clear a mistaken or obsolete card — to
/// just finish one, use `complete_task`.
pub(crate) struct DeleteTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DeleteTaskTool {
    fn name(&self) -> &str {
        "kanban_delete_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Delete a task (card) from its board by id. Use to remove a mistaken or \
         obsolete task; to just mark one finished, use kanban_complete_task instead."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": { "task_id": { "type": "string", "description": "Task to delete." } },
            "required": ["task_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let task_id: TaskId = parse_id(&args, "task_id")?;
        self.store.tasks().delete(ws, task_id).await?;
        Ok(json!({ "deleted": task_id }))
    }
}

/// `edit_task` — change a task's title + markdown body by id (SOUL §24). The
/// agent-facing side of `PUT /tasks/{id}` (mirrors `edit_note`), gated on
/// `tasks:write`. Fix a typo or flesh out a card without recreating it; status
/// and column are untouched (use `set_task_status` / `move_task` for those).
pub(crate) struct EditTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for EditTaskTool {
    fn name(&self) -> &str {
        "kanban_edit_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "tasks")
    }
    fn description(&self) -> &str {
        "Edit a task's title and/or markdown body by id — a field you omit keeps \
         its current value (status and column are unchanged). Use to correct or \
         expand a card rather than recreating it."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "description": "Task to edit." },
                "title": { "type": "string", "description": "New title (optional; non-empty when given)." },
                "body": { "type": "string", "description": "New markdown body (optional; an empty string clears it)." }
            },
            "required": ["task_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let task_id: TaskId = parse_id(&args, "task_id")?;
        if args
            .get("title")
            .and_then(Json::as_str)
            .is_some_and(|s| s.trim().is_empty())
        {
            return Err(Error::invalid("`title` must not be empty"));
        }
        let title = opt_str_some(&args, "title");
        // Unlike `title`, an explicit empty `body` is meaningful (clear the body),
        // so presence is checked raw rather than via the blank-is-absent helper.
        let body = args.get("body").and_then(Json::as_str).map(str::to_string);
        if title.is_none() && body.is_none() {
            return Err(Error::invalid(
                "pass `title` and/or `body` — nothing to change",
            ));
        }
        let current = self.store.tasks().get(ws, task_id).await?;
        let task = self
            .store
            .tasks()
            .update(
                ws,
                task_id,
                title.as_deref().unwrap_or(&current.title),
                body.as_deref().unwrap_or(&current.body_md),
            )
            .await?;
        Ok(serde_json::to_value(task)?)
    }
}

/// `next_task` — the next task to work in a column (lowest order, not done),
/// or `null` if the column is empty/all done (SOUL §24).
pub(crate) struct NextTaskTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for NextTaskTool {
    fn name(&self) -> &str {
        "kanban_next_task"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "tasks")
    }
    fn description(&self) -> &str {
        "Get the next task to work in a column (lowest position, not yet done), or \
         null if there is none. Name the column with `board` + `column` (their \
         names), or pass a `column_id`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "board": { "type": "string", "description": "Board name (with `column`); alternative to column_id." },
                "column": { "type": "string", "description": "Column name on that board (with `board`)." },
                "column_id": { "type": "string", "description": "Column id; wins over the names when given." }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let column_id: ColumnId = if args.get("column_id").and_then(Json::as_str).is_some() {
            parse_id(&args, "column_id")?
        } else {
            let board = resolve_board_arg(&self.store, ws, &args).await?;
            resolve_column_arg(&board, &args)?
                .ok_or_else(|| Error::invalid("pass `column` (with `board`) or `column_id`"))?
        };
        let next = self.store.tasks().next_in_column(ws, column_id).await?;
        Ok(match next {
            Some(task) => serde_json::to_value(task)?,
            None => json!(null),
        })
    }
}
