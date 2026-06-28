//! Memory + profile tools (SOUL §22).

use super::*;

/// `remember` — store a durable, curated fact about the user or workspace
/// (SOUL §22). Scoped `user` (private to the acting member, the default) or
/// `workspace` (shared). Every memory is an inspectable row, never hidden state.
///
/// Writes go through the shared dedup seam (SOUL §29): an already-known fact is
/// not stored twice (it is `deduplicated`), and a fact that extends a known one
/// `refine`s it. `search` (present iff a vector backend is configured) enables the
/// seam's embedding-similarity layer and gates embed-on-store — the same backend
/// `recall` searches — so the tool needs no separate ingest hook.
pub(crate) struct RememberTool {
    pub(crate) store: Store,
    pub(crate) search: Option<SemanticSearch>,
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "memory")
    }

    fn description(&self) -> &str {
        "Save a durable fact to remember later — a user preference, a recurring \
         detail, a relationship. scope='user' (default, private to this user) or \
         'workspace' (shared). Returns the memory including its id and a `status`: \
         'stored' (new), 'deduplicated' (already known — nothing added, so don't \
         re-save it), or 'refined' (it extended a known fact, updated in place)."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "The fact to remember (required, non-empty)." },
                "scope": {
                    "type": "string",
                    "enum": ["user", "workspace"],
                    "description": "'user' (default) keeps it private to this user; 'workspace' shares it."
                }
            },
            "required": ["text"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let text = required_str(&args, "text")?;
        // Default to a private user memory; fall back to workspace scope when the
        // caller is an agent with no acting user (a user memory needs a member).
        let requested_user = matches!(opt_str(&args, "scope").as_str(), "" | "user");
        let (scope, user_id) = match (requested_user, ctx.user_id) {
            (true, Some(uid)) => (MemoryScope::User, Some(uid)),
            _ => (MemoryScope::Workspace, None),
        };
        // Store through the shared dedup seam (SOUL §29): the heuristic exact/
        // superset layer plus, when a vector backend is configured, embedding
        // similarity. The seam skips duplicates (touching the existing row),
        // refines extensions, and (re-)embeds a genuinely new/changed memory.
        let index = self
            .search
            .as_ref()
            .map(|s| catalerum_ingest::MemoryDedupIndex {
                embedder: &*s.embedder,
                vector: &s.vector,
                embed_model: s.embed_model.as_str(),
            });
        let outcome = catalerum_ingest::store_memory_deduped(
            &self.store,
            index.as_ref(),
            ws,
            scope,
            user_id,
            &text,
            None,
        )
        .await
        .map_err(ingest_err)?;
        // Additive: the serialized memory plus a `status` so the model (and tests)
        // can tell a new fact from an already-known one.
        let mut value = serde_json::to_value(&outcome.memory)?;
        if let Json::Object(map) = &mut value {
            map.insert("status".to_string(), json!(outcome.status.as_str()));
        }
        Ok(value)
    }
}

/// `recall` — list the memories visible to the caller, most-recent first
/// (SOUL §22). (Semantic, relevance-ranked recall layers on once memories are
/// embedded into Qdrant; today this is recency-ordered.)
pub(crate) struct RecallTool {
    pub(crate) memories: MemoryRepo,
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "memory")
    }

    fn description(&self) -> &str {
        "Recall stored memories about the user and workspace (most recent first). \
         Use it to personalize answers with what you already know."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Max memories to return (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            }
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;
        let memories = self.memories.list_visible(ws, ctx.user_id, limit).await?;
        let results: Vec<Json> = memories
            .into_iter()
            .map(|m| {
                json!({
                    "id": m.id,
                    "text": m.text,
                    "scope": m.scope,
                    "created_at": m.created_at,
                })
            })
            .collect();
        Ok(json!({ "memories": results }))
    }
}

/// `forget` — delete a memory by id (workspace-scoped, SOUL §22). The user (or
/// the assistant on their behalf) can always remove a memory — never hidden.
pub(crate) struct ForgetTool {
    pub(crate) memories: MemoryRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for ForgetTool {
    fn name(&self) -> &str {
        "forget"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "memory")
    }

    fn description(&self) -> &str {
        "Delete a stored memory by id. Use when a remembered fact is wrong or no \
         longer wanted."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the memory to forget." }
            },
            "required": ["id"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = required_str(&args, "id")?
            .parse::<MemoryId>()
            .map_err(|e| Error::invalid(format!("invalid memory id: {e}")))?;
        self.memories.delete(ws, id).await?;
        // Reconcile: the embed worker finds the memory gone and purges its vectors.
        self.ingest.enqueue_memory(ws, id).await;
        Ok(json!({ "forgotten": true, "id": id }))
    }
}

/// `update_memory` — replace a stored memory's text by id (workspace-scoped,
/// SOUL §22). The in-place edit behind `PUT /memories/{id}`: correct a remembered
/// fact that changed **without** losing the memory's id/created_at/scope (the gap
/// `remember`+`forget` left — they can only re-create with a fresh id).
pub(crate) struct UpdateMemoryTool {
    pub(crate) memories: MemoryRepo,
    pub(crate) ingest: NoteIngest,
}

#[async_trait]
impl Tool for UpdateMemoryTool {
    fn name(&self) -> &str {
        "update_memory"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "memory")
    }

    fn description(&self) -> &str {
        "Update a stored memory's text by id, keeping its id and scope. Use to \
         correct a remembered fact that changed (rather than forget + remember)."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Id of the memory to update." },
                "text": { "type": "string", "description": "The corrected fact (required, non-empty)." }
            },
            "required": ["id", "text"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = required_str(&args, "id")?
            .parse::<MemoryId>()
            .map_err(|e| Error::invalid(format!("invalid memory id: {e}")))?;
        let text = required_str(&args, "text")?;
        let memory = self.memories.update_text(ws, id, &text).await?;
        // Re-embed the new text so semantic recall reflects the edit (SOUL §22),
        // the same reconcile `remember` does on create.
        self.ingest.enqueue_memory(ws, id).await;
        Ok(serde_json::to_value(memory)?)
    }
}

/// `update_profile` — set/merge structured fields on the acting user's profile
/// (SOUL §22), which is injected into the chat system prompt every turn. Merges
/// at the top level (existing keys not provided are preserved).
pub(crate) struct UpdateProfileTool {
    pub(crate) profiles: ProfileRepo,
}

#[async_trait]
impl Tool for UpdateProfileTool {
    fn name(&self) -> &str {
        "update_profile"
    }

    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "profile")
    }

    fn description(&self) -> &str {
        "Update the user's profile — structured personal details that personalize \
         every answer (e.g. timezone, working_hours, preferences). Pass `fields` \
         as a JSON object of key→value; it merges into the existing profile."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "fields": {
                    "type": "object",
                    "description": "Flat object of profile fields to set/merge, e.g. {\"timezone\": \"Europe/Berlin\"}."
                }
            },
            "required": ["fields"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        // The profile is per-user; an agent run with no acting user has no profile.
        let user_id = ctx
            .user_id
            .ok_or_else(|| Error::invalid("update_profile requires an acting user"))?;
        let obj = args
            .get("fields")
            .and_then(Json::as_object)
            .ok_or_else(|| Error::invalid("`fields` is required and must be an object"))?;
        if obj.is_empty() {
            return Err(Error::invalid("`fields` must not be empty"));
        }
        let fields: Map = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let profile = self.profiles.merge(ws, user_id, &fields).await?;
        Ok(serde_json::to_value(profile)?)
    }
}
