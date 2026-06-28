//! Postgres connection pool and embedded migrations.

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};

#[cfg(feature = "sqlite")]
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// Repository backend template. Repository code is compiled against
/// [`ActiveBackend`], so Rust monomorphises one database implementation into a
/// given binary; there is no enum match, trait object, or per-query dispatch.
pub trait RepositoryBackend: Send + Sync + 'static {
    type Database: sqlx::Database;
    const NAME: &'static str;
}

#[derive(Clone, Copy, Debug)]
pub struct PostgresBackend;

impl RepositoryBackend for PostgresBackend {
    type Database = sqlx::Postgres;
    const NAME: &'static str = "postgres";
}

#[derive(Clone, Copy, Debug)]
pub struct SqliteBackend;

impl RepositoryBackend for SqliteBackend {
    type Database = sqlx::Sqlite;
    const NAME: &'static str = "sqlite";
}

pub type BackendPool<B> = sqlx::Pool<<B as RepositoryBackend>::Database>;

/// Native backend selected at compile time. The normal/distributed build uses
/// PostgreSQL; the all-in-one build enables `sqlite` and is monomorphised over
/// SQLite. External user-configured databases remain PostgreSQL regardless.
#[cfg(not(feature = "sqlite"))]
pub type ActiveBackend = PostgresBackend;
#[cfg(feature = "sqlite")]
pub type ActiveBackend = SqliteBackend;
pub type DbPool = BackendPool<ActiveBackend>;

use crate::error::{Result, StoreError};

/// The embedded migration set (compile-time-bundled from `./migrations`).
///
/// `sqlx::migrate!` only reads the migrations directory at compile time — it
/// does **not** require a live database — so it is safe to use even though the
/// query macros are not.
#[cfg(not(feature = "sqlite"))]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
#[cfg(feature = "sqlite")]
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations-sqlite");

/// Options for constructing a [`PgPool`].
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Maximum number of pooled connections.
    pub max_connections: u32,
    /// Minimum number of idle connections kept warm.
    pub min_connections: u32,
    /// Timeout for acquiring a connection from the pool.
    pub acquire_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 10,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(30),
        }
    }
}

/// Build a Postgres connection pool from a `database_url` with default options.
///
/// ```no_run
/// # async fn f() -> catalerum_store::Result<()> {
/// let pool = catalerum_store::connect("postgres://localhost/catalerum").await?;
/// # let _ = pool; Ok(())
/// # }
/// ```
pub async fn connect(database_url: &str) -> Result<DbPool> {
    connect_with(database_url, &PoolConfig::default()).await
}

/// Build a Postgres connection pool from a `database_url` with explicit
/// [`PoolConfig`].
#[cfg(not(feature = "sqlite"))]
pub async fn connect_with(database_url: &str, config: &PoolConfig) -> Result<DbPool> {
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout)
        .connect(database_url)
        .await?;
    Ok(pool)
}

/// SQLite pool construction for the single-node image. WAL keeps readers from
/// blocking the one writer, foreign keys are enforced, and NORMAL synchronous
/// mode is the standard durable/WAL trade-off. A missing file is created.
#[cfg(feature = "sqlite")]
pub async fn connect_with(database_url: &str, config: &PoolConfig) -> Result<DbPool> {
    use std::str::FromStr as _;

    let options = SqliteConnectOptions::from_str(database_url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(config.acquire_timeout);
    let pool = SqlitePoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout)
        .connect_with(options)
        .await?;
    Ok(pool)
}

/// Liveness probe against an arbitrary pool (`SELECT 1`). Used to test an
/// external connection's reachability without going through a [`Store`].
pub async fn ping_pool(pool: &DbPool) -> Result<()> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Liveness probe for a workspace's external PostgreSQL connection. Kept
/// separate from [`ping_pool`] because the native store may be SQLite.
pub async fn ping_external_pool(pool: &PgPool) -> Result<()> {
    sqlx::query("SELECT 1").execute(pool).await?;
    Ok(())
}

/// Apply all embedded migrations against `pool`. Idempotent: already-applied
/// migrations are skipped.
pub async fn migrate(pool: &DbPool) -> Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

