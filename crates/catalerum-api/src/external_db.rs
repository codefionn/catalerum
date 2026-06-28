//! External PostgreSQL connections (SOUL §11/§19).
//!
//! A workspace can attach external Postgres databases it owns. Each is a
//! [`ConnectionKind::Postgres`](catalerum_core::model::ConnectionKind::Postgres)
//! connection row whose `config` blob ([`PostgresConnectionConfig`]) holds the
//! host/port/database/username/options, while the password lives **encrypted**
//! in the secret store, referenced by the row's `credential_ref`.
//!
//! [`ExternalDbRegistry`] owns the lazily-built, cached [`PgPool`] per connection
//! and is shared through `AppState`. It is the single place that decrypts a
//! credential and opens a socket to an external server; the `sql_query` tool,
//! managed-schema tools, and schema-migration engine all go through it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value as Json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::error::Result as CoreResult;
use catalerum_core::id::{ConnectionId, WorkspaceId};
use catalerum_core::model::ConnectionKind;
use catalerum_core::tool::{Tool, ToolContext, ToolRegistry};
use catalerum_core::Error as CoreError;
use catalerum_store::{
    connect_external, ping_external_pool, PgPool, PoolConfig, SecretStore, Store,
};

use crate::config::ExternalDbConnectionConfig;
use crate::db_migrate::{
    describe_schema, diff, introspect, list_schemas, quote_ident, schema_exists,
    validate_schema_prefix, DesiredSchema,
};

// The non-secret config blob lives in `catalerum-store` (shared with the ingest
// collect-SQL poller, which builds the same connect spec worker-side); re-export
// it so API-crate callers keep their `external_db::PostgresConnectionConfig` path.
pub use catalerum_store::PostgresConnectionConfig;

/// Lazily-built, cached connection pools for external Postgres databases, keyed
/// by connection id. One per process, shared via `Arc` in `AppState`. A pool is
/// built on first use (decrypting the credential) and reused thereafter; evict
/// on connection update/delete.
pub struct ExternalDbRegistry {
    store: Store,
    /// The credential vault; `None` when `[secrets].master_key` is unset — then
    /// any operation needing a credential fails closed with a clear message.
    secrets: Option<Arc<SecretStore>>,
    pools: RwLock<HashMap<ConnectionId, Arc<PgPool>>>,
    default_pool_max: u32,
    /// Hard per-session `statement_timeout` (ms) pinned on every external pool.
    statement_timeout_ms: u64,
    /// Immutable config base, keyed by workspace-visible connection name.
    configured: BTreeMap<String, ExternalDbConnectionConfig>,
    /// Workspaces reconciled during this process lifetime. The write guard also
    /// serializes the first-use reconcile for a workspace.
    reconciled_workspaces: RwLock<HashSet<WorkspaceId>>,
}

