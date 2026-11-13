use catalerum_core::id::{SkillId, WorkspaceId};
use catalerum_core::model::{Code, Skill};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::SkillRow;
use super::{map, PgPool};

// ===========================================================================
// Skills (SOUL §23)
// ===========================================================================

const SKILL_COLS: &str = "id, workspace_id, name, description, instructions_md, tools, code, \
     advertised, created_at, updated_at";

/// The fields to create or upsert a [`Skill`] (SOUL §23).
#[derive(Clone, Debug)]
pub struct NewSkill {
    /// Unique (per workspace) skill name — how it is invoked.
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Markdown runbook / instructions.
    pub instructions_md: String,
    /// Tool names the skill may use (a subset of the registry).
    pub tools: Vec<String>,
    /// Optional executable code (run via the Executor §20).
    pub code: Option<Code>,
    /// Whether the skill's name + description are advertised in the chat system
    /// prompt ("visible to agent", SOUL §23). `true` by default.
    pub advertised: bool,
}

/// CRUD for the `skills` table (SOUL §23). Skills are named, workspace-scoped,
/// and invoked by name via the `use_skill` tool.
#[derive(Clone, Debug)]
pub struct SkillRepo {
    pool: PgPool,
}

impl SkillRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a skill. Errors with [`StoreError::Conflict`] if `name` already
    /// exists in the workspace (the `(workspace_id, name)` unique key).
    pub async fn create(&self, workspace_id: WorkspaceId, skill: &NewSkill) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO skills
                 (id, workspace_id, name, description, instructions_md, tools, code, advertised)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING {SKILL_COLS}"
        ))
        .bind(SkillId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&skill.name)
        .bind(&skill.description)
        .bind(&skill.instructions_md)
        .bind(Json(skill.tools.clone()))
        .bind(skill.code.as_ref().map(Json))
        .bind(skill.advertised)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a skill by `(workspace_id, name)` — create it, or
    /// replace its definition. Used to seed first-party skills (SOUL §23).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        skill: &NewSkill,
    ) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO skills
                 (id, workspace_id, name, description, instructions_md, tools, code, advertised)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 description = EXCLUDED.description,
                 instructions_md = EXCLUDED.instructions_md,
                 tools = EXCLUDED.tools,
                 code = EXCLUDED.code,
                 advertised = EXCLUDED.advertised,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {SKILL_COLS}"
        ))
        .bind(SkillId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&skill.name)
        .bind(&skill.description)
        .bind(&skill.instructions_md)
        .bind(Json(skill.tools.clone()))
        .bind(skill.code.as_ref().map(Json))
        .bind(skill.advertised)
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a skill by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: SkillId) -> Result<Skill> {
        let row: SkillRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {SKILL_COLS} FROM skills WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a skill by name (how `use_skill` resolves it), or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<Skill>> {
        let row: Option<SkillRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(Skill::from))
    }

    /// Fetch multiple skills by name in **one** query (absent names omitted), so a
    /// caller resolving an agent's named skills avoids an N+1 of
    /// [`get_by_name`](Self::get_by_name).
    #[cfg(not(feature = "sqlite"))]
    pub async fn get_many_by_name(
        &self,
        workspace_id: WorkspaceId,
        names: &[String],
    ) -> Result<Vec<Skill>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<SkillRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 AND name = ANY($2)"
        ))
        .bind(workspace_id.into_uuid())
        .bind(names)
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    #[cfg(feature = "sqlite")]
    pub async fn get_many_by_name(
        &self,
        workspace_id: WorkspaceId,
        names: &[String],
    ) -> Result<Vec<Skill>> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<sqlx::Sqlite>::new(format!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = "
        ));
        query
            .push_bind(workspace_id.into_uuid())
            .push(" AND name IN (");
        let mut values = query.separated(", ");
        for name in names {
            values.push_bind(name);
        }
        values.push_unseparated(")");
        let rows: Vec<SkillRow> = query
            .build_query_as()
            .fetch_all(&self.pool)
            .await
            .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    /// `(name, description)` of every **advertised** skill in the workspace, by
    /// name — the lean read the chat system prompt's skill-advertising block
    /// makes every turn (SOUL §23), skipping the runbooks/code `SKILL_COLS`
    /// would drag along.
    pub async fn list_advertised(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, description FROM skills
             WHERE workspace_id = $1 AND advertised
             ORDER BY name ASC, id ASC",
        )
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows)
    }

    /// List a workspace's skills, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<Skill>> {
        let rows: Vec<SkillRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {SKILL_COLS} FROM skills WHERE workspace_id = $1 ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(Skill::from).collect())
    }

    /// Delete a skill, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: SkillId) -> Result<()> {
        let res = sqlx::query("DELETE FROM skills WHERE id = $1 AND workspace_id = $2")
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
