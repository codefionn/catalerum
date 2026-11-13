use catalerum_core::id::{McpServerId, WorkspaceId};
use catalerum_core::model::McpServerDef;
use sqlx::types::Json;
use crate::error::{Result, StoreError};
use crate::rows::McpServerRow;
use super::{map, PgPool};

const MCP_SERVER_COLS: &str = "id, workspace_id, name, transport, command, args, env, url, auth, \
     enabled, tools, created_at, updated_at";

/// The fields to create or upsert an [`McpServerDef`] (SOUL §26). `args`/`env`/
/// `auth`/`tools` ride as JSONB; `auth` carries credentials verbatim, so a
/// `NewMcpServerDef` is sensitive — never log it.
#[derive(Clone, Debug)]
pub struct NewMcpServerDef {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub url: String,
    pub auth: catalerum_core::model::McpAuthSpec,
    pub enabled: bool,
    pub tools: Vec<String>,
}

/// CRUD for the `mcp_servers` table (SOUL §26). Every query is workspace-scoped
/// (§18). Mirrors [`AgentProfileRepo`](crate::repo::AgentProfileRepo).
#[derive(Clone, Debug)]
pub struct McpServerRepo {
    pool: PgPool,
}

impl McpServerRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Create a server. [`StoreError::Conflict`] if `name` already exists in the
    /// workspace (the `(workspace_id, name)` unique key).
    pub async fn create(
        &self,
        workspace_id: WorkspaceId,
        server: &NewMcpServerDef,
    ) -> Result<McpServerDef> {
        let row: McpServerRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO mcp_servers
                 (id, workspace_id, name, transport, command, args, env, url, auth, enabled, tools)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING {MCP_SERVER_COLS}"
        ))
        .bind(McpServerId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&server.name)
        .bind(&server.transport)
        .bind(&server.command)
        .bind(Json(server.args.clone()))
        .bind(Json(server.env.clone()))
        .bind(&server.url)
        .bind(Json(server.auth.clone()))
        .bind(server.enabled)
        .bind(Json(server.tools.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Idempotently upsert a server by `(workspace_id, name)` — create it, or
    /// replace its definition (the `edit_mcp_server` create-or-replace semantics).
    pub async fn upsert_by_name(
        &self,
        workspace_id: WorkspaceId,
        server: &NewMcpServerDef,
    ) -> Result<McpServerDef> {
        let row: McpServerRow = sqlx::query_as(assert_sql_safe!(
            "INSERT INTO mcp_servers
                 (id, workspace_id, name, transport, command, args, env, url, auth, enabled, tools)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (workspace_id, name) DO UPDATE SET
                 transport = EXCLUDED.transport,
                 command = EXCLUDED.command,
                 args = EXCLUDED.args,
                 env = EXCLUDED.env,
                 url = EXCLUDED.url,
                 auth = EXCLUDED.auth,
                 enabled = EXCLUDED.enabled,
                 tools = EXCLUDED.tools,
                 updated_at = CURRENT_TIMESTAMP
             RETURNING {MCP_SERVER_COLS}"
        ))
        .bind(McpServerId::new().into_uuid())
        .bind(workspace_id.into_uuid())
        .bind(&server.name)
        .bind(&server.transport)
        .bind(&server.command)
        .bind(Json(server.args.clone()))
        .bind(Json(server.env.clone()))
        .bind(&server.url)
        .bind(Json(server.auth.clone()))
        .bind(server.enabled)
        .bind(Json(server.tools.clone()))
        .fetch_one(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.into())
    }

    /// Fetch a server by name, workspace-scoped (how the `*_mcp_server` tools
    /// resolve it), or `None`.
    pub async fn get_by_name(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<Option<McpServerDef>> {
        let row: Option<McpServerRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE workspace_id = $1 AND name = $2"
        ))
        .bind(workspace_id.into_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(map)?;
        Ok(row.map(McpServerDef::from))
    }

    /// List a workspace's servers, by name.
    pub async fn list_by_workspace(&self, workspace_id: WorkspaceId) -> Result<Vec<McpServerDef>> {
        let rows: Vec<McpServerRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE workspace_id = $1 \
             ORDER BY name ASC, id ASC"
        ))
        .bind(workspace_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(McpServerDef::from).collect())
    }

    /// List every **enabled** server across all workspaces — the boot-time load
    /// that reconnects DB-defined servers (SOUL §26). Not workspace-scoped: it
    /// runs at startup before any request, to rebuild the live tool set.
    pub async fn list_enabled(&self) -> Result<Vec<McpServerDef>> {
        let rows: Vec<McpServerRow> = sqlx::query_as(assert_sql_safe!(
            "SELECT {MCP_SERVER_COLS} FROM mcp_servers WHERE enabled = TRUE \
             ORDER BY workspace_id ASC, name ASC, id ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows.into_iter().map(McpServerDef::from).collect())
    }

    /// Delete a server by name, workspace-scoped. [`StoreError::NotFound`] if
    /// absent.
    pub async fn delete_by_name(&self, workspace_id: WorkspaceId, name: &str) -> Result<()> {
        let res = sqlx::query("DELETE FROM mcp_servers WHERE workspace_id = $1 AND name = $2")
            .bind(workspace_id.into_uuid())
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(map)?;
        if res.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }
}
