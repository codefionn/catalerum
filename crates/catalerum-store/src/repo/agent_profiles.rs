use catalerum_core::id::{AgentProfileId, GrantId, WorkspaceId};
use catalerum_core::model::{AgentProfile, ToolGuard};
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::AgentProfileRow;
use super::{map, PgPool};

// ===========================================================================
// Agent profiles (SOUL §19/§25)
// ===========================================================================

const AGENT_PROFILE_COLS: &str = "id, workspace_id, name, model, system_prompt, tools, skills, \
     subagents, channels, grant_id, guard, created_at, updated_at";

/// The fields to create or upsert an [`AgentProfile`] (SOUL §19). The name lists
/// (`tools`/`skills`/`subagents`/`channels`) and the optional `grant_id` bound the
/// profile's authority and behaviour.
#[derive(Clone, Debug)]
pub struct NewAgentProfile {
    /// Unique (per workspace) profile name.
    pub name: String,
    /// Model id to run against; `None` uses the workspace default.
    pub model: Option<String>,
    /// System prompt; `None` uses the default agent system prompt.
    pub system_prompt: Option<String>,
    /// Tool names the profile may dispatch (subset of the registry); empty = all.
    pub tools: Vec<String>,
    /// Skill names whose runbooks seed the system prompt.
    pub skills: Vec<String>,
    /// Agent-profile names this profile may delegate to (subagents).
    pub subagents: Vec<String>,
    /// Channel names this profile listens on.
    pub channels: Vec<String>,
    /// The §19 grant that is this profile's authority (`None` = base Member).
    pub grant_id: Option<GrantId>,
    /// Optional per-profile tool guard (SOUL §19); `None` leaves the profile gated
    /// only by its capabilities.
    pub guard: Option<ToolGuard>,
}

/// CRUD for the `agent_profiles` table (SOUL §19/§25). Profiles are named,
/// workspace-scoped scoped-agent configurations; the API resolves them by name
/// and the channel router resolves the ones listening on a channel.
#[derive(Clone, Debug)]
pub struct AgentProfileRepo {
    pool: PgPool,
}

impl AgentProfileRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a profile. [`StoreError::Conflict`] if `name` already exists in the
    /// workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        profile: &NewAgentProfile,
    ) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO agent_profiles
                 (id, workspace_id, name, model, system_prompt, tools, skills, subagents,
                  channels, grant_id, guard)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {AGENT_PROFILE_COLS}"
        ))
        .bind(AgentProfileId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&profile.name)
        .bind(&profile.model)
        .bind(&profile.system_prompt)
        .bind(Json(profile.tools.clone()))
        .bind(Json(profile.skills.clone()))
        .bind(Json(profile.subagents.clone()))
        .bind(Json(profile.channels.clone()))
        .bind(profile.grant_id.map(GrantId::into_uuid))
        .bind(profile.guard.as_ref().map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a profile by `(workspace_id, name)` — create it, or
    /// replace its definition (create-or-replace REST semantics).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        profile: &NewAgentProfile,
    ) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO agent_profiles
                 (id, workspace_id, name, model, system_prompt, tools, skills, subagents,
                  channels, grant_id, guard)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 model = EXCLUDED.model,
                 system_prompt = EXCLUDED.system_prompt,
                 tools = EXCLUDED.tools,
                 skills = EXCLUDED.skills,
                 subagents = EXCLUDED.subagents,
                 channels = EXCLUDED.channels,
                 grant_id = EXCLUDED.grant_id,
                 guard = EXCLUDED.guard,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {AGENT_PROFILE_COLS}"
        ))
        .bind(AgentProfileId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&profile.name)
        .bind(&profile.model)
        .bind(&profile.system_prompt)
        .bind(Json(profile.tools.clone()))
        .bind(Json(profile.skills.clone()))
        .bind(Json(profile.subagents.clone()))
        .bind(Json(profile.channels.clone()))
        .bind(profile.grant_id.map(GrantId::into_uuid))
        .bind(profile.guard.as_ref().map(Json))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a profile by id, workspace-scoped.
    pub async fn get(&self, workspace_id: WorkspaceId, id: AgentProfileId) -> Result<AgentProfile> {
        let row: AgentProfileRow = sqlx::query_as(assert_sql_safe!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE id = $1 AND workspace_id = $2"
        ))
        .bind(id.into_uuid())
        .bind(workspace_id.into_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a profile by name (how the API + the `delegate` tool resolve it), or
    /// `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<AgentProfile>> {
        let row: Option<AgentProfileRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(AgentProfile::from))
    }

    /// List a workspace's profiles, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<AgentProfile>> {
        let rows: Vec<AgentProfileRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles WHERE workspace_id = $1 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(AgentProfile::from).collect())
    }

    /// List the profiles **listening on** `channel` (its name is a member of the
    /// `channels` JSONB array), workspace-scoped — the channel→profile inbound
    /// routing lookup (SOUL §25). `@>` containment checks the array holds the name.
    pub async fn list_by_channel(
        &self,
        workspace_id: WorkspaceId,
        channel: &str,
    ) -> Result<Vec<AgentProfile>> {
        let rows: Vec<AgentProfileRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {AGENT_PROFILE_COLS} FROM agent_profiles \
             WHERE workspace_id = $1 AND channels @> $2 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .bind(Json(vec![channel.to_string()]))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(AgentProfile::from).collect())
    }

    /// Delete a profile, workspace-scoped. [`StoreError::NotFound`] if absent.
    pub async fn delete(&self, workspace_id: WorkspaceId, id: AgentProfileId) -> Result<()> {
        let res = sqlx::query("DELETE FROM agent_profiles WHERE id = $1 AND workspace_id = $2")
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
