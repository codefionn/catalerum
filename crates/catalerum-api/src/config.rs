//! Runtime configuration for the API + binary (SOUL §13).
//!
//! The TOML config file is the immutable base; environment variables override
//! individual fields (prefix `CATALERUM_`, double-underscore section delimiter,
//! e.g. `CATALERUM_DATABASE__URL`). The binary parses this and hands a
//! [`Config`] to [`crate::build_router`] via [`crate::AppState`].

mod auth;
mod channels;
mod env;
mod exec;
mod ingest;
mod llm;
mod server;
mod services;
mod storage;
mod web;

#[cfg(test)]
mod tests;

pub use auth::*;
pub use channels::*;
pub use exec::*;
pub use ingest::*;
pub use llm::*;
pub use server::*;
pub use services::*;
pub use storage::*;
pub use web::*;

use serde::{Deserialize, Serialize};

/// A configuration secret — an API key, password, or bot token (SOUL §13). It
/// **redacts itself in `Debug`** (printing `Secret("***")`), so a stray
/// `tracing::info!(?config)` or a `{:?}` of any config struct can never leak a
/// credential into the logs. It (de)serializes **transparently** as a plain string
/// (so TOML + env round-trip unchanged), and the value is read explicitly via
/// [`expose`](Secret::expose) only where a client is actually built.
#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The underlying secret. Call this only where the value is actually needed
    /// (building an authenticated client) — never to log or serialize it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is unset (empty / whitespace).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the value; reveal only whether it is set.
        if self.is_empty() {
            f.write_str("Secret(\"\")")
        } else {
            f.write_str("Secret(\"***\")")
        }
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Secret {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// Top-level configuration, mirroring `config/catalerum.toml`.
///
/// Every section the API/binary needs is typed strictly; unknown sections still
/// round-trip via `#[serde(default)]` so a config written for a newer binary
/// loads cleanly.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub valkey: ValkeyConfig,
    pub llm: LlmConfig,
    pub telemetry: TelemetryConfig,
    pub ocr: OcrConfig,
    pub auth: AuthConfig,
    pub fetch: FetchConfig,
    pub search: SearchConfig,
    pub qdrant: QdrantConfig,
    pub neo4j: Neo4jConfig,
    pub curation: CurationConfig,
    pub exec: ExecConfig,
    pub mcp: McpConfig,
    pub channels: ChannelsConfig,
    pub storage: StorageConfig,
    pub preview: PreviewConfig,
    pub backup: BackupConfig,
    pub ui: UiConfig,
    pub secrets: SecretsConfig,
    pub external_db: ExternalDbConfig,
    pub sso: SsoConfig,
    pub google: GoogleConfig,
    pub microsoft: MicrosoftConfig,
}

// Inbound email is no longer a static `[email]` config block + always-on poller
// (SOUL §10/§28, revised): collection is a user-authored automation headed by a
// `CollectEmail` trigger you fill with an email connection (registered via
// `POST /email/connections`). Adding a connection provisions nothing — until a
// Collect automation exists the source is dormant. The old `EmailConfig` /
// `EmailSyncWorker` were removed with that change.