impl ExternalDbRegistry {
    #[must_use]
    pub fn new(
        store: Store,
        secrets: Option<Arc<SecretStore>>,
        default_pool_max: u32,
        statement_timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            secrets,
            pools: RwLock::new(HashMap::new()),
            default_pool_max,
            statement_timeout_ms,
            configured: BTreeMap::new(),
            reconciled_workspaces: RwLock::new(HashSet::new()),
        }
    }

    /// Attach config-defined connections. Kept as a builder so existing tests and
    /// embedders using [`Self::new`] retain their runtime-only behavior.
    #[must_use]
    pub fn with_configured(
        mut self,
        configured: BTreeMap<String, ExternalDbConnectionConfig>,
    ) -> Self {
        self.configured = configured;
        self
    }

    /// Whether credential encryption is available (`[secrets].master_key` set).
    #[must_use]
    pub fn secrets_available(&self) -> bool {
        self.secrets.is_some()
    }

    /// The secret store, or an error naming the missing configuration.
    pub fn secret_store(&self) -> Result<&Arc<SecretStore>, CoreError> {
        self.secrets.as_ref().ok_or_else(|| {
            CoreError::other(
                "secret store unavailable — set [secrets].master_key to store credentials",
            )
        })
    }

    /// Resolve (and cache) the pool for a Postgres connection, workspace-scoped.
    /// Builds the pool on first use by decrypting the credential and opening the
    /// socket; subsequent calls return the cached `Arc`.
    pub async fn pool(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
    ) -> Result<Arc<PgPool>, CoreError> {
        self.ensure_configured(workspace_id).await?;
        if let Some(p) = self.pools.read().await.get(&id) {
            return Ok(p.clone());
        }
        let (cfg, password) = self.resolve(workspace_id, id).await?;
        let spec = cfg.to_spec(password, Some(self.statement_timeout_ms));
        let pool_cfg = PoolConfig {
            max_connections: cfg.pool_max.unwrap_or(self.default_pool_max).max(1),
            min_connections: 0,
            acquire_timeout: Duration::from_secs(10),
        };
        let pool = Arc::new(
            connect_external(&spec, &pool_cfg)
                .await
                .map_err(CoreError::from)?,
        );
        // Double-checked insert: a concurrent builder may have cached first —
        // keep whichever landed, drop the loser when this `Arc` goes out of scope.
        let mut cache = self.pools.write().await;
        Ok(cache.entry(id).or_insert(pool).clone())
    }

    /// Reconcile every config-defined connection assigned to this workspace into
    /// an ordinary workspace-owned `postgres` connection row. This happens once
    /// per workspace/process, lazily, so workspaces created after boot receive
    /// their assigned connections on first access too. Row ids are preserved and
    /// credentials are encrypted in the existing workspace secret store.
    pub async fn ensure_configured(&self, workspace_id: WorkspaceId) -> Result<(), CoreError> {
        if self.configured.is_empty()
            || self
                .reconciled_workspaces
                .read()
                .await
                .contains(&workspace_id)
        {
            return Ok(());
        }

        let mut reconciled = self.reconciled_workspaces.write().await;
        if reconciled.contains(&workspace_id) {
            return Ok(());
        }

        let workspace = self
            .store
            .workspaces()
            .get(workspace_id)
            .await
            .map_err(CoreError::from)?;
        let existing = self
            .store
            .connections()
            .list_by_workspace(workspace_id)
            .await
            .map_err(CoreError::from)?;

        for (name, configured) in &self.configured {
            if !configured.assigned_to(&workspace) {
                continue;
            }
            validate_configured(name, configured)?;

            let previous_ref = existing
                .iter()
                .find(|connection| {
                    connection.kind == ConnectionKind::Postgres && connection.name == *name
                })
                .and_then(|connection| connection.credential_ref.clone());
            let password = configured.resolved_password().map_err(CoreError::invalid)?;
            let credential_ref = if password.is_empty() {
                None
            } else {
                let reference = previous_ref
                    .clone()
                    .unwrap_or_else(|| configured_credential_ref(workspace_id, name));
                self.secret_store()?
                    .put_at(workspace_id, &reference, password.as_bytes())
                    .await
                    .map_err(CoreError::from)?;
                Some(reference)
            };

            let config = serde_json::to_value(configured.postgres_config()).map_err(|e| {
                CoreError::other(format!(
                    "serializing configured external database `{name}`: {e}"
                ))
            })?;
            let result = self
                .store
                .connections()
                .reconcile_configured(
                    workspace_id,
                    ConnectionKind::Postgres,
                    name,
                    credential_ref.as_deref(),
                    config,
                )
                .await;
            result.map_err(CoreError::from)?;
            if previous_ref.as_deref() != credential_ref.as_deref() {
                if let (Some(secrets), Some(reference)) =
                    (self.secrets.as_ref(), previous_ref.as_deref())
                {
                    let _ = secrets.delete(workspace_id, reference).await;
                }
            }
        }

        reconciled.insert(workspace_id);
        Ok(())
    }

    /// List the workspace's Postgres connections after applying its immutable
    /// config base.
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<catalerum_core::model::Connection>, CoreError> {
        self.ensure_configured(workspace_id).await?;
        Ok(self
            .store
            .connections()
            .list_by_workspace(workspace_id)
            .await
            .map_err(CoreError::from)?
            .into_iter()
            .filter(|connection| connection.kind == ConnectionKind::Postgres)
            .collect())
    }

    /// Whether a named connection is supplied by immutable config for this
    /// workspace. Config-defined connections cannot be deleted through runtime
    /// APIs; remove the config entry (or its workspace assignment) instead.
    pub async fn is_configured(
        &self,
        workspace_id: WorkspaceId,
        name: &str,
    ) -> Result<bool, CoreError> {
        let workspace = self
            .store
            .workspaces()
            .get(workspace_id)
            .await
            .map_err(CoreError::from)?;
        Ok(self
            .configured
            .get(name)
            .is_some_and(|configured| configured.assigned_to(&workspace)))
    }

    /// Load + decrypt a connection's config and password without building a pool.
    async fn resolve(
        &self,
        workspace_id: WorkspaceId,
        id: ConnectionId,
    ) -> Result<(PostgresConnectionConfig, String), CoreError> {
        let row = self
            .store
            .connections()
            .get_row(workspace_id, id)
            .await
            .map_err(CoreError::from)?;
        if row.kind != "postgres" {
            return Err(CoreError::invalid(format!(
                "connection '{}' is not a postgres connection",
                row.name
            )));
        }
        let cfg: PostgresConnectionConfig = serde_json::from_value(row.config.0.clone())
            .map_err(|e| CoreError::invalid(format!("invalid postgres connection config: {e}")))?;
        let password = match row.credential_ref.as_deref() {
            Some(reference) => {
                let bytes = self
                    .secret_store()?
                    .get(workspace_id, reference)
                    .await
                    .map_err(CoreError::from)?;
                String::from_utf8(bytes)
                    .map_err(|_| CoreError::other("stored credential is not valid UTF-8"))?
            }
            None => String::new(),
        };
        Ok((cfg, password))
    }

    /// Evict a cached pool (after the connection is updated or deleted). The pool
    /// closes once the last outstanding `Arc` to it is dropped.
    pub async fn evict(&self, id: ConnectionId) {
        self.pools.write().await.remove(&id);
    }

    /// Establish (or reuse) the pool and run a `SELECT 1` liveness probe — the
    /// connectivity test for `POST /db/connections/{id}/test`.
    pub async fn test(&self, workspace_id: WorkspaceId, id: ConnectionId) -> Result<(), CoreError> {
        let pool = self.pool(workspace_id, id).await?;
        ping_external_pool(&pool).await.map_err(CoreError::from)
    }

    /// Resolve a connection reference (its id **or** its name) to `(id, name)`,
    /// verifying it is a Postgres connection. The name is what capability
    /// selectors are keyed on (`db:read@<name>`).
    async fn resolve_ref(
        &self,
        workspace_id: WorkspaceId,
        reference: &str,
    ) -> Result<(ConnectionId, String), CoreError> {
        self.ensure_configured(workspace_id).await?;
        if let Ok(uuid) = uuid::Uuid::parse_str(reference) {
            let id = ConnectionId::from_uuid(uuid);
            let c = self
                .store
                .connections()
                .get(workspace_id, id)
                .await
                .map_err(CoreError::from)?;
            if c.kind != ConnectionKind::Postgres {
                return Err(CoreError::invalid(format!(
                    "connection '{}' is not a postgres connection",
                    c.name
                )));
            }
            return Ok((id, c.name));
        }
        self.store
            .connections()
            .list_by_workspace(workspace_id)
            .await
            .map_err(CoreError::from)?
            .into_iter()
            .find(|c| c.kind == ConnectionKind::Postgres && c.name == reference)
            .map(|c| (c.id, c.name))
            .ok_or_else(|| {
                CoreError::invalid(format!("no postgres connection named '{reference}'"))
            })
    }
}

