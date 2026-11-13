use catalerum_core::id::WorkspaceId;
use chrono::{DateTime, Utc};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::AppDataRow;
use super::{map, PgPool};

// ===========================================================================
// Per-App durable key/value store (SOUL §12/§29)
// ===========================================================================

const APP_DATA_COLS: &str = "app, key, value, created_at, updated_at";

/// Max serialized bytes of a single stored value. Mirrors the `initial_state`
/// cap philosophy (SOUL §12): a value is a small JSON document (a saved layout, a
/// per-user tracker), not a blob — files/external Postgres are the home for bulk.
pub const MAX_APP_DATA_VALUE_BYTES: usize = 64 * 1024;

/// Max keys one App (one `(workspace, app)` namespace) may hold. Bounds the store
/// so an App cannot grow unbounded rows; a data model that outgrows this belongs
/// in a first-class store (SOUL §12).
pub const MAX_APP_DATA_KEYS_PER_APP: i64 = 1_000;

/// One entry in the per-App key/value store: `(app, key) → value` plus timestamps.
#[derive(Clone, Debug)]
pub struct AppDataEntry {
    /// The App namespace this entry lives under (a UI id on the handler path).
    pub app: String,
    pub key: String,
    pub value: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<AppDataRow> for AppDataEntry {
    fn from(r: AppDataRow) -> Self {
        AppDataEntry {
            app: r.app,
            key: r.key,
            value: r.value.0,
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// The `app_data` table (SOUL §12/§29): a workspace-scoped `(app, key) → JSONB`
/// map the emerged-App tools (`app_data_get`/`set`/`list`/`delete`) are thin
/// clients of. Every method is workspace-filtered; the `app` namespace is chosen
/// by the *caller* of the tool (forced to the firing UI's id on the handler path,
/// so cross-App reads are impossible there — see the migration + the tools).
#[derive(Clone, Debug)]
pub struct AppDataRepo {
    pool: PgPool,
}

impl AppDataRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Fetch one entry by `(app, key)`, or `None` when unset. `app`/`key` are
    /// trimmed and must be non-empty.
    pub async fn get(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        key: &str,
    ) -> Result<Option<AppDataEntry>> {
        let (app, key) = Self::validate_key(app, key)?;
        let row: Option<AppDataRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {APP_DATA_COLS} FROM app_data
             WHERE workspace_id = $1 AND app = $2 AND key = $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Into::into))
    }

    /// Upsert `value` at `(app, key)`, returning the stored entry. Enforces the
    /// per-value byte cap ([`MAX_APP_DATA_VALUE_BYTES`]) and the per-App key cap
    /// ([`MAX_APP_DATA_KEYS_PER_APP`], only when introducing a *new* key — an
    /// update of an existing key never grows the row count). Idempotent on
    /// `(workspace, app, key)`.
    pub async fn set(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<AppDataEntry> {
        let (app, key) = Self::validate_key(app, key)?;
        // Byte-bound the value (serialize once; the bind below re-serializes, but
        // this is the authoritative size check the caller sees).
        let bytes = serde_json::to_vec(value).map_err(StoreError::decode)?;
        if bytes.len() > MAX_APP_DATA_VALUE_BYTES {
            return Err(StoreError::invalid(format!(
                "value is {} bytes (max {MAX_APP_DATA_VALUE_BYTES}); \
                 store large data in a file or external database instead",
                bytes.len()
            )));
        }
        // Row cap: reject a *new* key once the namespace is full (an update to an
        // existing key is always allowed). Best-effort check-then-insert — the cap
        // is a guardrail, not a hard transactional invariant.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM app_data
                 WHERE workspace_id = $1 AND app = $2 AND key = $3)",
        )
        .bind(workspace_id.into_uuid())
        .bind(&app)
        .bind(&key)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        if !exists {
            let count = self.count(workspace_id, &app).await?;
            if count >= MAX_APP_DATA_KEYS_PER_APP {
                return Err(StoreError::invalid(format!(
                    "app `{app}` already holds {count} keys (max {MAX_APP_DATA_KEYS_PER_APP})"
                )));
            }
        }
        let row: AppDataRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO app_data (workspace_id, app, key, value)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (workspace_id, app, key)
             DO UPDATE SET value = EXCLUDED.value, updated_at = CURRENT_TIMESTAMP
             RETURNING {APP_DATA_COLS}"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(key)
        .bind(Json(value))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Every entry in one `(workspace, app)` namespace, key-ordered, bounded to
    /// `limit` (floored at 1). `app` is trimmed and must be non-empty.
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
        app: &str,
        limit: i64,
    ) -> Result<Vec<AppDataEntry>> {
        let app = app.trim();
        if app.is_empty() {
            return Err(StoreError::invalid("`app` must not be empty"));
        }
        let rows: Vec<AppDataRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {APP_DATA_COLS} FROM app_data
             WHERE workspace_id = $1 AND app = $2
             ORDER BY key ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(app)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Number of keys in one `(workspace, app)` namespace (for the row cap + a
    /// `list` count).
    pub async fn count(&self, workspace_id: WorkspaceId, app: &str) -> Result<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM app_data WHERE workspace_id = $1 AND app = $2",
        )
        .bind(workspace_id.into_uuid())
        .bind(app.trim())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(n)
    }

    /// Delete one entry by `(app, key)`. Returns whether a row was removed
    /// (idempotent — deleting an absent key is a no-op returning `false`).
    pub async fn delete(&self, workspace_id: WorkspaceId, app: &str, key: &str) -> Result<bool> {
        let (app, key) = Self::validate_key(app, key)?;
        let done =
            sqlx::query("DELETE FROM app_data WHERE workspace_id = $1 AND app = $2 AND key = $3")
                .bind(workspace_id.into_uuid())
                .bind(app)
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(map)?;
        Ok(done.rows_affected() > 0)
    }

    /// Trim + non-empty-check an `(app, key)` pair, returning the cleaned owned
    /// pair or [`StoreError::Invalid`].
    fn validate_key(app: &str, key: &str) -> Result<(String, String)> {
        let app = app.trim();
        let key = key.trim();
        if app.is_empty() {
            return Err(StoreError::invalid("`app` must not be empty"));
        }
        if key.is_empty() {
            return Err(StoreError::invalid("`key` must not be empty"));
        }
        Ok((app.to_string(), key.to_string()))
    }
}
