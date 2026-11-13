//! Backing services: Postgres, Valkey, Qdrant, Neo4j, secrets, external databases.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{workspace_assigned, Secret};

/// Qdrant — the derived vector index (SOUL §6.4). When `enabled = false` (the
/// default) the binary spawns no embed-capable ingest worker, so note ingestion
/// into Qdrant is off until a Qdrant is configured and enabled. The collection's
/// vector width is discovered from the embedding model, not set here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct QdrantConfig {
    /// Qdrant base URL, e.g. `http://localhost:6333`.
    pub url: String,
    /// Whether to run note embedding into Qdrant (the embed→upsert worker).
    pub enabled: bool,
}

impl Default for QdrantConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:6333".to_string(),
            enabled: false,
        }
    }
}

/// Neo4j — the derived graph projection (SOUL §6.3). `catalerum-graph` drives the
/// **HTTP transactional API** (port 7474), so `url` is the HTTP endpoint, not
/// Bolt. When `enabled = false` (default) the binary attaches no graph context,
/// so note→graph projection is off until configured + enabled.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Neo4jConfig {
    /// Neo4j HTTP base URL, e.g. `http://localhost:7474`.
    pub url: String,
    /// HTTP-basic user (the `neo4j` of `NEO4J_AUTH`).
    pub user: String,
    /// HTTP-basic password.
    pub password: Secret,
    /// Target database (default `neo4j`).
    pub database: String,
    /// Whether to run note→graph projection (the project-to-graph worker).
    pub enabled: bool,
}

impl Default for Neo4jConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:7474".to_string(),
            user: "neo4j".to_string(),
            password: "catalerum".into(),
            database: "neo4j".to_string(),
            enabled: false,
        }
    }
}

/// Postgres — the source of truth (SOUL §6.1).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct DatabaseConfig {
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgres://catalerum:catalerum@localhost:5432/catalerum".to_string(),
        }
    }
}

/// Secret-store configuration (SOUL §13/§29). `master_key` is the **base64**-
/// encoded 32-byte key that encrypts every workspace secret (external Postgres
/// passwords, …) at rest with AES-256-GCM. Set it via env
/// (`CATALERUM_SECRETS__MASTER_KEY`) in production — never the TOML. When empty
/// the secret store is disabled: features that need it (external DB connections)
/// refuse credential operations rather than store a password in the clear.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SecretsConfig {
    pub master_key: Secret,
}

impl SecretsConfig {
    /// Decode + validate the master key. `Ok(None)` when unset (feature
    /// disabled); `Ok(Some(key))` when a valid 32-byte base64 key is set;
    /// `Err` when set-but-malformed (bad base64 or wrong length), so boot can
    /// fail loudly rather than run with a broken key.
    ///
    /// # Errors
    /// A human-readable message when the key is present but not valid base64 of
    /// exactly 32 bytes.
    pub fn master_key_bytes(&self) -> Result<Option<[u8; 32]>, String> {
        use base64::Engine as _;
        if self.master_key.is_empty() {
            return Ok(None);
        }
        let raw = base64::engine::general_purpose::STANDARD
            .decode(self.master_key.expose().trim())
            .map_err(|e| format!("secrets.master_key is not valid base64: {e}"))?;
        let arr: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
            format!(
                "secrets.master_key must decode to 32 bytes for AES-256, got {}",
                raw.len()
            )
        })?;
        Ok(Some(arr))
    }
}

/// Defaults + hard caps for external PostgreSQL connections (SOUL §11/§19). The
/// caps bound the blast radius of a capability-gated `sql_query`: every read is
/// truncated at `max_rows` and every statement is bounded by `statement_timeout_ms`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ExternalDbConfig {
    /// Default maximum pooled connections per external connection.
    pub pool_max_connections: u32,
    /// Hard cap on a single SQL statement's execution time, in milliseconds.
    pub statement_timeout_ms: u64,
    /// Hard cap on the number of rows a read query may return.
    pub max_rows: u64,
    /// Config-defined PostgreSQL connections keyed by their workspace-visible
    /// name. These are reconciled into ordinary workspace-owned connection rows
    /// on first use so tools, automations, and migrations all use the same path as
    /// connections created at runtime.
    pub connections: BTreeMap<String, ExternalDbConnectionConfig>,
}

impl Default for ExternalDbConfig {
    fn default() -> Self {
        Self {
            pool_max_connections: 5,
            statement_timeout_ms: 15_000,
            max_rows: 1000,
            connections: BTreeMap::new(),
        }
    }
}

/// One config-defined external PostgreSQL connection. TOML:
/// `[external_db.connections.<name>]`. An empty [`workspaces`](Self::workspaces)
/// list assigns it to every workspace; otherwise entries are workspace slugs
/// (case-insensitive) or UUIDs, matching config-defined storage backends.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ExternalDbConnectionConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    /// Password supplied directly by config. Prefer [`password_env`](Self::password_env)
    /// so the value does not appear in the TOML file.
    pub password: Secret,
    /// Name of an environment variable containing the password. When set, it
    /// takes precedence over `password`; the variable must exist at runtime.
    pub password_env: String,
    /// libpq sslmode (`disable`/`require`/`verify-full`/…). Empty uses sqlx's
    /// default.
    pub sslmode: String,
    /// Default schema (`search_path`) pinned for every session. Empty means none.
    pub schema: String,
    /// Per-connection pool-size override. `0` uses the `[external_db]` default.
    pub pool_max: u32,
    /// Workspace slugs or UUIDs; empty assigns this connection to every workspace.
    pub workspaces: Vec<String>,
}

impl Default for ExternalDbConnectionConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 5432,
            database: String::new(),
            username: String::new(),
            password: Secret::default(),
            password_env: String::new(),
            sslmode: String::new(),
            schema: String::new(),
            pool_max: 0,
            workspaces: Vec::new(),
        }
    }
}

impl ExternalDbConnectionConfig {
    /// Whether this connection is assigned to workspace `ws`.
    #[must_use]
    pub fn assigned_to(&self, ws: &catalerum_core::model::Workspace) -> bool {
        workspace_assigned(&self.workspaces, ws)
    }

    /// Resolve the password without exposing it through `Debug`. A named
    /// environment variable is deliberately resolved lazily, immediately before
    /// the credential is encrypted into the workspace secret store.
    pub fn resolved_password(&self) -> Result<String, String> {
        let env_name = self.password_env.trim();
        if env_name.is_empty() {
            Ok(self.password.expose().to_string())
        } else {
            std::env::var(env_name).map_err(|_| {
                format!("external database password environment variable `{env_name}` is not set")
            })
        }
    }

    /// Convert to the non-secret connection payload persisted in Postgres.
    #[must_use]
    pub fn postgres_config(&self) -> catalerum_store::PostgresConnectionConfig {
        let optional = |value: &str| {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        };
        catalerum_store::PostgresConnectionConfig {
            host: self.host.trim().to_string(),
            port: self.port,
            database: self.database.trim().to_string(),
            username: self.username.trim().to_string(),
            sslmode: optional(&self.sslmode),
            schema: optional(&self.schema),
            pool_max: (self.pool_max != 0).then_some(self.pool_max),
        }
    }
}

/// Valkey coordination + token relay (SOUL §6.6). When `enabled = false` the
/// in-process bus fallback is used (single-pod dev, M1 default-safe).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ValkeyConfig {
    pub url: String,
    pub enabled: bool,
}

impl Default for ValkeyConfig {
    fn default() -> Self {
        Self {
            url: "redis://localhost:6379".to_string(),
            enabled: true,
        }
    }
}