fn configured_credential_ref(workspace_id: WorkspaceId, name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(workspace_id.to_string().as_bytes());
    digest.update(b"\0external-db\0");
    digest.update(name.as_bytes());
    format!("cfg-db-{:x}", digest.finalize())
}

fn validate_configured(
    name: &str,
    configured: &ExternalDbConnectionConfig,
) -> Result<(), CoreError> {
    if name.trim().is_empty() {
        return Err(CoreError::invalid(
            "configured external database name must not be empty",
        ));
    }
    for (field, value) in [
        ("host", configured.host.as_str()),
        ("database", configured.database.as_str()),
        ("username", configured.username.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(CoreError::invalid(format!(
                "configured external database `{name}`: `{field}` must not be empty"
            )));
        }
    }
    if configured.port == 0 {
        return Err(CoreError::invalid(format!(
            "configured external database `{name}`: `port` must be greater than zero"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sql_query tool (SOUL §7/§11/§19)
// ---------------------------------------------------------------------------

/// The coarse class of a SQL statement, used to pick the read vs write path and
/// the capability required. Classification is best-effort by leading keyword; the
/// real read-only guarantee comes from [`catalerum_store::sql_run_read`] wrapping
/// the statement in a subquery (which Postgres forbids from modifying data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SqlClass {
    /// `SELECT` / `WITH` / `VALUES` / `TABLE` — subqueryable, read-only.
    Read,
    /// `INSERT` / `UPDATE` / `DELETE` / `MERGE` — data modification (DML).
    Write,
    /// `CREATE` / `ALTER` / `DROP` / … — schema change; routed to the migration
    /// API instead (needs `db:write@<conn>/schema`).
    Ddl,
    /// Anything else (`SET`, `COPY`, `SHOW`, admin) — refused here.
    Other,
}

/// The leading keyword of a statement, lowercased. Skips leading whitespace and
/// `--` line comments so a commented statement still classifies by its verb.
fn leading_keyword(sql: &str) -> String {
    let mut rest = sql.trim_start();
    while let Some(after) = rest.strip_prefix("--") {
        // Drop the rest of the comment line, then re-trim.
        rest = after
            .find('\n')
            .map_or("", |i| &after[i + 1..])
            .trim_start();
    }
    rest.split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn classify(sql: &str) -> SqlClass {
    match leading_keyword(sql).as_str() {
        "select" | "with" | "values" | "table" => SqlClass::Read,
        "insert" | "update" | "delete" | "merge" => SqlClass::Write,
        "create" | "alter" | "drop" | "truncate" | "grant" | "revoke" | "comment" | "vacuum"
        | "analyze" | "reindex" | "cluster" | "refresh" => SqlClass::Ddl,
        _ => SqlClass::Other,
    }
}

/// Deny-by-default per-call capability check. `None` capabilities = a trusted
/// internal caller (the framework convention, SOUL §19); `Some` = enforce.
fn enforce(ctx: &ToolContext, requested: &Capability) -> Result<(), CoreError> {
    match &ctx.capabilities {
        None => Ok(()),
        Some(caps) if caps.iter().any(|held| held.covers(requested)) => Ok(()),
        Some(_) => Err(CoreError::unauthorized(format!(
            "missing capability db:{}@{}",
            match requested.action {
                Action::Read => "read",
                Action::Write => "write",
                _ => "access",
            },
            requested.resource.selector.as_deref().unwrap_or("*"),
        ))),
    }
}

/// The `sql_query` tool (SOUL §7/§11): run a parameterized SQL statement against
/// an external Postgres connection the workspace owns. Reads (`SELECT`/…) require
/// `db:read@<conn>`; writes (`INSERT`/`UPDATE`/`DELETE`) require `db:write@<conn>`.
/// DDL is refused — schema changes go through the migration API. Reachable from
/// chat, agents, MCP, the emerged UI, and the `SqlQuery` automation action.
pub struct SqlQueryTool {
    external_db: Arc<ExternalDbRegistry>,
    max_rows: u64,
}

// ---------------------------------------------------------------------------
// External database schema tools (SOUL §7/§11/§19)
// ---------------------------------------------------------------------------

/// List the external PostgreSQL connections visible to this tool invocation.
/// Returning only identity avoids leaking connection configuration while giving
/// agents the stable name/id needed by every other external-database tool.
struct ListExternalDatabaseConnectionsTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for ListExternalDatabaseConnectionsTool {
    fn name(&self) -> &str {
        "list_external_database_connections"
    }

    fn description(&self) -> &str {
        "List the workspace's attached external PostgreSQL databases that the caller can read. \
         Use this before sql_query or the external-schema tools when the user has not supplied a \
         connection name; never guess a connection name. Returns non-secret id/name pairs only."
    }

    fn parameters_schema(&self) -> Json {
        json!({ "type": "object", "properties": {} })
    }

    async fn invoke(&self, _args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let workspace = ctx
            .workspace_id
            .ok_or_else(|| CoreError::unauthorized("no workspace in context"))?;
        let connections = self
            .external_db
            .list(workspace)
            .await?
            .into_iter()
            .filter(|connection| {
                enforce(
                    ctx,
                    &Capability::new(Action::Read, Resource::new("db", connection.name.clone())),
                )
                .is_ok()
            })
            .map(|connection| {
                json!({
                    "id": connection.id,
                    "name": connection.name,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "connections": connections }))
    }
}

fn desired_tables_schema() -> Json {
    json!({
        "type": "array",
        "description": "The desired tables in this prefix. Existing objects omitted from this list are never dropped.",
        "items": {
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "columns": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "type": { "type": "string", "description": "PostgreSQL type, for example uuid, text, bigint, or timestamptz." },
                            "nullable": { "type": "boolean", "description": "Defaults to true." },
                            "primary_key": { "type": "boolean", "description": "Used when creating a new table." },
                            "default": { "type": "string", "description": "Optional PostgreSQL default expression." }
                        },
                        "required": ["name", "type"]
                    }
                },
                "indexes": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "columns": { "type": "array", "items": { "type": "string" } },
                            "unique": { "type": "boolean" }
                        },
                        "required": ["name", "columns"]
                    }
                }
            },
            "required": ["name", "columns"]
        }
    })
}

