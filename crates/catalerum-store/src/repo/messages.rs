use catalerum_core::id::{ConversationId, MessageId, WorkspaceId};
use catalerum_core::model::{Message, MessageRole};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{message_role_to_text, MessageRow, MessageSearchRow};
use super::{map, PgPool};

// ===========================================================================
// Messages
// ===========================================================================

/// A message to insert. The id and `created_at` are assigned by the store.
#[derive(Clone, Debug)]
pub struct NewMessage<'a> {
    /// Owning workspace. The insert only succeeds when `conversation_id` belongs
    /// to this workspace (SOUL §18), so a caller cannot write into another
    /// tenant's thread.
    pub workspace_id: WorkspaceId,
    /// Owning conversation.
    pub conversation_id: ConversationId,
    /// Pre-chosen row id. `None` mints a fresh [`MessageId`] at insert time (the
    /// default); `Some` lets a caller fix the id ahead of the insert — the chat
    /// path pre-generates the anchoring user message's id so a detached turn's
    /// Valkey stream key (`cat:turnbuf:{conv}:{msg}`) is known synchronously,
    /// before the row persists (SOUL §7/§12). `created_at` stays DB-assigned.
    pub id: Option<MessageId>,
    /// Message role.
    pub role: MessageRole,
    /// Text content (may be empty for a pure tool-call assistant turn).
    pub content: &'a str,
    /// User-turn file/image references (SOUL §9/§12); empty for non-user rows.
    pub attachments: &'a [catalerum_core::model::Attachment],
    /// A user turn's `/<skill>` invocation snapshot (SOUL §12/§23); `None` for
    /// every other row.
    pub skill: Option<&'a catalerum_core::model::SkillInvocation>,
    /// Tool calls emitted by an assistant turn (empty otherwise).
    pub tool_calls: &'a [catalerum_core::model::ToolCall],
    /// For a `Tool` message, the id of the call it answers.
    pub tool_call_id: Option<&'a str>,
    /// For a `Tool` message, whether the call failed (the `content` holds the
    /// error payload). `false` for non-tool rows.
    pub tool_is_error: bool,
    /// For a `Tool` message, the dispatch duration in milliseconds, when measured.
    pub tool_duration_ms: Option<i64>,
    /// Per-turn token + cost accounting for the exchange — set only on the
    /// **final assistant message** (the agent loop's summed usage), `None`
    /// everywhere else. Persisted so a replayed transcript keeps the token
    /// info-icon / cost readout the live turn showed.
    pub usage: Option<catalerum_core::stream::Usage>,
    /// An assistant turn's reasoning blocks and the model that produced them;
    /// `None` for every other row and for a turn without reasoning.
    pub reasoning: Option<&'a catalerum_core::model::MessageReasoning>,
}

impl<'a> NewMessage<'a> {
    /// A plain text message with no tool calls.
    #[must_use]
    pub fn text(
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        role: MessageRole,
        content: &'a str,
    ) -> Self {
        Self {
            workspace_id,
            conversation_id,
            id: None,
            role,
            content,
            attachments: &[],
            skill: None,
            tool_calls: &[],
            tool_call_id: None,
            tool_is_error: false,
            tool_duration_ms: None,
            usage: None,
            reasoning: None,
        }
    }
}

/// Default cap on [`MessageRepo::search_in_workspace`] results.
pub const DEFAULT_MESSAGE_SEARCH_LIMIT: i64 = 50;

/// A message-content search hit: the matched [`Message`] plus the title of the
/// conversation it belongs to (for displaying / opening the thread).
#[derive(Clone, Debug)]
pub struct MessageSearchHit {
    pub message: Message,
    pub conversation_title: Option<String>,
}

/// CRUD for the `messages` table.
#[derive(Clone, Debug)]
pub struct MessageRepo {
    pool: PgPool,
}

