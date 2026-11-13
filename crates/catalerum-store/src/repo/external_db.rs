use catalerum_core::id::{ConnectionId, WorkspaceId};
use uuid::Uuid;
use crate::error::Result;
use super::{map, PgPool};

// ===========================================================================
// External-DB manual migrations (SOUL §11) — ordered, hand-written SQL migrations
// authored for a specific external Postgres connection, plus the applied ledger
// (tracked in catalerum's own DB, keyed by connection).
// ===========================================================================

/// A registered manual migration for an external connection, with whether it has
/// been applied (from the ledger). Returned by [`ExternalDbMigrationRepo::list`].
#[derive(Clone, Debug, serde::Serialize)]
pub struct ExternalDbMigration {
    pub version: i64,
    pub name: String,
    pub up_sql: String,
    pub checksum: String,
    pub applied: bool,
}

#[derive(sqlx::FromRow)]
struct ExternalDbMigrationRow {
    version: i64,
    name: String,
    up_sql: String,
    checksum: String,
    applied: bool,
}

/// CRUD over the external-DB migration scripts + applied ledger (SOUL §11).
#[derive(Clone, Debug)]
pub struct ExternalDbMigrationRepo {
    pool: PgPool,
}

impl ExternalDbMigrationRepo {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Register a manual migration script for `connection_id`. `version` is unique
    /// per connection ([`StoreError::Conflict`](crate::error::StoreError::Conflict) on a duplicate).
    pub async fn add_script(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        version: i64,
        name: &str,
        up_sql: &str,
        checksum: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO external_db_migration_scripts
                 (id, workspace_id, connection_id, version, name, up_sql, checksum)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(version)
        .bind(name)
        .bind(up_sql)
        .bind(checksum)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }

    /// List a connection's migration scripts in ascending version order, each with
    /// an `applied` flag from the ledger.
    pub async fn list(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
    ) -> Result<Vec<ExternalDbMigration>> {
        let rows: Vec<ExternalDbMigrationRow> = sqlx::query_as(
            "SELECT s.version, s.name, s.up_sql, s.checksum,
                    (m.version IS NOT NULL) AS applied
             FROM external_db_migration_scripts s
             LEFT JOIN external_db_migrations m
                 ON m.connection_id = s.connection_id AND m.version = s.version
             WHERE s.workspace_id = $1 AND s.connection_id = $2
             ORDER BY s.version ASC",
        )
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(map)?;
        Ok(rows
            .into_iter()
            .map(|r| ExternalDbMigration {
                version: r.version,
                name: r.name,
                up_sql: r.up_sql,
                checksum: r.checksum,
                applied: r.applied,
            })
            .collect())
    }

    /// Record a migration as applied in the ledger (idempotent via the
    /// `(connection_id, version)` unique key — a re-apply is a [`StoreError::Conflict`](crate::error::StoreError::Conflict)).
    pub async fn record_applied(
        &self,
        workspace_id: WorkspaceId,
        connection_id: ConnectionId,
        version: i64,
        name: &str,
        checksum: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO external_db_migrations
                 (id, workspace_id, connection_id, version, name, checksum)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::new_v4())
        .bind(workspace_id.into_uuid())
        .bind(connection_id.into_uuid())
        .bind(version)
        .bind(name)
        .bind(checksum)
        .execute(&self.pool)
        .await
        .map_err(map)?;
        Ok(())
    }
}