fn required_arg<'a>(args: &'a Json, key: &str) -> Result<&'a str, CoreError> {
    args.get(key)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CoreError::invalid(format!("`{key}` is required")))
}

fn schema_prefix(args: &Json) -> Result<&str, CoreError> {
    let prefix = required_arg(args, "prefix")?;
    validate_schema_prefix(prefix)?;
    Ok(prefix)
}

fn desired_schema(args: &Json, prefix: &str) -> Result<DesiredSchema, CoreError> {
    let tables = args
        .get("tables")
        .cloned()
        .ok_or_else(|| CoreError::invalid("`tables` is required"))?;
    serde_json::from_value(json!({ "schema": prefix, "tables": tables }))
        .map_err(|error| CoreError::invalid(format!("invalid desired schema: {error}")))
}

fn render_schema_migration(raw: &[Json], prefix: &str) -> Result<Vec<String>, CoreError> {
    if raw.is_empty() || raw.len() > 100 {
        return Err(CoreError::invalid(
            "`statements` must contain between 1 and 100 DDL statements",
        ));
    }
    let quoted_prefix = quote_ident(prefix);
    let mut total_bytes = 0usize;
    let mut statements = Vec::with_capacity(raw.len() + 1);
    statements.push(format!("SET LOCAL search_path TO {quoted_prefix}"));
    for value in raw {
        let statement = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                CoreError::invalid("every migration statement must be a non-empty string")
            })?;
        let without_trailing = statement.trim_end_matches(';').trim_end();
        total_bytes = total_bytes.saturating_add(without_trailing.len());
        let transactional_ddl = matches!(
            leading_keyword(without_trailing).as_str(),
            "create" | "alter" | "drop" | "truncate" | "grant" | "revoke" | "comment" | "refresh"
        );
        if without_trailing.contains(';') || !transactional_ddl {
            return Err(CoreError::invalid(
                "every migration item must be one transactional DDL statement (CREATE/ALTER/DROP/TRUNCATE/GRANT/REVOKE/COMMENT/REFRESH)",
            ));
        }
        statements.push(without_trailing.replace("{{prefix}}", &quoted_prefix));
    }
    if total_bytes > 256 * 1024 {
        return Err(CoreError::invalid(
            "migration statements exceed the 256 KiB request limit",
        ));
    }
    Ok(statements)
}