/// Connect and migrate in one step — the common bootstrap path.
pub async fn connect_and_migrate(database_url: &str) -> Result<DbPool> {
    let pool = connect(database_url).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// A typed connection spec for an **external** PostgreSQL database (SOUL §11) —
/// a workspace-owned server catalerum connects to, distinct from its own store.
/// Built field-by-field (never a URL) so a password with `@`/`:`/`/` never needs
/// escaping and can't corrupt the DSN. `search_path` and `statement_timeout_ms`
/// are applied as libpq session options, so every statement on the pool inherits
/// the schema and the hard timeout.
#[derive(Clone, Debug)]
pub struct PgConnectSpec {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    /// libpq sslmode: `disable` | `allow` | `prefer` | `require` | `verify-ca`
    /// | `verify-full`. `None` uses sqlx's default (`prefer`).
    pub sslmode: Option<String>,
    /// `search_path` to pin (the connection's default schema), if any.
    pub search_path: Option<String>,
    /// Server-side `statement_timeout` in milliseconds, applied per session.
    pub statement_timeout_ms: Option<u64>,
}

fn default_external_port() -> u16 {
    5432
}

/// The `config` JSONB blob stored on a `postgres`-kind connection row (SOUL
/// §11/§13) — the non-secret half of an external database connection. The
/// password is intentionally absent: it lives encrypted in the secret store,
/// referenced by the row's `credential_ref`, so a dump of `connections` never
/// reveals a credential. Shared by the API's `ExternalDbRegistry` and the
/// ingest collect-SQL poller so both build identical [`PgConnectSpec`]s.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct PostgresConnectionConfig {
    pub host: String,
    #[serde(default = "default_external_port")]
    pub port: u16,
    pub database: String,
    pub username: String,
    /// libpq sslmode (`disable`/`require`/`verify-full`/…); `None` = sqlx default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sslmode: Option<String>,
    /// Default schema (`search_path`) to pin for every session on this connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Per-connection override of the pool size (else the caller's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_max: Option<u32>,
}

impl PostgresConnectionConfig {
    /// Combine this config with the decrypted `password` into the typed connect
    /// spec [`connect_external`] takes, pinning `statement_timeout_ms` per session.
    #[must_use]
    pub fn to_spec(&self, password: String, statement_timeout_ms: Option<u64>) -> PgConnectSpec {
        PgConnectSpec {
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            password,
            sslmode: self.sslmode.clone(),
            search_path: self.schema.clone(),
            statement_timeout_ms,
        }
    }
}

fn parse_ssl_mode(mode: &str) -> Result<PgSslMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "disable" => Ok(PgSslMode::Disable),
        "allow" => Ok(PgSslMode::Allow),
        "prefer" => Ok(PgSslMode::Prefer),
        "require" => Ok(PgSslMode::Require),
        "verify-ca" => Ok(PgSslMode::VerifyCa),
        "verify-full" => Ok(PgSslMode::VerifyFull),
        other => Err(StoreError::invalid(format!("unknown sslmode '{other}'"))),
    }
}

/// Build a pool to an external PostgreSQL database from a typed [`PgConnectSpec`].
/// Unlike [`connect_with`], this never parses a URL — every field is set on the
/// connect options directly, and `search_path`/`statement_timeout` are pinned as
/// session options so the caller's per-statement caps are enforced by the server.
pub async fn connect_external(spec: &PgConnectSpec, config: &PoolConfig) -> Result<PgPool> {
    let mut opts = PgConnectOptions::new()
        .host(&spec.host)
        .port(spec.port)
        .database(&spec.database)
        .username(&spec.username)
        .password(&spec.password);
    if let Some(mode) = &spec.sslmode {
        opts = opts.ssl_mode(parse_ssl_mode(mode)?);
    }
    // Pin session options: search_path (default schema) and a hard statement
    // timeout. Both are ordinary server GUCs, safe to set at connect time. Set in
    // one `.options()` call so neither overwrites the other.
    let mut session_opts: Vec<(&str, String)> = Vec::new();
    if let Some(sp) = &spec.search_path {
        session_opts.push(("search_path", sp.clone()));
    }
    if let Some(ms) = spec.statement_timeout_ms {
        session_opts.push(("statement_timeout", ms.to_string()));
    }
    if !session_opts.is_empty() {
        opts = opts.options(session_opts.iter().map(|(k, v)| (*k, v.as_str())));
    }
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(config.acquire_timeout)
        .connect_with(opts)
        .await?;
    Ok(pool)
}
