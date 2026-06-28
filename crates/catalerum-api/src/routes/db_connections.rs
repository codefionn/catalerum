//! External PostgreSQL connection management (SOUL §11/§19).
//!
//! A workspace registers external Postgres databases it owns here; the password
//! is encrypted into the secret store (`[secrets].master_key`) and the row keeps
//! only an opaque `credential_ref`. Registering a connection provisions nothing
//! on its own — the `sql_query` tool, the `SqlQuery` automation action, and the
//! schema-migration routes are what actually reach the database, each gated on a
//! `db:*` capability (`db:read@conn`, `db:write@conn`, `db:write@conn/schema`).
//!
//! **Reads** (list / get / test / introspect / list migrations) gate on
//! `db:read` — every role, since a Member's tools and Apps legitimately use a
//! connection. **Connection lifecycle** (register / remove) is a
//! workspace-operational config write — it stores workspace-shared credentials
//! and provisions infrastructure every member's `sql_query` then reaches — so it
//! additionally requires a workspace **administrator** (Owner/Admin) via
//! [`Auth::require_workspace_admin`], deny-by-default and independent of the
//! deployment `mode` (SOUL §18/§29). The data-plane schema/migration writes stay
//! on `db:write` (a Member manages their App's schema on a provisioned
//! connection, SOUL §11/§19).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use catalerum_store::PgPool;

use catalerum_core::capability::Action;
use catalerum_core::model::{Connection, ConnectionKind};
use catalerum_core::ConnectionId;

use sha2::{Digest, Sha256};

use crate::auth::Auth;
use crate::db_migrate::{diff, introspect, ActualSchema, DesiredSchema, MigrationPlan};
use crate::error::{ApiError, ApiResult};
use crate::external_db::PostgresConnectionConfig;
use crate::state::AppState;

/// Routes for managing external Postgres connections (SOUL §11).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/db/connections", post(create).get(list))
        .route("/db/connections/{id}", get(get_one).delete(delete_one))
        .route("/db/connections/{id}/test", post(test))
        .route("/db/connections/{id}/schema", get(get_schema))
        .route("/db/connections/{id}/schema/plan", post(plan_schema))
        .route("/db/connections/{id}/schema/apply", post(apply_schema))
        .route(
            "/db/connections/{id}/migrations",
            get(list_migrations).post(register_migration),
        )
        .route("/db/connections/{id}/migrate", post(migrate))
}

fn default_port() -> u16 {
    5432
}

/// Body for `POST /db/connections`. The password is stored **encrypted** (never
/// in the connection `config`); everything else lands in the `config` blob.
#[derive(Debug, Deserialize)]
pub struct CreatePostgresConnection {
    /// Human-readable, workspace-unique name (used to reference the connection
    /// from `sql_query` / the `SqlQuery` action).
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub username: String,
    /// The password; encrypted at rest. Empty means "no password" (e.g. trust auth).
    #[serde(default)]
    pub password: String,
    /// libpq sslmode (`disable`/`require`/`verify-full`/…).
    #[serde(default)]
    pub sslmode: Option<String>,
    /// Default schema (`search_path`) pinned for every session.
    #[serde(default)]
    pub schema: Option<String>,
    /// Per-connection pool-size override.
    #[serde(default)]
    pub pool_max: Option<u32>,
}

/// A connection's non-secret view: identity plus the `config` blob (never the
/// password), with a flag for whether an encrypted credential is stored.
#[derive(Debug, Serialize)]
pub struct PostgresConnectionView {
    pub id: ConnectionId,
    pub name: String,
    #[serde(flatten)]
    pub config: PostgresConnectionConfig,
    /// Whether an encrypted credential is stored for this connection.
    pub has_credential: bool,
}