async fn schema_pool(
    registry: &ExternalDbRegistry,
    args: &Json,
    ctx: &ToolContext,
    action: Action,
    schema_scope: bool,
) -> Result<(Arc<PgPool>, String), CoreError> {
    let workspace = ctx
        .workspace_id
        .ok_or_else(|| CoreError::unauthorized("no workspace in context"))?;
    let connection = required_arg(args, "connection")?;
    let (id, name) = registry.resolve_ref(workspace, connection).await?;
    let selector = if schema_scope {
        format!("{name}/schema")
    } else {
        name.clone()
    };
    enforce(ctx, &Capability::new(action, Resource::new("db", selector)))?;
    Ok((registry.pool(workspace, id).await?, name))
}

/// List the user-created PostgreSQL schema namespaces on one external database.
struct ListExternalDatabaseSchemasTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for ListExternalDatabaseSchemasTool {
    fn name(&self) -> &str {
        "list_external_database_schemas"
    }

    fn description(&self) -> &str {
        "List user-created PostgreSQL schemas on an external database. `prefix` is an optional literal starts-with filter; use a distinct prefix per App or data model so several managed schemas can coexist on one connection."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string", "description": "External Postgres connection name or id." },
                "prefix": { "type": "string", "description": "Optional literal schema-name prefix filter." }
            },
            "required": ["connection"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Read, false).await?;
        let prefix = args
            .get("prefix")
            .and_then(Json::as_str)
            .map(str::trim)
            .unwrap_or("");
        let schemas = list_schemas(&pool, prefix).await?;
        Ok(json!({ "connection": connection, "prefix": prefix, "schemas": schemas }))
    }
}

/// Describe one exact managed schema namespace, including types/defaults/index DDL.
struct GetExternalDatabaseSchemaTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for GetExternalDatabaseSchemaTool {
    fn name(&self) -> &str {
        "get_external_database_schema"
    }

    fn description(&self) -> &str {
        "Inspect one exact PostgreSQL schema namespace on an external database. `prefix` is the schema name; the result includes columns, SQL types, defaults, nullability, and index definitions."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string", "description": "External Postgres connection name or id." },
                "prefix": { "type": "string", "description": "Exact managed PostgreSQL schema name." }
            },
            "required": ["connection", "prefix"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let prefix = schema_prefix(&args)?;
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Read, false).await?;
        if !schema_exists(&pool, prefix).await? {
            return Err(CoreError::NotFound);
        }
        let mut description = describe_schema(&pool, prefix).await?;
        description["connection"] = Json::String(connection);
        Ok(description)
    }
}

/// Create an isolated schema prefix and its initial additive table definition.
struct CreateExternalDatabaseSchemaTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for CreateExternalDatabaseSchemaTool {
    fn name(&self) -> &str {
        "create_external_database_schema"
    }

    fn description(&self) -> &str {
        "Create an isolated PostgreSQL schema namespace named exactly `prefix`, optionally with initial tables and indexes. Fails if the prefix already exists; use edit_external_database_schema for an existing prefix. Requires db:write@<connection>/schema."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string", "description": "External Postgres connection name or id." },
                "prefix": { "type": "string", "description": "New schema namespace; use a stable App/data-model prefix such as crm or billing_v2." },
                "tables": desired_tables_schema()
            },
            "required": ["connection", "prefix", "tables"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let prefix = schema_prefix(&args)?;
        let desired = desired_schema(&args, prefix)?;
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Write, true).await?;
        if schema_exists(&pool, prefix).await? {
            return Err(CoreError::invalid(format!(
                "schema prefix `{prefix}` already exists; use edit_external_database_schema"
            )));
        }
        let mut plan = diff(&desired, &Default::default());
        plan.apply
            .insert(0, format!("CREATE SCHEMA {}", quote_ident(prefix)));
        catalerum_store::sql_run_ddl_batch(&pool, &plan.apply)
            .await
            .map_err(CoreError::from)?;
        Ok(json!({
            "connection": connection,
            "prefix": prefix,
            "created": true,
            "apply": plan.apply,
            "blocked": plan.blocked,
        }))
    }
}

/// Preview the exact additive DDL for an existing managed schema prefix.
struct PlanExternalDatabaseSchemaTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for PlanExternalDatabaseSchemaTool {
    fn name(&self) -> &str {
        "plan_external_database_schema"
    }

    fn description(&self) -> &str {
        "Preview the additive-safe DDL needed to edit an existing external PostgreSQL schema prefix. Nothing is changed. Existing tables/columns omitted from `tables` are retained; destructive/type changes require execute_external_database_schema_migration. Requires schema-write authority because it plans structural changes."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string" },
                "prefix": { "type": "string", "description": "Exact existing schema namespace." },
                "tables": desired_tables_schema()
            },
            "required": ["connection", "prefix", "tables"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let prefix = schema_prefix(&args)?;
        let desired = desired_schema(&args, prefix)?;
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Write, true).await?;
        if !schema_exists(&pool, prefix).await? {
            return Err(CoreError::invalid(format!(
                "schema prefix `{prefix}` does not exist; use create_external_database_schema"
            )));
        }
        let plan = diff(&desired, &introspect(&pool, prefix).await?);
        Ok(json!({
            "connection": connection,
            "prefix": prefix,
            "apply": plan.apply,
            "blocked": plan.blocked,
        }))
    }
}

