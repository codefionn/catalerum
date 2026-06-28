//! Note CRUD tools (SOUL §21).

use super::*;

/// `create_note` — author a new markdown note in the caller's workspace.
pub(crate) struct CreateNoteTool {
    pub(crate) notes: NoteRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for CreateNoteTool {
    fn name(&self) -> &str {
        "create_note"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "notes")
    }

    fn description(&self) -> &str {
        "Create a markdown note (e.g. a list, journal entry, or meeting notes) in \
         the user's workspace. Returns the stored note including its id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Note title (required, non-empty)." },
                "markdown": { "type": "string", "description": "Markdown body. Optional." },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional free-text tags."
                }
            },
            "required": ["title"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let author = author(ctx)?;
        let title = required_str(&args, "title")?;
        let markdown = opt_str(&args, "markdown");
        let tags = opt_tags(&args);
        let note = self
            .notes
            .create(ws, author, &title, &markdown, &tags)
            .await?;
        self.ingest.enqueue(ws, note.id).await;
        Ok(serde_json::to_value(note)?)
    }
}

/// `edit_note` — replace a note's title / markdown / tags (workspace-scoped).
pub(crate) struct EditNoteTool {
    pub(crate) notes: NoteRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for EditNoteTool {
    fn name(&self) -> &str {
        "edit_note"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "notes")
    }

    fn description(&self) -> &str {
        "Update an existing note's title, markdown body, and tags by id. Replaces \
         all three fields; the author is immutable. Returns the updated note."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the note to edit." },
                "title": { "type": "string", "description": "New title (required, non-empty)." },
                "markdown": { "type": "string", "description": "New markdown body. Optional (defaults to empty)." },
                "tags": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "New tag set (replaces existing). Optional."
                }
            },
            "required": ["id", "title"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = note_id(&args)?;
        let title = required_str(&args, "title")?;
        let markdown = opt_str(&args, "markdown");
        let tags = opt_tags(&args);
        let note = self.notes.update(ws, id, &title, &markdown, &tags).await?;
        self.ingest.enqueue(ws, note.id).await;
        Ok(serde_json::to_value(note)?)
    }
}

/// `delete_note` — remove a note by id (SOUL §21/§7). Completes the agent's note
/// CRUD set (`create_note`/`edit_note`/`read_note`/`list_notes`) at parity with the
/// `DELETE /notes/{id}` route and the workbench's delete control. Gated on
/// `notes:write` (the same authority the route requires — note deletion is a write,
/// not a separate `delete` scope). After the row is gone it reconciles the derived
/// projection so search/graph data doesn't dangle.
pub(crate) struct DeleteNoteTool {
    pub(crate) notes: NoteRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for DeleteNoteTool {
    fn name(&self) -> &str {
        "delete_note"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "notes")
    }

    fn description(&self) -> &str {
        "Delete a note by id. Its derived search/graph projections are reconciled \
         away. Returns the deleted note's id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the note to delete." }
            },
            "required": ["id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = note_id(&args)?;
        self.notes.delete(ws, id).await?;
        // Reconcile the derived projection: the worker finds the note gone and
        // purges its vectors/chunks/document (best-effort; no-op unless enabled).
        self.ingest.enqueue(ws, id).await;
        Ok(json!({ "deleted": id }))
    }
}

/// `read_note` — fetch a single note (with its markdown body) by id.
pub(crate) struct ReadNoteTool {
    pub(crate) notes: NoteRepo,
}

#[async_trait]
impl Tool for ReadNoteTool {
    fn name(&self) -> &str {
        "read_note"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "notes")
    }

    fn description(&self) -> &str {
        "Fetch one note by id, including its full markdown body."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the note to read." }
            },
            "required": ["id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = note_id(&args)?;
        let note = self.notes.get(ws, id).await?;
        Ok(serde_json::to_value(note)?)
    }
}

/// `list_notes` — list the workspace's notes (compact: no markdown body).
pub(crate) struct ListNotesTool {
    pub(crate) notes: NoteRepo,
}

#[async_trait]
impl Tool for ListNotesTool {
    fn name(&self) -> &str {
        "list_notes"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "notes")
    }

    fn description(&self) -> &str {
        "List the workspace's notes (most-recently-edited first) as id / title / \
         tags / updated_at. Use read_note to fetch a note's body; search_files / \
         search_messages-style search isn't here — use query_structured notes_by_tag."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max notes to return, newest-first (1-200, default 50).",
                    "minimum": 1,
                    "maximum": 200
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let limit = opt_clamped_u64(&args, "limit", 50, 200) as i64;
        let notes = self.notes.list_by_workspace(ws, limit).await?;
        // Keep the result cheap in context: omit the markdown bodies.
        let summaries: Vec<Json> = notes
            .into_iter()
            .map(|n| {
                json!({
                    "id": n.id,
                    "title": n.title,
                    "tags": n.tags,
                    "updated_at": n.updated_at,
                })
            })
            .collect();
        Ok(json!({ "notes": summaries }))
    }
}

// ---------------------------------------------------------------------------
// Links — relationships between objects (SOUL §5/§6.3).
// ---------------------------------------------------------------------------