/// `POST /db/connections` — register an external Postgres database. `db:write`
/// **and** a workspace administrator (Owner/Admin): registering a connection is a
/// workspace-operational config write (stores shared credentials, SOUL §18/§29).
/// Encrypts the password into the secret store and stores host/port/database/
/// username/options in the connection `config`.
async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreatePostgresConnection>,
) -> ApiResult<(StatusCode, Json<Connection>)> {
    auth.require(Action::Write, "db")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    state.external_db().ensure_configured(ws).await?;

    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("connection name must not be empty"));
    }
    if body.host.trim().is_empty() {
        return Err(ApiError::bad_request("`host` must not be empty"));
    }
    if body.database.trim().is_empty() {
        return Err(ApiError::bad_request("`database` must not be empty"));
    }
    if body.username.trim().is_empty() {
        return Err(ApiError::bad_request("`username` must not be empty"));
    }

    // Encrypt the password first (needs the secret store), so a failure here
    // never persists a connection referencing a credential that doesn't exist.
    let credential_ref = if body.password.is_empty() {
        None
    } else {
        let secrets = state.external_db().secret_store()?;
        Some(secrets.put(ws, body.password.as_bytes()).await?)
    };

    let cfg = PostgresConnectionConfig {
        host: body.host.trim().to_string(),
        port: body.port,
        database: body.database.trim().to_string(),
        username: body.username.trim().to_string(),
        sslmode: body
            .sslmode
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        schema: body
            .schema
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        pool_max: body.pool_max,
    };
    let config = serde_json::to_value(&cfg)
        .map_err(|e| ApiError::internal(format!("serializing connection config: {e}")))?;

    match state
        .store()
        .connections()
        .create(
            ws,
            ConnectionKind::Postgres,
            name,
            credential_ref.as_deref(),
            Some(config),
        )
        .await
    {
        Ok(connection) => Ok((StatusCode::CREATED, Json(connection))),
        Err(e) => {
            // Roll back the orphaned secret so a name-conflict retry stays clean.
            if let Some(reference) = &credential_ref {
                if let Ok(secrets) = state.external_db().secret_store() {
                    let _ = secrets.delete(ws, reference).await;
                }
            }
            Err(ApiError::from(e))
        }
    }
}

/// `GET /db/connections` — this workspace's Postgres connections, newest first.
/// `db:read`.
async fn list(State(state): State<AppState>, auth: Auth) -> ApiResult<Json<Vec<Connection>>> {
    auth.require(Action::Read, "db")?;
    let ws = auth.principal().workspace_id;
    let connections = state.external_db().list(ws).await?;
    Ok(Json(connections))
}