/// Apply an additive-safe edit to one existing managed schema prefix.
struct EditExternalDatabaseSchemaTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for EditExternalDatabaseSchemaTool {
    fn name(&self) -> &str {
        "edit_external_database_schema"
    }

    fn description(&self) -> &str {
        "Apply additive-safe edits to an existing external PostgreSQL schema prefix: create missing tables, add safe columns, and create missing indexes, atomically. It never drops objects or changes existing column types. Preview with plan_external_database_schema first. Requires db:write@<connection>/schema."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string" },
                "prefix": { "type": "string", "description": "Exact existing schema namespace." },
                "tables": desired_tables_schema()
            },
            "required": ["connection", "prefix", "tables"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let prefix = schema_prefix(&args)?;
        let desired = desired_schema(&args, prefix)?;
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Write, true).await?;
        if !schema_exists(&pool, prefix).await? {
            return Err(CoreError::invalid(format!(
                "schema prefix `{prefix}` does not exist; use create_external_database_schema"
            )));
        }
        let plan = diff(&desired, &introspect(&pool, prefix).await?);
        if !plan.apply.is_empty() {
            catalerum_store::sql_run_ddl_batch(&pool, &plan.apply)
                .await
                .map_err(CoreError::from)?;
        }
        Ok(json!({
            "connection": connection,
            "prefix": prefix,
            "apply": plan.apply,
            "blocked": plan.blocked,
        }))
    }
}

/// Execute advanced/destructive DDL transactionally inside one schema prefix.
struct ExecuteExternalDatabaseSchemaMigrationTool {
    external_db: Arc<ExternalDbRegistry>,
}

#[async_trait]
impl Tool for ExecuteExternalDatabaseSchemaMigrationTool {
    fn name(&self) -> &str {
        "execute_external_database_schema_migration"
    }

    fn description(&self) -> &str {
        "Execute an ordered DDL migration atomically for one existing external PostgreSQL schema prefix. Use only for changes the additive editor cannot make (ALTER TYPE/nullability, rename, or drop). Unqualified names resolve inside `prefix`; `{{prefix}}` is replaced with a safely quoted schema identifier. Requires db:write@<connection>/schema."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": { "type": "string" },
                "prefix": { "type": "string", "description": "Exact existing schema namespace." },
                "statements": {
                    "type": "array",
                    "description": "Ordered, single-statement DDL strings. Do not combine statements in one item.",
                    "minItems": 1,
                    "maxItems": 100,
                    "items": { "type": "string" }
                }
            },
            "required": ["connection", "prefix", "statements"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let prefix = schema_prefix(&args)?;
        let raw = args
            .get("statements")
            .and_then(Json::as_array)
            .ok_or_else(|| CoreError::invalid("`statements` must be an array"))?;
        let statements = render_schema_migration(raw, prefix)?;
        let (pool, connection) =
            schema_pool(&self.external_db, &args, ctx, Action::Write, true).await?;
        if !schema_exists(&pool, prefix).await? {
            return Err(CoreError::invalid(format!(
                "schema prefix `{prefix}` does not exist; use create_external_database_schema"
            )));
        }
        catalerum_store::sql_run_ddl_batch(&pool, &statements)
            .await
            .map_err(CoreError::from)?;
        Ok(json!({
            "connection": connection,
            "prefix": prefix,
            "applied": statements.into_iter().skip(1).collect::<Vec<_>>(),
        }))
    }
}

#[async_trait]
impl Tool for SqlQueryTool {
    fn name(&self) -> &str {
        "sql_query"
    }

