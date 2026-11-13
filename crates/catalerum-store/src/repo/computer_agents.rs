use catalerum_core::computer::ComputerCapabilities;
use catalerum_core::id::{ComputerAgentId, UserId, WorkspaceId};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{computer_platform_to_text, ComputerAgent, ComputerAgentRow};
use super::{map, PgPool};

// ===========================================================================
// Computer agents (enrolled server/desktop daemons, SOUL §19/§20)
// ===========================================================================

/// The stored columns of a `computer_agents` row, projected identically into a
/// [`ComputerAgentRow`]. `token_hash` is deliberately excluded from the general
/// projection — it is only ever read by the token-lookup path.
const COMPUTER_AGENT_COLS: &str = "id, workspace_id, user_id, name, token_hash, platform, \
     capabilities, created_at, last_seen_at, revoked_at";

/// CRUD for the `computer_agents` table (SOUL §19/§20). Every query is
/// workspace-filtered (SOUL §18); `(workspace_id, name)` is UNIQUE. The enrollment
/// token is stored only as its hash (`token_hash`); the plaintext is returned to
/// the enroller exactly once at [`create`](ComputerAgentRepo::create) time by the
/// caller (this repo takes the already-hashed value).
#[derive(Clone, Debug)]
pub struct ComputerAgentRepo {
    pool: PgPool,
}

impl ComputerAgentRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Enroll a new agent owned by `user_id`. `token_hash` is the SHA-256 hash of
    /// the freshly minted token the caller hands back once. A duplicate
    /// `(workspace, name)` is a [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        name: &str,
        token_hash: &str,
    ) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO computer_agents (id, workspace_id, user_id, name, token_hash)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {COMPUTER_AGENT_COLS}"
        ))
        .bind(ComputerAgentId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(name)
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Look up an **active** (non-revoked) agent by its token hash — the WS
    /// handshake path. A revoked or unknown token yields [`StoreError::NotFound`].
    /// Not workspace-filtered: the token is what identifies the agent's workspace.
    pub async fn get_active_by_token_hash(&self, token_hash: &str) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE token_hash = $1 AND revoked_at IS NULL"
        ))
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch one agent by id, scoped to its workspace.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        id: ComputerAgentId,
    ) -> Result<ComputerAgent> {
        let row: ComputerAgentRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's agents (including revoked), most-recently-enrolled first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<ComputerAgent>> {
        let rows: Vec<ComputerAgentRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {COMPUTER_AGENT_COLS} FROM computer_agents
             WHERE workspace_id = $1
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(ComputerAgent::from).collect())
    }

    /// Record a live connection: refresh the announced capabilities + denormalised
    /// platform and bump `last_seen_at`. Called on connect and on each heartbeat.
    /// Workspace-scoped; an id outside `workspace_id` is
    /// [`StoreError::NotFound`].
    pub async fn touch_seen(
        &self,
        workspace_id: WorkspaceId,
        id: ComputerAgentId,
        capabilities: &ComputerCapabilities,
    ) -> Result<()> {
        let platform = computer_platform_to_text(capabilities.platform)?;
        let res = sqlx::query(
            "UPDATE computer_agents
                SET capabilities = $2, platform = $3, last_seen_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $4",
        )
        .bind(id.into_uuid())
        .bind(Json(capabilities))
        .bind(platform)
        .bind(workspace_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Revoke an agent (its token stops authenticating; the row is retained for
    /// audit). Idempotent — a second revoke keeps the original timestamp.
    pub async fn revoke(&self, workspace_id: WorkspaceId, id: ComputerAgentId) -> Result<()> {
        let res = sqlx::query(
            "UPDATE computer_agents
                SET revoked_at = COALESCE(revoked_at, CURRENT_TIMESTAMP)
             WHERE id = $1 AND workspace_id = $2",
        )
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

    /// Permanently delete an agent row, scoped to its workspace.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: ComputerAgentId) -> Result<()> {
        let res = sqlx::query("DELETE FROM computer_agents WHERE id = $1 AND workspace_id = $2")
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
