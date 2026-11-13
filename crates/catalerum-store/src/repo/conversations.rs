use catalerum_core::id::{AgentProfileId, ConversationId, MessageId, WorkspaceId};
use catalerum_core::model::{Conversation, Message, MessageRole, Origin};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{ConversationRow, message_role_to_text, MessageRow, origin_to_text};
use super::{map, PgPool};

// ===========================================================================
// Conversations
// ===========================================================================

/// CRUD for the `conversations` table.
#[derive(Clone, Debug)]
pub struct ConversationRepo {
    pool: PgPool,
}

impl ConversationRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a conversation in a workspace.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        title: Option<&str>,
        origin: Origin,
    ) -> Result<Conversation> {
        self.create_with_id(workspace_id, ConversationId::new(), title, origin)
            .await
    }

    /// Create a conversation with a caller-chosen id, idempotently.
    ///
    /// The web chat uses this for its durable outbox: if the POST response is lost,
    /// retrying the same request returns the row that was already committed instead
    /// of creating a second empty conversation. An id owned by another workspace is
    /// never returned (the scoped re-read yields [`StoreError::NotFound`]).
    pub async fn create_with_id(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
        origin: Origin,
    ) -> Result<Conversation> {
        let origin_text = origin_to_text(origin)?;
        let inserted: Option<ConversationRow> = sqlx::query_as(
            "INSERT INTO conversations (id, workspace_id, title, origin)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(origin_text)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        match inserted {
            Some(row) => row.try_into(),
            None => self.get(workspace_id, id).await,
        }
    }

    /// Atomically create a conversation and its first plain-text message.
    ///
    /// Automation output uses this so a failed message insert can never leave an
    /// empty thread in the chat sidebar. The caller chooses the origin and role;
    /// ids and timestamps are assigned here.
    pub async fn create_with_initial_message(
        &self,
        workspace_id: WorkspaceId,
        title: Option<&str>,
        origin: Origin,
        role: MessageRole,
        content: &str,
    ) -> Result<(Conversation, Message)> {
        let conversation_id = ConversationId::new();
        let message_id = MessageId::new();
        let origin_text = origin_to_text(origin)?;
        let role_text = message_role_to_text(role)?;
        let mut tx = self.pool.begin().await.map_err(map)?;

        let conversation_row: ConversationRow = sqlx::query_as(
            "INSERT INTO conversations (id, workspace_id, title, origin)
             VALUES ($1, $2, $3, $4)
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(conversation_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .bind(origin_text)
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let message_row: MessageRow = sqlx::query_as(
            "INSERT INTO messages (id, conversation_id, role, content, workspace_id)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls,
                       tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens,
                       completion_tokens, total_tokens, cached_tokens, cache_creation_tokens,
                       cost_usd, created_at",
        )
        .bind(message_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(role_text)
        .bind(content)
        .bind(workspace_id.into_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(map)?;

        let conversation = conversation_row.try_into()?;
        let message = message_row.try_into()?;
        tx.commit().await.map_err(map)?;
        Ok((conversation, message))
    }

    /// Fetch a conversation, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: ConversationId) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "SELECT id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at
             FROM conversations WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's conversations, newest first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Conversation>> {
        let rows: Vec<ConversationRow> = sqlx::query_as(
            "SELECT id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at
             FROM conversations WHERE workspace_id = $1
             ORDER BY created_at DESC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Conversation::try_from).collect()
    }

    /// Rename a conversation (workspace-scoped). Pass `None` to clear the title.
    pub async fn update_title(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET title = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Bind (or unbind, with `None`) the [`AgentProfile`](catalerum_core::model::AgentProfile) a conversation runs as —
    /// the chat "run as a profile" picker (SOUL §19/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The same-workspace FK
    /// guarantees the profile (when `Some`) belongs to this workspace.
    pub async fn set_agent_profile(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        agent_profile_id: Option<AgentProfileId>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET agent_profile_id = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(agent_profile_id.map(AgentProfileId::into_uuid))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Pin (or clear, with `None`) the model a conversation's chat loop thinks with
    /// — the chat "model" picker (SOUL §7/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The model id is a
    /// free-form gateway string (validated, if at all, at the app layer), mirroring
    /// the user's `llm_settings.chat_model`; pass a trimmed non-empty value or
    /// `None` to clear.
    pub async fn set_model(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        model: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET model = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(model)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear, with `None`) the reasoning ("thinking") effort a conversation's
    /// chat loop requests — the chat "thinking" picker (SOUL §7/§12), workspace-scoped.
    /// [`StoreError::NotFound`] if the conversation is absent. The effort is a
    /// free-form gateway token (`low`/`medium`/`high`/`xhigh`/`max`, passed through to
    /// the model, validated only at the app layer); pass a trimmed non-empty value or
    /// `None` to clear (no reasoning requested).
    pub async fn set_reasoning(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        reasoning_effort: Option<&str>,
    ) -> Result<Conversation> {
        let row: ConversationRow = sqlx::query_as(
            "UPDATE conversations SET reasoning_effort = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING id, workspace_id, title, tags, title_manual, origin, agent_profile_id, model, reasoning_effort, summary, summary_upto, created_at, updated_at",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(reasoning_effort)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Set (or clear, with `None`) a conversation's rolling auto-compaction
    /// summary + the last message it covers (SOUL §7/§12), workspace-scoped.
    /// The two columns move together — a summary without its coverage anchor is
    /// meaningless (and the seed ignores a half-set pair). The FK's
    /// `ON DELETE SET NULL` clears the anchor when a regenerate prunes the row
    /// it points at, which invalidates the summary the same way.
    /// [`StoreError::NotFound`] if the conversation is absent.
    pub async fn set_summary(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        summary: Option<(&str, MessageId)>,
    ) -> Result<()> {
        let (text, upto) = match summary {
            Some((text, upto)) => (Some(text), Some(upto.into_uuid())),
            None => (None, None),
        };
        let res = sqlx::query(
            "UPDATE conversations SET summary = $3, summary_upto = $4, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(text)
        .bind(upto)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Store the background auto-title/auto-tag pass result (workspace-scoped):
    /// `tags` always; `title` only when it is `Some` **and** the thread has no
    /// user-chosen title (`title_manual` — an explicit rename pins the name and
    /// the generator must never overwrite it). `StoreError::NotFound` if absent.
    pub async fn set_generated_meta(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
        tags: &[String],
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE conversations
             SET tags = $3,
                 title = COALESCE(CASE WHEN NOT title_manual THEN $4 END, title),
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(Json(tags.to_vec()))
        .bind(title)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Rename a conversation (workspace-scoped, **manual** — sets `title_manual`
    /// so the background auto-title pass stops overwriting the chosen name).
    /// Pass `None` to clear the title (and unpin).
    pub async fn rename_manual(
        &self,
        workspace_id: WorkspaceId,
        id: ConversationId,
        title: Option<&str>,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE conversations
             SET title = $3, title_manual = ($3 IS NOT NULL), updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(title)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Delete a conversation (cascades to its messages), workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ConversationId) -> Result<()> {
        let res = sqlx::query("DELETE FROM conversations WHERE id = $1 AND workspace_id = $2")
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
}