    fn description(&self) -> &str {
        "Run a parameterized SQL statement against a workspace's external Postgres \
         connection. `connection` is the connection name (or id). Use `$1`, `$2`, … \
         placeholders and pass values in `params` — never interpolate values into \
         `sql`. Plain arrays/objects bind as jsonb. To bind a native PostgreSQL text[] \
         parameter, use {\"$pg_type\":\"text[]\",\"value\":[\"a\",\"b\"]}. Reads \
         (SELECT) return rows as JSON; writes (INSERT/UPDATE/DELETE) return the \
         affected-row count. DDL (CREATE/ALTER/DROP) is not allowed here."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "connection": {
                    "type": "string",
                    "description": "The external Postgres connection name (or id)."
                },
                "sql": {
                    "type": "string",
                    "description": "A single SQL statement with $1/$2 placeholders for values."
                },
                "params": {
                    "type": "array",
                    "description": "Positional values bound to $1, $2, …. Strings, numbers, booleans, and null bind natively; plain arrays/objects bind as jsonb. For PostgreSQL text[], pass {\"$pg_type\":\"text[]\",\"value\":[\"a\",\"b\"]} (value may also be null).",
                    "items": {}
                },
                "mode": {
                    "type": "string",
                    "enum": ["read", "write"],
                    "description": "Optional: assert the statement is a read or a write; inferred when omitted."
                },
                "max_rows": {
                    "type": "integer",
                    "description": "Cap on rows returned by a read (bounded by the server config)."
                }
            },
            "required": ["connection", "sql"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> CoreResult<Json> {
        let ws = ctx
            .workspace_id
            .ok_or_else(|| CoreError::unauthorized("no workspace in context"))?;
        let connection = args
            .get("connection")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::invalid("`connection` is required"))?;
        let sql = args
            .get("sql")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CoreError::invalid("`sql` is required"))?;
        let params: Vec<Json> = match args.get("params") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(a)) => a.clone(),
            Some(_) => return Err(CoreError::invalid("`params` must be an array")),
        };

        // Decide read vs write, reconciling any explicit `mode` with the statement.
        let class = classify(sql);
        let write = match args.get("mode").and_then(Json::as_str) {
            Some("read") if class == SqlClass::Read => false,
            Some("read") => {
                return Err(CoreError::invalid(
                    "mode=read but the statement is not a read",
                ))
            }
            Some("write") if class == SqlClass::Write => true,
            Some("write") => {
                return Err(CoreError::invalid(
                    "mode=write but the statement is not INSERT/UPDATE/DELETE",
                ))
            }
            Some(other) => return Err(CoreError::invalid(format!("unknown mode `{other}`"))),
            None => match class {
                SqlClass::Read => false,
                SqlClass::Write => true,
                SqlClass::Ddl => {
                    return Err(CoreError::invalid(
                        "schema changes (CREATE/ALTER/DROP/…) are not allowed via sql_query — \
                         use create_external_database_schema, edit_external_database_schema, or \
                         execute_external_database_schema_migration (requires \
                         db:write@<conn>/schema)",
                    ))
                }
                SqlClass::Other => {
                    return Err(CoreError::invalid(
                        "unsupported statement: only SELECT/WITH/VALUES/TABLE (read) and \
                         INSERT/UPDATE/DELETE (write) are allowed",
                    ))
                }
            },
        };

        let (id, name) = self.external_db.resolve_ref(ws, connection).await?;
        let action = if write { Action::Write } else { Action::Read };
        enforce(
            ctx,
            &Capability::new(action, Resource::new("db", name.clone())),
        )?;

        let pool = self.external_db.pool(ws, id).await?;
        if write {
            let affected = catalerum_store::sql_run_write(&pool, sql, &params)
                .await
                .map_err(CoreError::from)?;
            Ok(json!({ "connection": name, "rows_affected": affected }))
        } else {
            let max = args
                .get("max_rows")
                .and_then(Json::as_u64)
                .unwrap_or(self.max_rows)
                .clamp(1, self.max_rows);
            let rows = catalerum_store::sql_run_read(&pool, sql, &params, max)
                .await
                .map_err(CoreError::from)?;
            Ok(json!({ "connection": name, "row_count": rows.len(), "rows": rows }))
        }
    }
}

