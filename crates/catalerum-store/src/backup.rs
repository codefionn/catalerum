//! Postgres access for the logical backup + restore in `catalerum-backup`
//! (SOUL §30). The backup crate owns the artifact format, compression, checksums
//! and load-order logic; this module owns every SQL statement it needs, so raw
//! `sqlx` stays inside the store crate (SOUL §4/§6.1).
//!
//! - Dump side: [`list_tables`], [`schema_version`], [`copy_out`].
//! - Restore side: a [`RestoreSession`] pinned to one pooled connection, which
//!   reports the role's privilege and FK graph, opens one transaction (optionally
//!   with FK/user triggers disabled), truncates, `COPY … FROM STDIN`s each table
//!   and commits.
//!
//! These always run against a [`PgPool`], also in the all-in-one SQLite build:
//! backup is a Postgres feature.

use futures::StreamExt;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPoolCopyExt;
use sqlx::{AssertSqlSafe, PgPool, Postgres};

use crate::error::Result;

/// A `public` table and its columns in ordinal order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSpec {
    pub name: String,
    pub columns: Vec<String>,
}

/// Quote a SQL identifier for safe interpolation (double-quote, escaping any
/// embedded `"`). Table/column names come from the catalog, but quoting is still
/// correct hygiene and survives reserved words / mixed case.
fn quote_ident(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

/// A comma-separated, quoted column list for a `COPY` statement.
fn column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Enumerate the user tables to dump: every base table in the `public` schema
/// except sqlx's `_sqlx_migrations` (the schema, and that table, are recreated
/// by migrating on boot; the version is captured separately by
/// [`schema_version`]). Columns are read in `ordinal_position` order.
/// Identifiers are cast to `text` so sqlx decodes them without a `name`/domain
/// codec.
pub async fn list_tables(pool: &PgPool) -> Result<Vec<TableSpec>> {
    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT table_name::text \
           FROM information_schema.tables \
          WHERE table_schema = 'public' \
            AND table_type = 'BASE TABLE' \
            AND table_name <> '_sqlx_migrations' \
          ORDER BY table_name",
    )
    .fetch_all(pool)
    .await?;

    let mut specs = Vec::with_capacity(names.len());
    for (name,) in names {
        let columns: Vec<(String,)> = sqlx::query_as(
            "SELECT column_name::text \
               FROM information_schema.columns \
              WHERE table_schema = 'public' AND table_name = $1 \
              ORDER BY ordinal_position",
        )
        .bind(&name)
        .fetch_all(pool)
        .await?;
        specs.push(TableSpec {
            name,
            columns: columns.into_iter().map(|(c,)| c).collect(),
        });
    }
    Ok(specs)
}

