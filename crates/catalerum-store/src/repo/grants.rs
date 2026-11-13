use catalerum_core::capability::{Capability, Constraints};
use catalerum_core::id::{GrantId, WorkspaceId};
use catalerum_core::model::Grant;
use sqlx::types::Json;
use crate::error::Result;
use crate::rows::GrantRow;
use super::{map, PgPool};

/// The full set of `mailboxes` columns, in row order.
const GRANT_COLS: &str = "id, workspace_id, name, capabilities, constraints, created_at";

/// CRUD for the `grants` table (SOUL §19) — named capability bundles a workspace
/// Owner/Admin defines. An automation runs *under* a grant (its attenuated
/// authority); the runtime enforcement that resolves an automation's grant into
/// its `ToolContext` capabilities is a follow-up slice. Workspace-scoped (§18).
#[derive(Clone, Debug)]
pub struct GrantRepo {
    pool: PgPool,
}

impl GrantRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create (or, on a name conflict, **replace**) a grant. Idempotent by
    /// `(workspace_id, name)` so re-defining a named grant refreshes its
    /// capabilities + constraints; the id is preserved.
    pub async fn upsert(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
        capabilities: &[Capability],
        constraints: &Constraints,
    ) -> Result<Grant> {
        let id = GrantId::new().into_uuid();
        let row: GrantRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO grants (id, workspace_id, name, capabilities, constraints)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (workspace_id, name)
             DO UPDATE SET capabilities = EXCLUDED.capabilities,
                           constraints  = EXCLUDED.constraints
             RETURNING {GRANT_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(name)
        .bind(Json(capabilities.to_vec()))
        .bind(Json(constraints.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a grant, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: GrantId) -> Result<Grant> {
        let row: GrantRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {GRANT_COLS} FROM grants WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// List a workspace's grants, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Grant>> {
        let rows: Vec<GrantRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {GRANT_COLS} FROM grants WHERE workspace_id = $1 ORDER BY name ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Grant::from).collect())
    }

    /// Delete a grant, workspace-scoped. Returns whether a row was removed
    /// (idempotent). An automation referencing it has its `grant_id` nulled by the
    /// FK (`ON DELETE SET NULL`), falling back to its default authority.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: GrantId) -> Result<bool> {
        let res = sqlx::query("DELETE FROM grants WHERE id = $1 AND workspace_id = $2")
            .bind(id.into_uuid())
            .bind(workspace_id.into_uuid())
            .execute(&self.pool)
            .await
            .map_err(map)?;
        Ok(res.rows_affected() > 0)
    }
}