/// `GET /db/connections/{id}` — one connection with its non-secret config.
/// `db:read`. `404` for a foreign/unknown id; `400` if not a Postgres connection.
async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<PostgresConnectionView>> {
    auth.require(Action::Read, "db")?;
    let ws = auth.principal().workspace_id;
    state.external_db().ensure_configured(ws).await?;
    let row = state
        .store()
        .connections()
        .get_row(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if row.kind != "postgres" {
        return Err(ApiError::bad_request("not a postgres connection"));
    }
    let config: PostgresConnectionConfig = serde_json::from_value(row.config.0.clone())
        .map_err(|e| ApiError::internal(format!("decoding connection config: {e}")))?;
    Ok(Json(PostgresConnectionView {
        id,
        name: row.name,
        config,
        has_credential: row.credential_ref.is_some(),
    }))
}

/// `DELETE /db/connections/{id}` — remove a connection, its stored credential,
/// and its cached pool. `db:write` **and** a workspace administrator (removing a
/// shared connection is workspace-operational config, SOUL §18/§29). The external
/// database is untouched.
async fn delete_one(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "db")?;
    auth.require_workspace_admin()?;
    let ws = auth.principal().workspace_id;
    state.external_db().ensure_configured(ws).await?;
    let connection = state
        .store()
        .connections()
        .get(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if connection.kind != ConnectionKind::Postgres {
        return Err(ApiError::bad_request("not a postgres connection"));
    }
    if state
        .external_db()
        .is_configured(ws, &connection.name)
        .await?
    {
        return Err(ApiError::bad_request(format!(
            "connection `{}` is defined in [external_db.connections]; remove it from config instead",
            connection.name
        )));
    }
    // Drop the cached pool + the encrypted credential before the row.
    state.external_db().evict(id).await;
    if let Some(reference) = &connection.credential_ref {
        if let Ok(secrets) = state.external_db().secret_store() {
            let _ = secrets.delete(ws, reference).await;
        }
    }
    state.store().connections().delete(ws, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /db/connections/{id}/test` — build (or reuse) the pool and run a
/// `SELECT 1` liveness probe. `db:read`. `200 {ok:true}` on success.
async fn test(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<serde_json::Value>> {
    auth.require(Action::Read, "db")?;
    let ws = auth.principal().workspace_id;
    state.external_db().test(ws, id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Managed schema: declarative auto-migration + manual migrations (SOUL §11)
// ---------------------------------------------------------------------------

/// Resolve a Postgres connection to its live pool + its default schema name
/// (from the connection config, defaulting to `public`).
async fn pool_and_schema(
    state: &AppState,
    ws: catalerum_core::WorkspaceId,
    id: ConnectionId,
) -> ApiResult<(Arc<PgPool>, String)> {
    state.external_db().ensure_configured(ws).await?;
    let row = state
        .store()
        .connections()
        .get_row(ws, id)
        .await
        .map_err(|_| ApiError::NotFound)?;
    if row.kind != "postgres" {
        return Err(ApiError::bad_request("not a postgres connection"));
    }
    let cfg: PostgresConnectionConfig = serde_json::from_value(row.config.0.clone())
        .map_err(|e| ApiError::internal(format!("decoding connection config: {e}")))?;
    let schema = cfg.schema.unwrap_or_else(|| "public".to_string());
    let pool = state.external_db().pool(ws, id).await?;
    Ok((pool, schema))
}

/// `GET /db/connections/{id}/schema` — the introspected live schema. `db:read`.
async fn get_schema(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<ActualSchema>> {
    auth.require(Action::Read, "db")?;
    let ws = auth.principal().workspace_id;
    let (pool, schema) = pool_and_schema(&state, ws, id).await?;
    let actual = introspect(&pool, &schema).await?;
    Ok(Json(actual))
}

/// `POST /db/connections/{id}/schema/plan` — dry-run the additive-safe diff for a
/// desired schema; returns the DDL that *would* apply plus any blocked changes.
/// `db:write` (schema changes; also `db:write@<conn>/schema` for agent grants).
async fn plan_schema(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
    Json(mut desired): Json<DesiredSchema>,
) -> ApiResult<Json<MigrationPlan>> {
    auth.require(Action::Write, "db")?;
    let ws = auth.principal().workspace_id;
    let (pool, schema) = pool_and_schema(&state, ws, id).await?;
    if desired.schema.is_none() {
        desired.schema = Some(schema);
    }
    let actual = introspect(&pool, desired.schema.as_deref().unwrap_or("public")).await?;
    Ok(Json(diff(&desired, &actual)))
}

/// `POST /db/connections/{id}/schema/apply` — compute the additive-safe plan and
/// apply it in one transaction. Returns the applied DDL + blocked changes.
/// `db:write`.
async fn apply_schema(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
    Json(mut desired): Json<DesiredSchema>,
) -> ApiResult<Json<MigrationPlan>> {
    auth.require(Action::Write, "db")?;
    let ws = auth.principal().workspace_id;
    let (pool, schema) = pool_and_schema(&state, ws, id).await?;
    if desired.schema.is_none() {
        desired.schema = Some(schema);
    }
    let actual = introspect(&pool, desired.schema.as_deref().unwrap_or("public")).await?;
    let plan = diff(&desired, &actual);
    if !plan.apply.is_empty() {
        catalerum_store::sql_run_ddl_batch(&pool, &plan.apply)
            .await
            .map_err(catalerum_core::Error::from)?;
    }
    Ok(Json(plan))
}

/// SHA-256 hex checksum of a migration script's SQL (drift guard).
fn checksum(sql: &str) -> String {
    let mut h = Sha256::new();
    h.update(sql.as_bytes());
    format!("{:x}", h.finalize())
}

/// Body for `POST /db/connections/{id}/migrations` — register a manual migration.
#[derive(Debug, Deserialize)]
pub struct RegisterMigration {
    /// Unique, ascending version for this connection.
    pub version: i64,
    pub name: String,
    /// The `up` SQL (may contain multiple statements).
    pub up_sql: String,
}

/// `POST /db/connections/{id}/migrations` — register a manual migration script.
/// `db:write`.
async fn register_migration(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
    Json(body): Json<RegisterMigration>,
) -> ApiResult<StatusCode> {
    auth.require(Action::Write, "db")?;
    let ws = auth.principal().workspace_id;
    // Ensure the connection exists + is Postgres before recording a script for it.
    let _ = pool_and_schema(&state, ws, id).await?;
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("migration `name` must not be empty"));
    }
    if body.up_sql.trim().is_empty() {
        return Err(ApiError::bad_request(
            "migration `up_sql` must not be empty",
        ));
    }
    state
        .store()
        .external_db_migrations()
        .add_script(
            ws,
            id,
            body.version,
            body.name.trim(),
            &body.up_sql,
            &checksum(&body.up_sql),
        )
        .await?;
    Ok(StatusCode::CREATED)
}

/// `GET /db/connections/{id}/migrations` — the connection's migration scripts with
/// applied status, ascending by version. `db:read`.
async fn list_migrations(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<Vec<catalerum_store::ExternalDbMigration>>> {
    auth.require(Action::Read, "db")?;
    let ws = auth.principal().workspace_id;
    state.external_db().ensure_configured(ws).await?;
    let list = state.store().external_db_migrations().list(ws, id).await?;
    Ok(Json(list))
}

/// `POST /db/connections/{id}/migrate` — apply pending manual migrations in version
/// order (each in its own transaction), recording each in the ledger. Returns the
/// versions applied this run. `db:write`.
async fn migrate(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<ConnectionId>,
) -> ApiResult<Json<serde_json::Value>> {
    auth.require(Action::Write, "db")?;
    let ws = auth.principal().workspace_id;
    let (pool, _schema) = pool_and_schema(&state, ws, id).await?;
    let pending: Vec<catalerum_store::ExternalDbMigration> = state
        .store()
        .external_db_migrations()
        .list(ws, id)
        .await?
        .into_iter()
        .filter(|m| !m.applied)
        .collect();
    let mut applied: Vec<i64> = Vec::new();
    for m in &pending {
        catalerum_store::sql_run_script(&pool, &m.up_sql)
            .await
            .map_err(catalerum_core::Error::from)?;
        state
            .store()
            .external_db_migrations()
            .record_applied(ws, id, m.version, &m.name, &m.checksum)
            .await?;
        applied.push(m.version);
    }
    Ok(Json(serde_json::json!({ "applied": applied })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Auth;
    use catalerum_core::model::Role;
    use catalerum_core::{UserId, WorkspaceId};

    fn auth(role: Role) -> Auth {
        Auth::from_principal(catalerum_iam::Principal::new(
            UserId::new(),
            WorkspaceId::new(),
            role,
        ))
    }

    /// Connection lifecycle (`POST`/`DELETE /db/connections`) is gated on a
    /// workspace administrator: both handlers call `auth.require_workspace_admin()`
    /// first, so a plain Member (who still holds `db:write` for its tools/Apps) and
    /// a Viewer are `403`, while an Owner/Admin passes — independent of the
    /// deployment mode (SOUL §18/§29). The data-plane schema/migration writes are
    /// intentionally *not* admin-gated (a Member manages their App's schema).
    #[test]
    fn connection_lifecycle_requires_workspace_admin() {
        assert!(auth(Role::Owner).require_workspace_admin().is_ok());
        assert!(auth(Role::Admin).require_workspace_admin().is_ok());
        assert!(matches!(
            auth(Role::Member).require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
        assert!(matches!(
            auth(Role::Viewer).require_workspace_admin(),
            Err(ApiError::Forbidden(_))
        ));
        // The write capability the routes also require is held by Member+ but not a
        // Viewer — the two gates are complementary (deny-by-default, SOUL §19).
        assert!(auth(Role::Member).require(Action::Write, "db").is_ok());
        assert!(auth(Role::Viewer).require(Action::Write, "db").is_err());
    }
}