/// The Postgres schema version: the max applied `_sqlx_migrations.version`
/// (`None` if no migrations are recorded).
pub async fn schema_version(pool: &PgPool) -> Result<Option<i64>> {
    // `MAX(version)` is NULL on an empty table → the outer Option; a missing
    // `_sqlx_migrations` would error, but the binary always migrates first.
    let version: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

/// `COPY public.<table> (<cols>) TO STDOUT WITH (FORMAT text)` and return the
/// raw text-format bytes. Every row is exactly one `\n`-terminated line (data
/// newlines are escaped).
pub async fn copy_out(pool: &PgPool, table: &TableSpec) -> Result<Vec<u8>> {
    let stmt = format!(
        "COPY public.{} ({}) TO STDOUT WITH (FORMAT text)",
        quote_ident(&table.name),
        column_list(&table.columns),
    );
    let mut stream = pool.copy_out_raw(&stmt).await?;

    let mut plain = Vec::new();
    while let Some(chunk) = stream.next().await {
        plain.extend_from_slice(&chunk?);
    }
    Ok(plain)
}

/// One pooled connection dedicated to a destructive restore.
///
/// Call order: [`open`](Self::open) → optional [`is_superuser`](Self::is_superuser)
/// / [`fk_edges`](Self::fk_edges) → [`begin`](Self::begin) →
/// [`truncate`](Self::truncate) → [`copy_in`](Self::copy_in) per table →
/// [`commit`](Self::commit). Dropping the session before `commit` drops the
/// connection with the transaction still open, so the load never commits half
/// way.
pub struct RestoreSession {
    conn: PoolConnection<Postgres>,
    /// Whether [`begin`](Self::begin) set `session_replication_role = replica`,
    /// which [`commit`](Self::commit) must reset (it is connection-scoped, not
    /// transactional).
    replica: bool,
}

impl RestoreSession {
    /// Acquire a connection from `pool`.
    pub async fn open(pool: &PgPool) -> Result<Self> {
        Ok(Self {
            conn: pool.acquire().await?,
            replica: false,
        })
    }

    /// Whether the session role is a Postgres superuser. `is_superuser` is a
    /// preset (read-only) parameter, so this is one cheap query with no side
    /// effects.
    pub async fn is_superuser(&mut self) -> Result<bool> {
        let flag: String = sqlx::query_scalar("SELECT current_setting('is_superuser')")
            .fetch_one(&mut *self.conn)
            .await?;
        Ok(flag.eq_ignore_ascii_case("on"))
    }

    /// The foreign-key edges among `public`-schema tables as `(child, parent)` =
    /// (referencing table, referenced table). One row per FK constraint, so a
    /// composite FK is a single edge; a self-referential FK shows up as
    /// `child == parent`. Read from `pg_constraint` (contype `'f'`), which needs
    /// no special privilege.
    pub async fn fk_edges(&mut self) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT c.relname::text AS child, p.relname::text AS parent \
               FROM pg_constraint con \
               JOIN pg_class c ON c.oid = con.conrelid \
               JOIN pg_class p ON p.oid = con.confrelid \
               JOIN pg_namespace nc ON nc.oid = c.relnamespace \
               JOIN pg_namespace np ON np.oid = p.relnamespace \
              WHERE con.contype = 'f' \
                AND nc.nspname = 'public' \
                AND np.nspname = 'public'",
        )
        .fetch_all(&mut *self.conn)
        .await?;
        Ok(rows)
    }

    /// Open the restore transaction. With `disable_triggers`, first
    /// `SET session_replication_role = replica` so FK/user triggers are off and
    /// load order is irrelevant. That GUC is superuser-only; a hardened role
    /// errors on it with SQLSTATE `42501`.
    pub async fn begin(&mut self, disable_triggers: bool) -> Result<()> {
        if disable_triggers {
            sqlx::query("SET session_replication_role = replica")
                .execute(&mut *self.conn)
                .await?;
            self.replica = true;
        }
        sqlx::query("BEGIN").execute(&mut *self.conn).await?;
        Ok(())
    }

    /// `TRUNCATE` every listed `public` table in one statement with
    /// `RESTART IDENTITY CASCADE`. Postgres refuses to truncate a FK-referenced
    /// table on its own even when the referencing table is empty, so the combined
    /// form is what lets mutually-referencing tables truncate together. Needs
    /// `TRUNCATE` privilege (table ownership), not superuser.
    pub async fn truncate(&mut self, tables: &[&str]) -> Result<()> {
        let list = tables
            .iter()
            .map(|t| format!("public.{}", quote_ident(t)))
            .collect::<Vec<_>>()
            .join(", ");
        sqlx::query(AssertSqlSafe(format!(
            "TRUNCATE {list} RESTART IDENTITY CASCADE"
        )))
        .execute(&mut *self.conn)
        .await?;
        Ok(())
    }

    /// `COPY public.<table> (<cols>) FROM STDIN WITH (FORMAT text)` with `data`
    /// (text-format bytes as produced by [`copy_out`]).
    pub async fn copy_in(&mut self, table: &TableSpec, data: &[u8]) -> Result<()> {
        let stmt = format!(
            "COPY public.{} ({}) FROM STDIN WITH (FORMAT text)",
            quote_ident(&table.name),
            column_list(&table.columns),
        );
        let mut sink = self.conn.copy_in_raw(&stmt).await?;
        if !data.is_empty() {
            sink.send(data).await?;
        }
        sink.finish().await?;
        Ok(())
    }

    /// `COMMIT`, then reset `session_replication_role` if [`begin`](Self::begin)
    /// set it, before the connection returns to the pool.
    pub async fn commit(mut self) -> Result<()> {
        sqlx::query("COMMIT").execute(&mut *self.conn).await?;
        if self.replica {
            sqlx::query("SET session_replication_role = DEFAULT")
                .execute(&mut *self.conn)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers_and_escapes_quotes() {
        assert_eq!(quote_ident("events"), "\"events\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(
            column_list(&["id".into(), "workspace_id".into()]),
            "\"id\", \"workspace_id\""
        );
    }
}
