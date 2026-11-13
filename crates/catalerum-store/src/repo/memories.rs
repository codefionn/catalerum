use catalerum_core::id::{MemoryId, UserId, WorkspaceId};
use catalerum_core::model::{Memory, MemoryScope, SourceRef};
#[cfg(not(feature = "sqlite"))]
use uuid::Uuid;
use crate::error::{Result, StoreError};
use crate::rows::{memory_scope_to_text, MemoryRow, source_to_parts};
use super::{map, PgPool};

// ===========================================================================
// Memories (personalization, SOUL §22)
// ===========================================================================

/// The full set of `memories` columns, in row order (for `SELECT`/`RETURNING`).
const MEMORY_COLS: &str =
    "id, workspace_id, scope, user_id, text, source_kind, source_id, point_id, created_at, updated_at";

/// CRUD for the `memories` table — durable, curated, inspectable facts (SOUL §22).
/// `scope` is `User` (private to one member) or `Workspace` (shared); recall
/// applies the user-visibility filter so a member never sees another member's
/// private memories.
#[derive(Clone, Debug)]
pub struct MemoryRepo {
    pool: PgPool,
}

impl MemoryRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a memory in a workspace. `user_id` is required for `User` scope
    /// (the member it is private to) and ignored for `Workspace` scope. `source`
    /// optionally records where it was derived from (e.g. a conversation).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        scope: MemoryScope,
        user_id: Option<UserId>,
        text: &str,
        source: Option<&SourceRef>,
    ) -> Result<Memory> {
        // A workspace-scoped memory is never tied to a member.
        let user_id = match scope {
            MemoryScope::User => user_id,
            MemoryScope::Workspace => None,
        };
        let (source_kind, source_id) = match source {
            Some(s) => {
                let (k, i) = source_to_parts(s);
                (Some(k), Some(i))
            }
            None => (None, None),
        };
        let row: MemoryRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO memories
                 (id, workspace_id, scope, user_id, text, source_kind, source_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             RETURNING {MEMORY_COLS}"
        ))
        .bind(MemoryId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(memory_scope_to_text(scope))
        .bind(user_id.map(UserId::into_uuid))
        .bind(text)
        .bind(source_kind)
        .bind(source_id)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch a memory, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {MEMORY_COLS} FROM memories WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch multiple memories by id in **one** round-trip, workspace-scoped. Ids
    /// with no matching row are simply absent (no error) and order is unspecified,
    /// so callers index the result by id. Lets a batch of semantic-search hits be
    /// visibility-filtered without a `get` per hit on the hot chat-recall path.
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MemoryId],
    ) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(MemoryId::as_uuid).collect();
        let rows: Vec<MemoryRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MEMORY_COLS} FROM memories WHERE workspace_id = $1 AND id = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many(
        &self,
        workspace_id: WorkspaceId,
        ids: &[MemoryId],
    ) -> Result<Vec<Memory>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {MEMORY_COLS} FROM memories WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND id IN (");
        let mut values = query.separated(", ");
        for id in ids {
            values.push_bind(id.as_uuid());
        }
        values.push_unseparated(")");
        let rows: Vec<MemoryRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    /// Memories **visible to `user_id`** in a workspace, most-recent first: all
    /// `Workspace`-scoped ones plus the acting user's own `User`-scoped ones.
    /// With `user_id = None` (an agent run with no member), only workspace-scoped
    /// memories are visible — a member's private memories are never leaked (§22).
    pub async fn list_visible(
        &self,
        workspace_id: WorkspaceId,
        user_id: Option<UserId>,
        limit: i64,
    ) -> Result<Vec<Memory>> {
        let rows: Vec<MemoryRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MEMORY_COLS} FROM memories
             WHERE workspace_id = $1
               AND (scope = 'workspace' OR ($2::uuid IS NOT NULL AND user_id = $2))
             ORDER BY created_at DESC, id ASC
             LIMIT $3"
        ))
        .bind(workspace_id.into_uuid())
        .bind(user_id.map(UserId::into_uuid))
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(Memory::try_from).collect()
    }

    /// Replace a memory's `text` (workspace-scoped), bumping `updated_at`.
    /// [`StoreError::NotFound`] if no such memory exists in the workspace.
    pub async fn update_text(
        &self,
        workspace_id: WorkspaceId,
        id: MemoryId,
        text: &str,
    ) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE memories SET text = $3, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MEMORY_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(text)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Bump a memory's `updated_at` **without** changing its text — the "reaffirm"
    /// applied on a dedup hit (SOUL §22/§29). When a `remember` / auto-curate store
    /// turns out to duplicate this row we skip the insert but touch it, so recency
    /// signals the fact was seen again while the row's id/created_at/scope are kept.
    /// Returns the (unchanged-text) row. [`StoreError::NotFound`] if absent.
    pub async fn touch(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<Memory> {
        let row: MemoryRow = sqlx::query_as(assert_sql_safe!(
            "UPDATE memories SET updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2
             RETURNING {MEMORY_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Delete a memory, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: MemoryId) -> Result<()> {
        let res = sqlx::query("DELETE FROM memories WHERE id = $1 AND workspace_id = $2")
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
