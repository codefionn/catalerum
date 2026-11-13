use catalerum_core::id::{GrantId, McpEndpointId, WorkspaceId};
use catalerum_core::model::{Author, McpEndpoint};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{author_to_parts, McpEndpointRow};
use super::{map, PgPool};

// ===========================================================================
// MCP endpoints (Boa-scripted scoped MCP endpoints, SOUL §26)
// ===========================================================================

/// The stored columns of an `mcp_endpoints` row, in one place so every query
/// projects them identically into a [`McpEndpointRow`].
const MCP_ENDPOINT_COLS: &str = "id, workspace_id, name, description, script, bucket_name, \
     key_prefix, grant_id, enabled, author_kind, author_id, created_at, updated_at";

/// The editable fields of an MCP endpoint — shared by `create` and `update`.
#[derive(Clone, Debug)]
pub struct McpEndpointInput {
    pub name: String,
    pub description: String,
    pub script: String,
    pub bucket_name: Option<String>,
    pub key_prefix: Option<String>,
    pub grant_id: Option<GrantId>,
    pub enabled: bool,
}

/// CRUD for the `mcp_endpoints` table — user-authored, Boa-scripted MCP endpoints
/// (SOUL §26). Every query is workspace-filtered (SOUL §18); `(workspace_id,
/// name)` is UNIQUE, so endpoints are addressed by name in the URL.
#[derive(Clone, Debug)]
pub struct McpEndpointRepo {
    pool: PgPool,
}