impl MessageRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Insert a message and return the stored row.
    ///
    /// The row's `workspace_id` comes from the conversation, selected by both
    /// `msg.conversation_id` and `msg.workspace_id`: a conversation outside that
    /// workspace inserts nothing and returns [`StoreError::NotFound`].
    pub async fn insert(&self, msg: &NewMessage<'_>) -> Result<Message> {
        let id = msg.id.unwrap_or_default().into_uuid();
        let role_text = message_role_to_text(msg.role)?;
        // Token counts are u32 on the wire; widen to BIGINT for storage. Bound
        // together with the message so usage is atomic — all six or all NULL.
        let u = msg.usage.as_ref();
        let row: MessageRow = sqlx::query_as(
            "INSERT INTO messages \
             (id, workspace_id, conversation_id, role, content, tool_calls, tool_call_id, \
              tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, \
              cached_tokens, cache_creation_tokens, cost_usd, attachments, skill, reasoning)
             SELECT $1, c.workspace_id, c.id, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, \
                    $14, $15, $16, $18
             FROM conversations c WHERE c.id = $2 AND c.workspace_id = $17
             RETURNING id, workspace_id, conversation_id, role, content, attachments, skill, \
                       tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, \
                       completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, \
                       cost_usd, reasoning, created_at",
        )
        .bind(id)
        .bind(msg.conversation_id.into_uuid())
        .bind(role_text)
        .bind(msg.content)
        .bind(Json(msg.tool_calls.to_vec()))
        .bind(msg.tool_call_id)
        .bind(msg.tool_is_error)
        .bind(msg.tool_duration_ms)
        .bind(u.map(|u| i64::from(u.prompt_tokens)))
        .bind(u.map(|u| i64::from(u.completion_tokens)))
        .bind(u.map(|u| i64::from(u.total_tokens)))
        .bind(u.map(|u| i64::from(u.cached_tokens)))
        .bind(u.map(|u| i64::from(u.cache_creation_tokens)))
        .bind(u.and_then(|u| u.cost_usd))
        .bind(Json(msg.attachments.to_vec()))
        .bind(msg.skill.map(Json))
        .bind(msg.workspace_id.into_uuid())
        .bind(msg.reasoning.map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Stamp the summed token/cost [`Usage`](catalerum_core::stream::Usage) of a
    /// finished exchange onto its **final assistant message** (SOUL §12).
    ///
    /// The agent loop persists each turn incrementally as it completes, with no
    /// usage — the exchange total isn't known until the loop ends. This back-fills
    /// that total onto the last assistant row once the run finishes, so a reopened
    /// conversation replays the same token info-icon / cost readout the live turn
    /// showed. A `None` usage writes NULLs (a no-op on an already-null row).
    /// Returns [`StoreError::NotFound`] if `id` matches no row in `workspace_id`.
    pub async fn set_usage(
        &self,
        workspace_id: WorkspaceId,
        id: MessageId,
        usage: Option<catalerum_core::stream::Usage>,
    ) -> Result<()> {
        let u = usage.as_ref();
        let res = sqlx::query(
            "UPDATE messages SET \
             prompt_tokens = $2, completion_tokens = $3, total_tokens = $4, \
             cached_tokens = $5, cache_creation_tokens = $6, cost_usd = $7 \
             WHERE id = $1 AND workspace_id = $8",
        )
        .bind(id.into_uuid())
        .bind(u.map(|u| i64::from(u.prompt_tokens)))
        .bind(u.map(|u| i64::from(u.completion_tokens)))
        .bind(u.map(|u| i64::from(u.total_tokens)))
        .bind(u.map(|u| i64::from(u.cached_tokens)))
        .bind(u.map(|u| i64::from(u.cache_creation_tokens)))
        .bind(u.and_then(|u| u.cost_usd))
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Fetch a single message by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: MessageId) -> Result<Message> {
        let row: MessageRow = sqlx::query_as(
            "SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
             FROM messages WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List all messages in a conversation, oldest first (LLM replay order).
    pub async fn list_by_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
             FROM messages WHERE conversation_id = $1 AND workspace_id = $2
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// List the most recent `limit` messages in a conversation, oldest first
    /// (a bounded context window for the agent loop).
    pub async fn list_recent(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
             FROM (
                 SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
                 FROM messages WHERE conversation_id = $1 AND workspace_id = $3
                 ORDER BY created_at DESC, id DESC
                 LIMIT $2
             ) recent
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .bind(limit)
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// List the most recent `limit` messages **strictly after** `anchor` in
    /// replay order (`created_at ASC, id ASC`), oldest first — the bounded seed
    /// window for a conversation whose older prefix was folded into a rolling
    /// compaction summary (SOUL §7/§12): the summary stands in for everything
    /// up to and including the anchor, this returns what comes after. An
    /// unknown anchor matches the empty subquery and returns nothing (callers
    /// only pass a `summary_upto` the FK guarantees exists).
    pub async fn list_recent_after(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        anchor: MessageId,
        limit: i64,
    ) -> Result<Vec<Message>> {
        let rows: Vec<MessageRow> = sqlx::query_as(
            "SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
             FROM (
                 SELECT id, workspace_id, conversation_id, role, content, attachments, skill, tool_calls, tool_call_id, tool_is_error, tool_duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens, cache_creation_tokens, cost_usd, reasoning, created_at
                 FROM messages
                 WHERE conversation_id = $1 AND workspace_id = $4
                   AND (created_at, id) > (
                       SELECT created_at, id FROM messages
                       WHERE id = $2 AND workspace_id = $4
                   )
                 ORDER BY created_at DESC, id DESC
                 LIMIT $3
             ) recent
             ORDER BY created_at ASC, id ASC",
        )
        .bind(conversation_id.into_uuid())
        .bind(anchor.into_uuid())
        .bind(limit)
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Message::try_from).collect()
    }

    /// Search message **content** across a workspace, newest match first. Filters
    /// on `messages.workspace_id`, joins each row to its conversation for the
    /// thread title, and matches `query` as a **literal, case-insensitive substring**
    /// (`strpos(lower(content), lower($2))` — no `LIKE` wildcard semantics, so a
    /// user's `%`/`_` are matched literally). A blank `query` returns nothing
    /// (the caller should not run an unbounded "match everything" search). Bounded
    /// by `limit` (floored at 1). `messages_workspace_idx` orders the scan; the
    /// substring match itself is unindexed.
    pub async fn search_in_workspace(
        &self,
        workspace_id: WorkspaceId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<MessageSearchHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<MessageSearchRow> = sqlx::query_as(
            "SELECT m.id, m.workspace_id, m.conversation_id, m.role, m.content, m.tool_calls,
                    m.tool_call_id, m.tool_is_error, m.tool_duration_ms, m.created_at,
                    c.title AS conversation_title
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id AND c.workspace_id = m.workspace_id
             WHERE m.workspace_id = $1 AND strpos(lower(m.content), lower($2)) > 0
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT $3",
        )
        .bind(workspace_id.into_uuid())
        .bind(query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter()
            .map(|row| {
                let (msg_row, conversation_title) = row.split();
                Message::try_from(msg_row).map(|message| MessageSearchHit {
                    message,
                    conversation_title,
                })
            })
            .collect()
    }

    /// Delete a single message, workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: MessageId) -> Result<()> {
        let res = sqlx::query("DELETE FROM messages WHERE id = $1 AND workspace_id = $2")
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

    /// Delete every message in `conversation_id` that comes *after* `anchor` in
    /// replay order (`created_at ASC, id ASC`) — the anchor itself is kept.
    ///
    /// Used to regenerate a turn (SOUL §12): the anchoring user message stays and
    /// the transcript tail it produced (the old answer + any later exchanges) is
    /// dropped before the agent loop re-answers it. The `(created_at, id)`
    /// row-value comparison is exactly the replay ordering, so "everything after
    /// the anchor" is expressed precisely. Returns the number of rows removed
    /// (`0` when the anchor was already the last message, or unknown — not an
    /// error). The anchor is *not* required to exist: an unknown id matches the
    /// empty subquery and deletes nothing.
    pub async fn delete_after(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        anchor: MessageId,
    ) -> Result<u64> {
        let res = sqlx::query(
            "DELETE FROM messages \
             WHERE conversation_id = $1 AND workspace_id = $3 \
               AND (created_at, id) > ( \
                   SELECT created_at, id FROM messages WHERE id = $2 AND workspace_id = $3 \
               )",
        )
        .bind(conversation_id.into_uuid())
        .bind(anchor.into_uuid())
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }
}
