use catalerum_core::id::{UiDefinitionId, WorkspaceId};
use catalerum_core::model::{Author, UiDefinition};
use catalerum_core::model_ui::UiSpec;
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::{author_to_parts, UiDefinitionRow};
use super::{map, PgPool};

// ===========================================================================
// emerged UIs
// ===========================================================================

const UI_DEF_COLS: &str = "id, workspace_id, author_kind, author_id, name, title, description, \
     spec_version, version, definition, created_at, updated_at";

/// The mutable fields of an emerged UI, shared by create + update (keeps both
/// repository calls under the argument-count lint, like the other `New*` inputs).
#[derive(Clone, Debug)]
pub struct UiDefinitionInput {
    /// Optional slug, unique-when-set per workspace.
    pub name: Option<String>,
    /// Human title.
    pub title: String,
    /// Optional description.
    pub description: Option<String>,
    /// The component tree.
    pub definition: UiSpec,
}

/// CRUD for the `ui_definitions` table — AI-authored emerged UIs (declarative
/// component trees). Every query is workspace-filtered (SOUL §6.1/§18); edits
/// use an optimistic `version` counter so concurrent patches don't clobber.
#[derive(Clone, Debug)]
pub struct UiDefinitionRepo {
    pool: PgPool,
}

impl UiDefinitionRepo {
    /// Wrap a pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create an emerged UI, authored by `author`. `input.name` is an optional
    /// slug, unique-when-set per workspace ([`StoreError::Conflict`] on
    /// collision). Returns the stored definition at `version = 1`.
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        author: Author,
        spec_version: u32,
        input: &UiDefinitionInput,
    ) -> Result<UiDefinition> {
        let id = UiDefinitionId::new().into_uuid();
        let (author_kind, author_id) = author_to_parts(author);
        let row: UiDefinitionRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO ui_definitions
                 (id, workspace_id, author_kind, author_id, name, title, description,
                  spec_version, definition)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             RETURNING {UI_DEF_COLS}"
        ))
        .bind(id)
        .bind(workspace_id.into_uuid())
        .bind(author_kind)
        .bind(author_id)
        .bind(input.name.as_deref())
        .bind(&input.title)
        .bind(input.description.as_deref())
        .bind(i32::try_from(spec_version).unwrap_or(i32::MAX))
        .bind(Json(input.definition.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an emerged UI by id, scoped to its workspace.
    pub async fn get(&self, workspace_id: WorkspaceId, id: UiDefinitionId) -> Result<UiDefinition> {
        let row: UiDefinitionRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// Fetch an emerged UI by its (workspace-unique) name.
    pub async fn get_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<UiDefinition> {
        let row: UiDefinitionRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        row.try_into()
    }

    /// List a workspace's emerged UIs, most-recently-edited first. The
    /// `definition` JSONB is returned too; callers wanting a compact list should
    /// project the fields they need.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<UiDefinition>> {
        let rows: Vec<UiDefinitionRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {UI_DEF_COLS} FROM ui_definitions
             WHERE workspace_id = $1
             ORDER BY updated_at DESC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        rows.into_iter().map(UiDefinition::try_from).collect()
    }

    /// Optimistically update a UI's metadata + definition, bumping `version`.
    /// The update only applies when the stored `version` equals `expected`,
    /// guarding against a concurrent edit ([`StoreError::Conflict`] otherwise).
    /// The author and `spec_version` are immutable here.
    pub async fn update_definition(
        &self,
        workspace_id: WorkspaceId,
        id: UiDefinitionId,
        expected: i64,
        input: &UiDefinitionInput,
    ) -> Result<UiDefinition> {
        let row: Option<UiDefinitionRow> = sqlx::query_as(assert_sql_safe!(
            "UPDATE ui_definitions
                SET name = $4, title = $5, description = $6, definition = $7,
                    version = version + 1, updated_at = CURRENT_TIMESTAMP
             WHERE id = $1 AND workspace_id = $2 AND version = $3
             RETURNING {UI_DEF_COLS}"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(expected)
        .bind(input.name.as_deref())
        .bind(&input.title)
        .bind(input.description.as_deref())
        .bind(Json(input.definition.clone()))
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        match row {
            Some(r) => r.try_into(),
            None => Err(StoreError::Conflict(format!(
                "ui {id}: stale version {expected} (or not found in workspace)"
            ))),
        }
    }

    /// Delete an emerged UI, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: UiDefinitionId) -> Result<()> {
        let res = sqlx::query("DELETE FROM ui_definitions WHERE id = $1 AND workspace_id = $2")
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