impl McpEndpointRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an endpoint authored by `author`. A duplicate `(workspace, name)` is
    /// a [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        input: &McpEndpointInput,
    ) -> Result<McpEndpoint> {
        let (author_kind, author_id) = author_to_parts(author);
        let row: McpEndpointRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO mcp_endpoints
                 (id, workspace_id, name, description, script, bucket_name, key_prefix,
                  grant_id, enabled, author_kind, author_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {MCP_ENDPOINT_COLS}"
        ))
        .bind(McpEndpointId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.script)
        .bind(input.bucket_name.as_deref())
        .bind(input.key_prefix.as_deref())
        .bind(input.grant_id.map(GrantId::into_uuid))
        .bind(input.enabled)
        .bind(author_kind)
        .bind(author_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an endpoint by id, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: McpEndpointId) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an endpoint by its (workspace-unique) name — the serve-time lookup.
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's endpoints, most-recently-edited first.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<McpEndpoint>> {
        let rows: Vec<McpEndpointRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_ENDPOINT_COLS} FROM mcp_endpoints
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(McpEndpoint::try_from).collect()
    }

    /// Update an endpoint's editable fields (author is immutable), bumping
    /// `updated_at`. A rename that collides is a [`StoreError::Conflict`].
    pub async fn update(
        &self,
        workspace_id: WorkspaceId,
        id: McpEndpointId,
        input: &McpEndpointInput,
    ) -> Result<McpEndpoint> {
        let row: McpEndpointRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE mcp_endpoints
                SET name = $3, description = $4, script = $5, bucket_name = $6,
                    key_prefix = $7, grant_id = $8, enabled = $9, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MCP_ENDPOINT_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&input.name)
        .bind(&input.description)
        .bind(&input.script)
        .bind(input.bucket_name.as_deref())
        .bind(input.key_prefix.as_deref())
        .bind(input.grant_id.map(GrantId::into_uuid))
        .bind(input.enabled)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete an endpoint by id, scoped to its workspace.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: McpEndpointId) -> Result<()> {
        let res = sqlx::query("DELETE FROM mcp_endpoints WHERE id = $1 AND workspace_id = $2")
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

// ===========================================================================
// MCP endpoint share tokens (revocable scoped tokens, SOUL §26)
// ===========================================================================

/// A minted share token for an MCP endpoint, as shown in a management listing.
/// The raw token is never stored (only its SHA-256 hash) and never leaves the
/// mint response — a listing carries only id + timestamps + revocation state.
#[derive(Clone, Debug)]
pub struct McpEndpointToken {
    /// The token row id (the handle used to revoke).
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub endpoint_id: McpEndpointId,
    /// When the token was minted.
    pub created_at: DateTime<Utc>,
    /// When it expires (also enforced by the signed claims).
    pub expires_at: DateTime<Utc>,
    /// When it was revoked, if it has been.
    pub revoked_at: Option<DateTime<Utc>>,
}

/// The stored columns of a `mcp_endpoint_tokens` row, projected identically
/// into a [`McpEndpointTokenRow`]. `token_hash` is deliberately excluded from
/// the general projection — it is only ever used in the `WHERE` of the
/// serve-time lookup, never read back out.
const MCP_ENDPOINT_TOKEN_COLS: &str =
    "id, workspace_id, endpoint_id, created_at, expires_at, revoked_at";

#[derive(sqlx::FromRow)]
struct McpEndpointTokenRow {
    id: Uuid,
    workspace_id: Uuid,
    endpoint_id: Uuid,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    revoked_at: Option<DateTime<Utc>>,
}

impl From<McpEndpointTokenRow> for McpEndpointToken {
    fn from(r: McpEndpointTokenRow) -> Self {
        Self {
            id: r.id,
            workspace_id: WorkspaceId::from_uuid(r.workspace_id),
            endpoint_id: McpEndpointId::from_uuid(r.endpoint_id),
            created_at: r.created_at,
            expires_at: r.expires_at,
            revoked_at: r.revoked_at,
        }
    }
}

/// CRUD for the `mcp_endpoint_tokens` table (SOUL §26). The HMAC signature on a
/// scoped endpoint token makes it unforgeable; this table makes it
/// **revocable**: the serve path (`POST /mcp/s/{token}`) requires a live row
/// (present, not revoked, not expired) in addition to a valid signature, so
/// deleting the row (or the endpoint, via cascade) kills the token immediately.
/// Every query is workspace-filtered (SOUL §18); the store only ever sees the
/// token's hash.
#[derive(Clone, Debug)]
pub struct McpEndpointTokenRepo {
    pool: PgPool,
}

impl McpEndpointTokenRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Record a freshly-minted token. `token_hash` must already be the hash of
    /// the raw token (the store never sees plaintext). A duplicate hash is a
    /// [`StoreError::Conflict`].
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<McpEndpointToken> {
        let row: McpEndpointTokenRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO mcp_endpoint_tokens
                 (id, workspace_id, endpoint_id, token_hash, expires_at)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {MCP_ENDPOINT_TOKEN_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .bind(token_hash)
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// The serve-time check: is there a **live** (not revoked, not expired)
    /// token with this hash? Returns the row, or [`StoreError::NotFound`] —
    /// indistinguishable between unknown, revoked, and expired (no probing
    /// signal, mirroring the route's `404`).
    pub async fn get_live_by_token_hash(&self, token_hash: &str) -> Result<McpEndpointToken> {
        let row: McpEndpointTokenRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_ENDPOINT_TOKEN_COLS} FROM mcp_endpoint_tokens
             WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > CURRENT_TIMESTAMP"
        ))
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List an endpoint's tokens (newest first), workspace-scoped, for the
    /// management panel. Revoked/expired rows are included so the panel can
    /// show them as dead.
    pub async fn list_by_endpoint(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
    ) -> Result<Vec<McpEndpointToken>> {
        let rows: Vec<McpEndpointTokenRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_ENDPOINT_TOKEN_COLS} FROM mcp_endpoint_tokens
             WHERE workspace_id = $1 AND endpoint_id = $2
             ORDER BY created_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Revoke a token by id, scoped to its workspace + endpoint (a foreign id
    /// is [`StoreError::NotFound`]). Idempotent: re-revoking an already-revoked
    /// token succeeds.
    pub async fn revoke(
        &self,
        workspace_id: WorkspaceId,
        endpoint_id: McpEndpointId,
        id: Uuid,
    ) -> Result<()> {
        let res = sqlx::query(
            "UPDATE mcp_endpoint_tokens SET revoked_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 AND endpoint_id = $3 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(endpoint_id.into_uuid())
        .execute(&self.pool)
        .await
        .map_err(map)?;
        if res.rows_affected() == 0 {
            // Distinguish "already revoked" (idempotent success) from "no such
            // token" with a follow-up existence check.
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                     SELECT 1 FROM mcp_endpoint_tokens
                     WHERE id = $1 AND workspace_id = $2 AND endpoint_id = $3)",
            )
            .bind(id)
            .bind(workspace_id.into_uuid())
            .bind(endpoint_id.into_uuid())
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
            if !exists {
                return Err(StoreError::NotFound);
            }
        }
        Ok(())
    }

    /// Garbage-collect tokens past their expiry. Returns the count removed.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res =
            sqlx::query("DELETE FROM mcp_endpoint_tokens WHERE expires_at <= CURRENT_TIMESTAMP")
                .execute(&self.pool)
                .await
                .map_err(map)?;
        Ok(res.rows_affected())
    }
}