/// Register `sql_query` plus the external-schema query/create/edit/migration
/// tools (SOUL §7/§11). Called before deferred tool search takes its snapshot.
pub fn register_external_database_tools(
    registry: &mut ToolRegistry,
    external_db: Arc<ExternalDbRegistry>,
    max_rows: u64,
) {
    registry.register(Arc::new(ListExternalDatabaseConnectionsTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(SqlQueryTool {
        external_db: external_db.clone(),
        max_rows,
    }));
    registry.register(Arc::new(ListExternalDatabaseSchemasTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(GetExternalDatabaseSchemaTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(CreateExternalDatabaseSchemaTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(PlanExternalDatabaseSchemaTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(EditExternalDatabaseSchemaTool {
        external_db: external_db.clone(),
    }));
    registry.register(Arc::new(ExecuteExternalDatabaseSchemaMigrationTool {
        external_db,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db_url() -> Option<String> {
        std::env::var("CATALERUM_TEST_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok()
    }

    #[tokio::test]
    async fn configured_connections_reconcile_only_into_assigned_workspaces() {
        let Some(url) = test_db_url() else {
            eprintln!(
                "skipping configured_connections_reconcile_only_into_assigned_workspaces: \
                 set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
            );
            return;
        };
        let store = Store::connect(&url).await.expect("connect+migrate");
        let slug = format!("cfg-db-a-{}", uuid::Uuid::new_v4());
        let assigned = store
            .workspaces()
            .create("Config DB A", &slug)
            .await
            .expect("assigned workspace");
        let excluded = store
            .workspaces()
            .create("Config DB B", &format!("cfg-db-b-{}", uuid::Uuid::new_v4()))
            .await
            .expect("excluded workspace");
        let configured = BTreeMap::from([(
            "reporting".to_string(),
            ExternalDbConnectionConfig {
                host: "db.internal".to_string(),
                database: "reports".to_string(),
                username: "reader".to_string(),
                workspaces: vec![slug.to_uppercase()],
                ..ExternalDbConnectionConfig::default()
            },
        )]);
        let registry =
            ExternalDbRegistry::new(store.clone(), None, 2, 1_000).with_configured(configured);

        let assigned_connections = registry.list(assigned.id).await.expect("assigned list");
        let excluded_connections = registry.list(excluded.id).await.expect("excluded list");
        assert_eq!(assigned_connections.len(), 1);
        assert_eq!(assigned_connections[0].name, "reporting");
        assert!(excluded_connections.is_empty());
        let row = store
            .connections()
            .get_row(assigned.id, assigned_connections[0].id)
            .await
            .expect("configured row");
        let config: PostgresConnectionConfig =
            serde_json::from_value(row.config.0).expect("postgres config");
        assert_eq!(config.database, "reports");
    }

    #[tokio::test]
    async fn connection_discovery_tool_is_explicit_and_secret_free() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://localhost/catalerum_test")
            .expect("lazy pool");
        let tool = ListExternalDatabaseConnectionsTool {
            external_db: Arc::new(ExternalDbRegistry::new(Store::new(pool), None, 1, 1_000)),
        };

        assert_eq!(tool.name(), "list_external_database_connections");
        assert!(tool.description().contains("never guess"));
        assert_eq!(
            tool.parameters_schema(),
            json!({ "type": "object", "properties": {} })
        );
        assert!(!tool.description().contains("password"));
    }

    #[test]
    fn classifies_statements_by_leading_keyword() {
        assert_eq!(classify("SELECT * FROM t"), SqlClass::Read);
        assert_eq!(
            classify("  with x as (select 1) select * from x"),
            SqlClass::Read
        );
        assert_eq!(classify("INSERT INTO t VALUES (1)"), SqlClass::Write);
        assert_eq!(classify("update t set a=1"), SqlClass::Write);
        assert_eq!(classify("DELETE FROM t"), SqlClass::Write);
        assert_eq!(classify("CREATE TABLE t (id int)"), SqlClass::Ddl);
        assert_eq!(classify("drop table t"), SqlClass::Ddl);
        assert_eq!(classify("SET search_path = x"), SqlClass::Other);
        assert_eq!(classify("COPY t FROM stdin"), SqlClass::Other);
    }

    #[test]
    fn leading_keyword_skips_line_comments() {
        assert_eq!(leading_keyword("-- a comment\nSELECT 1"), "select");
        assert_eq!(leading_keyword("   \n INSERT INTO t"), "insert");
    }

    #[test]
    fn enforce_denies_without_capability_and_allows_with() {
        let read_db = Capability::new(Action::Read, Resource::new("db", "news"));
        // No capabilities in context → trusted, allowed.
        let trusted = ToolContext::default();
        assert!(enforce(&trusted, &read_db).is_ok());
        // Holding db:read@news covers the request; holding only db:read@other does not.
        let mut ctx = ToolContext {
            capabilities: Some(vec![Capability::new(
                Action::Read,
                Resource::new("db", "news"),
            )]),
            ..ToolContext::default()
        };
        assert!(enforce(&ctx, &read_db).is_ok());
        ctx.capabilities = Some(vec![Capability::new(
            Action::Read,
            Resource::new("db", "other"),
        )]);
        assert!(enforce(&ctx, &read_db).is_err());
        // A read grant never authorizes a write.
        ctx.capabilities = Some(vec![Capability::new(
            Action::Read,
            Resource::new("db", "news"),
        )]);
        let write_db = Capability::new(Action::Write, Resource::new("db", "news"));
        assert!(enforce(&ctx, &write_db).is_err());
    }

    #[test]
    fn schema_write_scope_is_narrower_than_data_write_scope() {
        let schema_write = Capability::new(Action::Write, Resource::new("db", "news/schema"));
        let data_only = ToolContext {
            capabilities: Some(vec![Capability::new(
                Action::Write,
                Resource::new("db", "news"),
            )]),
            ..ToolContext::default()
        };
        assert!(enforce(&data_only, &schema_write).is_err());

        let schema_scoped = ToolContext {
            capabilities: Some(vec![schema_write.clone()]),
            ..ToolContext::default()
        };
        assert!(enforce(&schema_scoped, &schema_write).is_ok());

        let domain_wide = ToolContext {
            capabilities: Some(vec![Capability::new(Action::Write, Resource::domain("db"))]),
            ..ToolContext::default()
        };
        assert!(enforce(&domain_wide, &schema_write).is_ok());
    }

    #[test]
    fn schema_migrations_are_single_transactional_ddl_and_quote_prefix() {
        let rendered = render_schema_migration(
            &[
                json!("ALTER TABLE {{prefix}}.orders ADD COLUMN note text;"),
                json!("DROP INDEX old_orders_idx"),
            ],
            "billing_v2",
        )
        .unwrap();
        assert_eq!(rendered[0], "SET LOCAL search_path TO \"billing_v2\"");
        assert_eq!(
            rendered[1],
            "ALTER TABLE \"billing_v2\".orders ADD COLUMN note text"
        );
        assert_eq!(rendered[2], "DROP INDEX old_orders_idx");

        assert!(render_schema_migration(&[json!("DELETE FROM orders")], "billing").is_err());
        assert!(render_schema_migration(
            &[json!("ALTER TABLE orders ADD x int; DROP TABLE orders")],
            "billing"
        )
        .is_err());
        assert!(render_schema_migration(&[json!("VACUUM orders")], "billing").is_err());
    }
}
