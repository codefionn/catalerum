//! Link tools (SOUL §5/§6.3).

use super::*;

/// The JSON-schema fragment for a link endpoint — a tagged `{kind, id}` object.
/// Shared by `create_link`/`list_links` so both describe endpoints identically.
pub(crate) fn endpoint_schema(description: &str) -> Json {
    json!({
        "type": "object",
        "description": description,
        "properties": {
            "kind": {
                "type": "string",
                "description": "The object kind: note (markdown note), event (calendar event), \
                    object (stored file), email, memory, document, message, or external (a uri).",
                "enum": ["note", "event", "object", "email", "memory", "document", "message", "external"]
            },
            "id": {
                "type": "string",
                "description": "The object's id — a uuid for stored rows; a uri when kind is \"external\"."
            }
        },
        "required": ["kind", "id"]
    })
}

/// `create_link` — relate two objects with a directed `RELATES_TO` link (SOUL §6.3).
pub(crate) struct CreateLinkTool {
    pub(crate) links: LinkRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for CreateLinkTool {
    fn name(&self) -> &str {
        "create_link"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "links")
    }

    fn description(&self) -> &str {
        "Record a directed relationship (from → to) between two objects — e.g. link a \
         note to a calendar event, or a file to an email. Endpoints are {kind, id} \
         pairs; `label`/`note` are optional free text. Idempotent on (from, to, label): \
         re-linking refreshes the note instead of duplicating. Returns the stored link \
         with its id. A self-link (from == to) is rejected."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "from": endpoint_schema("The source endpoint (the 'from' side of the relationship)."),
                "to": endpoint_schema("The target endpoint (the 'to' side of the relationship)."),
                "label": {
                    "type": "string",
                    "description": "Optional relation label, e.g. \"attachment\", \"follow-up\", \"duplicate-of\"."
                },
                "note": { "type": "string", "description": "Optional free-text annotation on the link." }
            },
            "required": ["from", "to"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let author = author(ctx)?;
        let from = source_ref(&args, "from")?;
        let to = source_ref(&args, "to")?;
        let label = opt_str_some(&args, "label");
        let note = opt_str_some(&args, "note");
        let link = self
            .links
            .create(ws, author, &from, &to, label.as_deref(), note.as_deref())
            .await?;
        // Best-effort: project the RELATES_TO edge (no-op unless `[neo4j].enabled`).
        self.ingest.enqueue_link(ws, link.id).await;
        Ok(serde_json::to_value(link)?)
    }
}

/// `list_links` — list relationships, optionally those touching one object.
pub(crate) struct ListLinksTool {
    pub(crate) links: LinkRepo,
}

#[async_trait]
impl Tool for ListLinksTool {
    fn name(&self) -> &str {
        "list_links"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "links")
    }

    fn description(&self) -> &str {
        "List the workspace's links (relationships between objects), most-recently-touched \
         first. Pass `for` (a {kind, id} endpoint) to list only links touching that \
         object in either direction — 'what is related to X'. Each link has id, from, to, \
         label, note."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "for": endpoint_schema("Optional: only links touching this object (in either direction)."),
                "limit": {
                    "type": "integer",
                    "description": "Max links to return, newest-first (1-200, default 50).",
                    "minimum": 1,
                    "maximum": 200
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let limit = opt_clamped_u64(&args, "limit", 50, 200) as i64;
        let links = if args.get("for").is_some() {
            let endpoint = source_ref(&args, "for")?;
            self.links.list_for(ws, &endpoint, limit).await?
        } else {
            self.links.list_by_workspace(ws, limit).await?
        };
        Ok(json!({ "links": links }))
    }
}

/// `delete_link` — remove a relationship by id, reconciling its graph edge.
pub(crate) struct DeleteLinkTool {
    pub(crate) links: LinkRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for DeleteLinkTool {
    fn name(&self) -> &str {
        "delete_link"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "links")
    }

    fn description(&self) -> &str {
        "Delete a relationship (link) by id. Its RELATES_TO graph edge is reconciled \
         away. Returns the deleted link's id."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the link to delete." }
            },
            "required": ["id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = link_id(&args)?;
        self.links.delete(ws, id).await?;
        // Reconcile the projection: the worker finds the link gone and purges its edge.
        self.ingest.enqueue_link(ws, id).await;
        Ok(json!({ "deleted": id }))
    }
}

// ---------------------------------------------------------------------------
// emerged UIs — AI-authored declarative UIs (the "emerged UI" feature).
// ---------------------------------------------------------------------------
