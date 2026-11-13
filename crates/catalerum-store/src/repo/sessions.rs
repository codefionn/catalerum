use catalerum_core::id::{GrantId, UserId, WorkspaceId};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{LoginToken, Session};
use super::{map, PgPool};

// ===========================================================================
// Sessions
// ===========================================================================

/// CRUD for the `sessions` table (opaque server-side auth sessions).
#[derive(Clone, Debug)]
pub struct SessionRepo {
    pool: PgPool,
}

impl SessionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a session for `(workspace, user)`. `token_hash` must already be a
    /// hash of the raw token (the store never sees plaintext tokens). `grant_id`
    /// scopes the token to a named §19 grant (SOUL §19/§26) — its same-workspace
    /// composite FK rejects a cross-workspace/unknown grant at write time and
    /// cascade-revokes the session if the grant is later deleted; `None` is a
    /// role-derived session.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        token_hash: &str,
        grant_id: Option<GrantId>,
        expires_at: DateTime<Utc>,
    ) -> Result<Session> {
        let id = Uuid::new_v4();
        let row: Session = sqlx::query_as(
            "INSERT INTO sessions (id, workspace_id, user_id, token_hash, grant_id, expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(token_hash)
        .bind(grant_id.map(GrantId::into_uuid))
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Fetch a session by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: Uuid) -> Result<Session> {
        let row: Session = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions WHERE id = $1 AND workspace_id = $2",
        )
        .bind(id)
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Look up a session by its `token_hash` (the auth hot path). Expired
    /// sessions are excluded.
    ///
    /// Not workspace-filtered: this is how a bearer token resolves to its
    /// workspace in the first place, so no workspace is known yet.
    pub async fn get_by_token_hash(&self, token_hash: &str) -> Result<Session> {
        let row: Session = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions WHERE token_hash = $1 AND expires_at > CURRENT_TIMESTAMP",
        )
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// List a user's active (non-expired) sessions in one workspace. A user
    /// belonging to several workspaces holds sessions in each; only
    /// `workspace_id`'s are returned.
    pub async fn list_by_user(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
    ) -> Result<Vec<Session>> {
        let rows: Vec<Session> = sqlx::query_as(
            "SELECT id, workspace_id, user_id, token_hash, grant_id, created_at, expires_at
             FROM sessions
             WHERE workspace_id = $1 AND user_id = $2 AND expires_at > CURRENT_TIMESTAMP
             ORDER BY created_at DESC",
        )
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows)
    }

    /// Revoke a single session by id, workspace-scoped.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: Uuid) -> Result<()> {
        let res = sqlx::query("DELETE FROM sessions WHERE id = $1 AND workspace_id = $2")
            .bind(id)
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Garbage-collect expired sessions. Returns the number deleted. Global
    /// housekeeping across all workspaces, run by the server, never a tenant.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM sessions WHERE expires_at <= CURRENT_TIMESTAMP")
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }
}

// ===========================================================================
// Login tokens
// ===========================================================================

/// CRUD for the `login_tokens` table (one-time magic-link tokens, SOUL §18).
///
/// The store only ever sees a `token_hash` — a hash of the raw token handed to
/// the caller. Tokens are consumed exactly once via [`Self::consume`], which
/// atomically flips `consumed_at` only if it is still `NULL`.
#[derive(Clone, Debug)]
pub struct LoginTokenRepo {
    pool: PgPool,
}

impl LoginTokenRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a one-time login token for `(workspace, user)`. `token_hash` must
    /// already be a hash of the raw token (the store never sees plaintext).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        user_id: UserId,
        token_hash: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<LoginToken> {
        let row: LoginToken = sqlx::query_as(
            "INSERT INTO login_tokens (token_hash, workspace_id, user_id, expires_at)
             VALUES ($1, $2, $3, $4)
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at, consumed_at",
        )
        .bind(token_hash)
        .bind(workspace_id.into_uuid())
        .bind(user_id.into_uuid())
        .bind(expires_at)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Look up a login token by its `token_hash`, or [`StoreError::NotFound`].
    /// Not workspace-filtered: redeeming the token is what establishes the
    /// workspace.
    /// Returns the row regardless of consumption/expiry so callers can
    /// disambiguate "unknown" from "already consumed" / "expired".
    pub async fn get_by_token_hash(&self, token_hash: &str) -> Result<LoginToken> {
        let row: LoginToken = sqlx::query_as(
            "SELECT token_hash, workspace_id, user_id, created_at, expires_at, consumed_at
             FROM login_tokens WHERE token_hash = $1",
        )
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row)
    }

    /// Atomically consume a login token: flip `consumed_at` to `consumed_at`
    /// only if it was still `NULL`. Returns the row as it was **before**
    /// consumption (i.e. `consumed_at` is `NULL` in the returned value).
    ///
    /// Returns [`StoreError::NotFound`] if the token is unknown **or** was
    /// already consumed — the caller can distinguish the two by following up
    /// with [`Self::get_by_token_hash`].
    pub async fn consume(
        &self,
        token_hash: &str,
        consumed_at: DateTime<Utc>,
    ) -> Result<LoginToken> {
        #[cfg(not(feature = "sqlite"))]
        let statement = "UPDATE login_tokens SET consumed_at = $2
             WHERE token_hash = $1 AND consumed_at IS NULL
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at,
                       NULL::timestamptz AS consumed_at";
        #[cfg(feature = "sqlite")]
        let statement = "UPDATE login_tokens SET consumed_at = $2
             WHERE token_hash = $1 AND consumed_at IS NULL
             RETURNING token_hash, workspace_id, user_id, created_at, expires_at,
                       NULL AS consumed_at";
        let row: LoginToken = sqlx::query_as(statement)
            .bind(token_hash)
            .bind(consumed_at)
            .fetch_one(&self.pool)
            .await
            .map_err(map)?;
        Ok(row)
    }

    /// Garbage-collect expired (and not-yet-consumed) tokens. Returns the count.
    pub async fn delete_expired(&self) -> Result<u64> {
        let res = sqlx::query("DELETE FROM login_tokens WHERE expires_at <= CURRENT_TIMESTAMP")
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected())
    }
}
