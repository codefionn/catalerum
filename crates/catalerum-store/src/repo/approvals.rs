use catalerum_core::ask::{Answer, Question};
use catalerum_core::id::{ConversationId, PendingApprovalId, PendingQuestionId, WorkspaceId};
use catalerum_core::model::{ApprovalDecision, PendingApproval, PendingQuestion};
use sqlx::types::Json;
use crate::error::Result;
use crate::rows::{PendingApprovalRow, PendingQuestionRow};
use super::{map, PgPool};

// ===========================================================================
// Pending questions (ask_user, SOUL §7/§12)
// ===========================================================================

/// The full set of `pending_questions` columns, in row order.
const PENDING_QUESTION_COLS: &str =
    "id, workspace_id, conversation_id, questions, created_at, resolved_at, answers";

/// CRUD for the `pending_questions` table — unanswered `ask_user` question forms.
/// Every query is workspace-filtered (SOUL §18).
#[derive(Clone, Debug)]
pub struct PendingQuestionRepo {
    pool: PgPool,
}

impl PendingQuestionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Persist a new pending question for a conversation.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        questions: &[Question],
    ) -> Result<PendingQuestion> {
        let id = PendingQuestionId::new().into_uuid();
        let row: PendingQuestionRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO pending_questions (id, workspace_id, conversation_id, questions)
             VALUES ($1, $2, $3, $4)
             RETURNING {PENDING_QUESTION_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(Json(questions))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The most-recent **unresolved** question for a conversation, if any — what the
    /// client fetches on load to re-render the form after a reload/reconnect.
    pub async fn get_unresolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Option<PendingQuestion>> {
        let row: Option<PendingQuestionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {PENDING_QUESTION_COLS} FROM pending_questions
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Resolve **every** unresolved question for a conversation (idempotent). Called
    /// when the user answers or otherwise moves the thread on, so at most one stays
    /// pending. `answers` is the structured form reply, stamped onto the row(s) this
    /// call closes so the Q&A exchange stays durable; `None` (the user typed past
    /// the form / a fresh `ask_user` superseded it) leaves the column NULL — the
    /// question was never answered. Returns how many rows it closed.
    pub async fn resolve_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        answers: Option<&[Answer]>,
    ) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE pending_questions SET resolved_at = CURRENT_TIMESTAMP, answers = $3
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(answers.map(Json))
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// Every question ever asked in a conversation — resolved and pending, oldest
    /// first. The chat client fetches this when replaying a transcript, so an
    /// `ask_user` exchange re-renders with the answers the user actually gave
    /// (correlated to the tool call via the `pending_question_id` in its result).
    pub async fn list_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Vec<PendingQuestion>> {
        let rows: Vec<PendingQuestionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {PENDING_QUESTION_COLS} FROM pending_questions
             WHERE workspace_id = $1 AND conversation_id = $2
             ORDER BY created_at ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }
}

/// The full set of `pending_approvals` columns, in row order.
const PENDING_APPROVAL_COLS: &str =
    "id, workspace_id, conversation_id, tool, arguments, reason, created_at, resolved_at, decision";

/// CRUD for the `pending_approvals` table — guard-deferred tool calls awaiting the
/// user's Approve/Reject (SOUL §7/§12/§19). Every query is workspace-filtered.
#[derive(Clone, Debug)]
pub struct PendingApprovalRepo {
    pool: PgPool,
}

impl PendingApprovalRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a new deferred approval for a conversation (unresolved).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        tool: &str,
        arguments: &serde_json::Value,
        reason: &str,
    ) -> Result<PendingApproval> {
        let id = PendingApprovalId::new().into_uuid();
        let row: PendingApprovalRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO pending_approvals
                 (id, workspace_id, conversation_id, tool, arguments, reason)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING {PENDING_APPROVAL_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(tool)
        .bind(Json(arguments))
        .bind(reason)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The most-recent **unresolved** approval for a conversation, if any — what the
    /// client fetches on load to re-render the Approve/Reject prompt after a reload /
    /// reconnect / restart.
    pub async fn get_unresolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<Option<PendingApproval>> {
        let row: Option<PendingApprovalRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {PENDING_APPROVAL_COLS} FROM pending_approvals
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1"
        ))
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Record the user's decision on a pending approval by id (idempotent: a no-op
    /// if it was already resolved/superseded). Returns the resolved row (so the
    /// caller knows the tool + args to resume), or `None` if there was nothing
    /// unresolved under that id.
    pub async fn resolve(
        &self,
        workspace_id: WorkspaceId,
        id: PendingApprovalId,
        decision: ApprovalDecision,
    ) -> Result<Option<PendingApproval>> {
        let decision_text = match decision {
            ApprovalDecision::Approved => "approved",
            ApprovalDecision::Rejected => "rejected",
        };
        let row: Option<PendingApprovalRow> = sqlx::query_as(assert_sql_safe!(
            "UPDATE pending_approvals SET resolved_at = CURRENT_TIMESTAMP, decision = $3
             WHERE workspace_id = $1 AND id = $2 AND resolved_at IS NULL
             RETURNING {PENDING_APPROVAL_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(id.into_uuid())
        .bind(decision_text)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Supersede **every** unresolved approval for a conversation (idempotent),
    /// leaving `decision` NULL — the deferred tool is abandoned (the user moved the
    /// thread on without deciding). Returns how many rows it closed.
    pub async fn resolve_for_conversation(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
    ) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE pending_approvals SET resolved_at = CURRENT_TIMESTAMP
             WHERE workspace_id = $1 AND conversation_id = $2 AND resolved_at IS NULL",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(res.rows_affected())
    }

    /// The resume path: consume a **resolved** decision matching a re-attempted call
    /// (`(conversation, tool, arguments)`), deleting the row and returning its
    /// ruling. The guard calls this when the agent re-issues the approved/rejected
    /// call so the decision applies exactly once. `None` when nothing matches.
    pub async fn take_resolved(
        &self,
        workspace_id: WorkspaceId,
        conversation_id: ConversationId,
        tool: &str,
        arguments: &serde_json::Value,
    ) -> Result<Option<ApprovalDecision>> {
        let decision: Option<String> = sqlx::query_scalar(
            "DELETE FROM pending_approvals WHERE id = (
                 SELECT id FROM pending_approvals
                 WHERE workspace_id = $1 AND conversation_id = $2 AND tool = $3
                   AND arguments = $4 AND resolved_at IS NOT NULL AND decision IS NOT NULL
                 ORDER BY resolved_at DESC
                 LIMIT 1
             )
             RETURNING decision",
        )
        .bind(workspace_id.into_uuid())
        .bind(conversation_id.into_uuid())
        .bind(tool)
        .bind(Json(arguments))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(decision.and_then(|d| match d.as_str() {
            "approved" => Some(ApprovalDecision::Approved),
            "rejected" => Some(ApprovalDecision::Rejected),
            _ => None,
        }))
    }
}
