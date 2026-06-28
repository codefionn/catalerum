//! Runtime configuration for the API + binary (SOUL §13).
//!
//! The TOML config file is the immutable base; environment variables override
//! individual fields (prefix `CATALERUM_`, double-underscore section delimiter,
//! e.g. `CATALERUM_DATABASE__URL`). The binary parses this and hands a
//! [`Config`] to [`crate::build_router`] via [`crate::AppState`].

use std::collections::BTreeMap;

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

/// Distributed tracing exporters. Both destinations are optional and may run
/// together; logs continue to be written to stderr regardless.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// OpenTelemetry `service.name` resource attribute.
    pub service_name: String,
    /// Head sampling ratio in the inclusive range 0..=1.
    pub sample_ratio: f64,
    /// Vendor-neutral OTLP/HTTP exporter.
    pub otlp: OtelExporterConfig,
    /// Optional direct Langfuse OTLP/HTTP exporter.
    pub langfuse: LangfuseTelemetryConfig,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "catalerum".to_string(),
            sample_ratio: 1.0,
            otlp: OtelExporterConfig::default(),
            langfuse: LangfuseTelemetryConfig::default(),
        }
    }
}

/// Which LLM payloads may be attached to exported generation spans. Metadata
/// (model, latency, token usage, cost, finish reason, errors) is always kept.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TelemetryContent {
    /// Export no prompt, completion, reasoning, tool arguments, or tool results.
    #[default]
    MetadataOnly,
    /// Export content after removing every system-role message.
    AllExceptSystemPrompts,
    /// Export the complete request and generated output, including system prompts.
    Everything,
}

impl std::str::FromStr for TelemetryContent {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "metadata-only" | "metadata" | "none" => Ok(Self::MetadataOnly),
            "all-except-system-prompts" | "without-system-prompts" => {
                Ok(Self::AllExceptSystemPrompts)
            }
            "everything" | "all" => Ok(Self::Everything),
            _ => Err(format!("unknown telemetry content mode: {value}")),
        }
    }
}

/// A generic OTLP/HTTP protobuf destination. The exporter appends `/v1/traces`
/// to `endpoint`, matching standard OTLP endpoint behavior.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OtelExporterConfig {
    pub enabled: bool,
    pub endpoint: String,
    /// Static HTTP headers. Values are redacted by [`Secret`]'s `Debug` impl.
    pub headers: BTreeMap<String, Secret>,
    pub content: TelemetryContent,
}

impl Default for OtelExporterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4318".to_string(),
            headers: BTreeMap::new(),
            content: TelemetryContent::MetadataOnly,
        }
    }
}

/// Langfuse's native OTLP endpoint and Basic-auth credentials. `endpoint` is a
/// base OTLP URL; `/v1/traces` is appended by the exporter.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LangfuseTelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub public_key: Secret,
    pub secret_key: Secret,
    pub content: TelemetryContent,
}

impl Default for LangfuseTelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "https://cloud.langfuse.com/api/public/otel".to_string(),
            public_key: Secret::default(),
            secret_key: Secret::default(),
            content: TelemetryContent::MetadataOnly,
        }
    }
}

/// Emerged UIs — AI-authored declarative UIs (SOUL §5/§12). `handler_tools` is
/// the **server-defined** allow-list of tools a UI event handler (or its Boa
/// script) may invoke. It is the trust boundary that keeps an AI-authored UI
/// strictly **less** powerful than chat — re-checked before dispatch even for an
/// Owner whose grant is otherwise wildcard. It deliberately excludes
/// destructive (`delete_*`), egress (`fetch_url`), channel (`notify`), and code
/// (`run_command`) tools; an admin can widen it in config. (`run_javascript`'s
/// own nested `catalerum.callTool` bridge is disabled for UI-handler contexts
/// regardless, so listing it here cannot tunnel past this allow-list.)
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct UiConfig {
    /// Tools a UI event handler may call (capability-gated regardless).
    pub handler_tools: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            handler_tools: [
                // Reads.
                "read_note",
                "list_notes",
                "query_structured",
                "search_files",
                "search_messages",
                "read_conversation",
                "read_event",
                "search_events",
                "kanban_next_task",
                "list_skills",
                "recall",
                // SQL against a workspace's external Postgres connections (SOUL §11).
                // Still capability-gated (`db:read@conn` / `db:write@conn`) and the
                // read path can never modify data, so it fits the safe-tool bar.
                "sql_query",
                // Safe writes (no delete / egress / channel / exec).
                "create_note",
                "edit_note",
                "kanban_create_board",
                "kanban_create_task",
                "kanban_move_task",
                "kanban_complete_task",
                "kanban_set_task_status",
                "kanban_edit_task",
                "create_calendar",
                "create_event",
                "update_event",
                "remember",
                "update_memory",
                "update_profile",
                // Per-App durable key/value store (SOUL §12/§29): where an emerged App
                // persists and reads its own data model so it outlives the session.
                // Reads gated on `ui:read` (every role), writes on `ui:write` (Member+,
                // base authority so a script may call them — never confirm-required).
                // From an App handler the namespace is forced to the firing App, so one
                // App can never reach another App's keys.
                "app_data_get",
                "app_data_list",
                "app_data_set",
                "app_data_delete",
                // Fire a named automation signal on demand (SOUL §11/§12) — lets an
                // emerged-UI button run a backend workflow. Safe: no delete / egress /
                // channel / exec, gated on `automation:write`, and each fired
                // automation still runs under its own §19 authority.
                "fire_trigger",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        }
    }
}

// Inbound email is no longer a static `[email]` config block + always-on poller
// (SOUL §10/§28, revised): collection is a user-authored automation headed by a
// `CollectEmail` trigger you fill with an email connection (registered via
// `POST /email/connections`). Adding a connection provisions nothing — until a
// Collect automation exists the source is dormant. The old `EmailConfig` /
// `EmailSyncWorker` were removed with that change.

/// The name of the **default** storage backend — the one the legacy top-level
/// `[storage]` fields configure, and the destination a file op picks when it names
/// no `store` (SOUL §9). A `[storage.backends.default]` table overrides it.
pub const DEFAULT_STORE_NAME: &str = "default";

/// Object storage (SOUL §9). A workspace can hold **many** named backends so a
/// file chooses where it lives. The legacy top-level fields (`local_path` / the
/// `bucket` name / `[storage.s3]` / `[storage.webdav]`) configure the **default**
/// backend (`"default"`), kept for backward compatibility and chosen by the same
/// S3 > WebDAV > local precedence as before. Each `[storage.backends.<name>]` table
/// adds another backend — several of the **same** kind are allowed (two local
/// folders, two S3 buckets, …), keyed by name, mirroring `[channels.*]`. Users can
/// also add backends at runtime (persisted as storage `Connection`s); both sources
/// surface uniformly as selectable stores. Empty (no default + no named backend, and
/// no runtime backend) = storage disabled for that workspace.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Local directory backing the default bucket (empty = no local backend).
    pub local_path: String,
    /// The default backend's bucket name (the S3 bucket, and the name reported in
    /// `StorageObject` triggers / listings; empty → `"default"`).
    pub bucket: String,
    /// The default backend's S3 / S3-compatible config (takes precedence over
    /// `local_path` when its credentials are set).
    pub s3: S3StorageConfig,
    /// The default backend's WebDAV config (used when its `url` is set and S3 is not).
    pub webdav: WebDavStorageConfig,
    /// Additional named backends keyed by store name, each an independent
    /// local/S3/WebDAV backend (SOUL §9). TOML: `[storage.backends.<name>]` with the
    /// same fields as the top-level default (`local_path` / `bucket` / `[…s3]` /
    /// `[…webdav]`). Several of the same kind are fine — the name disambiguates.
    pub backends: std::collections::HashMap<String, StorageBackendConfig>,
    /// Workspace assignment for the **default** backend (see
    /// [`StorageBackendConfig::workspaces`]) — empty means every workspace.
    pub workspaces: Vec<String>,
    /// Max upload size in bytes for `PUT /storage/objects/*` (0 → the 64 MiB
    /// default). Bounds in-memory buffering — an over-limit upload is rejected with
    /// `413 Payload Too Large` before the body is read. Global across backends.
    pub max_object_bytes: u64,
    /// Re-scan cadence (seconds) for [`watch`](StorageBackendConfig::watch)-enabled
    /// stores: the poll interval for backends with no native change events
    /// (S3/WebDAV), and the safety-net cadence for local stores (which also get
    /// real-time inotify on top). `0` → 60s; floored at 5s.
    pub watch_interval_secs: u64,
}

/// One named storage backend's settings (SOUL §9) — a local directory, an S3
/// bucket, or a WebDAV collection, picked by the same S3 > WebDAV > local
/// precedence the default backend uses. The shape mirrors the top-level
/// `[storage]` default so `[storage.backends.<name>]` reads identically.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct StorageBackendConfig {
    /// Local directory backing this backend (empty = no local backend).
    pub local_path: String,
    /// This backend's bucket name (empty → the store's own name).
    pub bucket: String,
    /// S3 / S3-compatible config (precedence over `local_path` when credentialed).
    pub s3: S3StorageConfig,
    /// WebDAV config (used when its `url` is set and S3 is not).
    pub webdav: WebDavStorageConfig,
    /// **Browse mode** (SOUL §9/§18): expose this backend's *raw* root — list,
    /// read, write, and delete objects at their literal keys with **no**
    /// per-workspace `<workspace_id>/` prefix. This lets a backend pointed at an
    /// *existing* directory (e.g. a local store on `~/Documents`, a pre-populated
    /// S3 bucket) surface the files already there in the Files panel, instead of
    /// only showing what catalerum itself uploaded under the namespace.
    ///
    /// WARNING: a browse store has **no tenant isolation** — every workspace sees
    /// (and can write/delete) the same raw directory, and the host paths are
    /// exposed verbatim. Use only on a trusted single-tenant deployment. Default
    /// off (namespaced, the multi-tenant-safe behavior).
    pub browse: bool,
    /// **Watch** this backend for changes and keep the §10 index in sync (SOUL
    /// §9/§10). The storage watch worker reconciles the catalogue with the backend:
    /// new or changed files are catalogued then ingested, and files that vanished
    /// are purged. A **local** backend is watched in real time (an inotify watcher
    /// triggers a prompt re-scan on any change); **S3/WebDAV** (no native change
    /// events) are re-scanned on the `watch_interval_secs` cadence. Default off.
    /// Pairs naturally with [`browse`](Self::browse) for a directory you keep
    /// editing, but works on any store (e.g. indexing a pre-populated S3 bucket).
    pub watch: bool,
    /// **Workspace assignment** (SOUL §9/§18): the workspaces this store belongs
    /// to, each entry a workspace **slug** (case-insensitive) or **UUID**. Empty
    /// (the default) = every workspace, the previous behavior. When set, only the
    /// listed workspaces see the store — it resolves (`?store=`), lists
    /// (`GET /storage/stores`), acts as a default, and is watch-scanned only
    /// there; everywhere else it is as if the store didn't exist (fail-closed,
    /// including while a listed workspace can't be loaded). An entry matching no
    /// existing workspace is inert until such a workspace appears — config is the
    /// immutable base, workspaces are runtime state (SOUL principle 10).
    pub workspaces: Vec<String>,
}

/// Whether a [`workspaces`](StorageBackendConfig::workspaces) assignment admits
/// workspace `ws`: an empty list admits every workspace; otherwise some entry
/// must equal the workspace's slug (case-insensitive) or its UUID.
#[must_use]
pub fn workspace_assigned(assignment: &[String], ws: &catalerum_core::model::Workspace) -> bool {
    assignment.is_empty()
        || assignment.iter().any(|sel| {
            let sel = sel.trim();
            sel.eq_ignore_ascii_case(&ws.slug)
                || sel
                    .parse::<catalerum_core::WorkspaceId>()
                    .is_ok_and(|id| id == ws.id)
        })
}

impl StorageBackendConfig {
    /// Whether any backend (local, S3, or WebDAV) is configured here.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.local_path.trim().is_empty() || self.s3.enabled() || self.webdav.enabled()
    }

    /// Whether this store is assigned to workspace `ws` (see
    /// [`workspaces`](Self::workspaces); empty = every workspace).
    #[must_use]
    pub fn assigned_to(&self, ws: &catalerum_core::model::Workspace) -> bool {
        workspace_assigned(&self.workspaces, ws)
    }

    /// The effective bucket name (`fallback` — usually the store name — when unset).
    #[must_use]
    pub fn bucket_name<'a>(&'a self, fallback: &'a str) -> &'a str {
        let b = self.bucket.trim();
        if b.is_empty() {
            fallback
        } else {
            b
        }
    }

    /// Which concrete backend this entry resolves to by precedence (S3 > WebDAV >
    /// local), or `None` when nothing is configured. Used to label a store in
    /// listings.
    #[must_use]
    pub fn kind(&self) -> Option<&'static str> {
        if self.s3.enabled() {
            Some("s3")
        } else if self.webdav.enabled() {
            Some("webdav")
        } else if !self.local_path.trim().is_empty() {
            Some("local")
        } else {
            None
        }
    }
}

/// Default object upload cap (64 MiB) — well above axum's restrictive 2 MiB body
/// default (which would reject ordinary documents), yet bounded against an
/// unbounded-upload OOM (SOUL §9).
pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// WebDAV object storage (SOUL §9/§13): a collection on a WebDAV server (Nextcloud,
/// `rclone serve webdav`, Apache `mod_dav`, …). Credentials should be injected via
/// the environment (`CATALERUM_STORAGE__WEBDAV__PASSWORD=…`), not the TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct WebDavStorageConfig {
    /// Collection root URL, e.g. `http://localhost:8788/` (empty = WebDAV disabled).
    pub url: String,
    /// HTTP-basic username (empty = anonymous).
    pub username: String,
    /// HTTP-basic password (set via env in production — never the TOML).
    pub password: Secret,
}

impl WebDavStorageConfig {
    /// Whether a WebDAV collection URL is configured.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.url.trim().is_empty()
    }
}

/// S3-compatible object storage (SOUL §9/§13). Secrets should be injected via the
/// environment (`CATALERUM_STORAGE__S3__SECRET_KEY=…`), not committed to the TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct S3StorageConfig {
    /// Service endpoint, e.g. `http://localhost:9000` for MinIO. Empty → AWS's
    /// default regional endpoint.
    pub endpoint: String,
    /// Region (empty → `us-east-1`; ignored by most self-hosted gateways).
    pub region: String,
    /// Access key id (set via env in production).
    pub access_key: String,
    /// Secret access key (set via env in production — never the TOML).
    pub secret_key: Secret,
    /// Path-style addressing — `true` for MinIO and most self-hosted gateways,
    /// `false` for AWS (virtual-host style).
    pub path_style: bool,
}

impl S3StorageConfig {
    /// Whether S3 credentials are configured (the bucket name comes from the parent
    /// [`StorageConfig::bucket_name`]).
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.access_key.trim().is_empty() && !self.secret_key.is_empty()
    }

    /// The effective region (`us-east-1` when unset).
    #[must_use]
    pub fn region_name(&self) -> &str {
        let r = self.region.trim();
        if r.is_empty() {
            "us-east-1"
        } else {
            r
        }
    }
}

impl StorageConfig {
    /// Whether any storage backend — the default **or** a named one — is configured.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.default_backend().enabled()
            || self.backends.values().any(StorageBackendConfig::enabled)
    }

    /// The effective default-backend bucket name (`"default"` when unset).
    #[must_use]
    pub fn bucket_name(&self) -> &str {
        let b = self.bucket.trim();
        if b.is_empty() {
            DEFAULT_STORE_NAME
        } else {
            b
        }
    }

    /// The legacy top-level fields as a single [`StorageBackendConfig`] — the
    /// `"default"` backend.
    #[must_use]
    pub fn default_backend(&self) -> StorageBackendConfig {
        StorageBackendConfig {
            local_path: self.local_path.clone(),
            bucket: self.bucket.clone(),
            s3: self.s3.clone(),
            webdav: self.webdav.clone(),
            // The legacy default store is always the namespaced, tenant-isolated
            // canonical store; browse mode is opt-in per named/runtime backend.
            browse: false,
            // Watching is opt-in per named/runtime backend (the default store holds
            // only what catalerum uploaded, which is catalogued + ingested already).
            watch: false,
            workspaces: self.workspaces.clone(),
        }
    }

    /// Every configured backend as `(store_name, cfg)`: the legacy `"default"`
    /// backend (when set, unless a `[storage.backends.default]` overrides it) plus
    /// each `[storage.backends.<name>]`. Only **enabled** entries are returned, so a
    /// stray empty table is ignored. The destination a file op picks when it names
    /// no store is [`DEFAULT_STORE_NAME`].
    #[must_use]
    pub fn resolved_backends(&self) -> Vec<(String, StorageBackendConfig)> {
        let mut out: Vec<(String, StorageBackendConfig)> = Vec::new();
        let default = self.default_backend();
        if default.enabled() && !self.backends.contains_key(DEFAULT_STORE_NAME) {
            out.push((DEFAULT_STORE_NAME.to_string(), default));
        }
        let mut named: Vec<(&String, &StorageBackendConfig)> =
            self.backends.iter().filter(|(_, c)| c.enabled()).collect();
        // Deterministic order (a HashMap iterates arbitrarily) so the registry,
        // listings, and the default-store pick are stable across boots.
        named.sort_by(|a, b| a.0.cmp(b.0));
        out.extend(named.into_iter().map(|(n, c)| (n.clone(), c.clone())));
        out
    }

    /// The effective max upload size in bytes ([`DEFAULT_MAX_OBJECT_BYTES`] when
    /// unset/0).
    #[must_use]
    pub fn max_object_bytes(&self) -> u64 {
        if self.max_object_bytes == 0 {
            DEFAULT_MAX_OBJECT_BYTES
        } else {
            self.max_object_bytes
        }
    }

    /// The effective watch re-scan cadence in seconds (60s when unset/0; floored at
    /// 5s so a typo can't busy-loop the watcher).
    #[must_use]
    pub fn watch_interval_secs(&self) -> u64 {
        if self.watch_interval_secs == 0 {
            60
        } else {
            self.watch_interval_secs.max(5)
        }
    }
}

/// Disaster-recovery backup (SOUL §30). When `enabled`, the binary spawns a
/// [`catalerum_backup::BackupWorker`](../../catalerum_backup/struct.BackupWorker.html)
/// that, every [`interval`](BackupConfig::interval), dumps Postgres (the source
/// of truth, §6.1) and copies the object blobs (§9) to `destination` — an
/// **independent** storage backend (an S3 bucket "and so on", a WebDAV
/// collection, or a local directory), distinct from the live `[storage]` so a
/// backup survives the loss of the primary. `catalerum backup` / `catalerum
/// restore` run the same engine on demand. Neo4j + Qdrant are derived and
/// rebuildable, so they are not backed up (§6.3/§6.4). Off by default.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct BackupConfig {
    /// Whether to run the scheduled backup worker. `catalerum backup`/`restore`
    /// work regardless of this flag, as long as `destination` is set.
    pub enabled: bool,
    /// Backup cadence in seconds (0 → the 24-hour default; floored to 60s).
    pub interval_secs: u64,
    /// The destination "directory" prefix backups live under (empty →
    /// `"backups"`).
    pub prefix: String,
    /// How many recent backups to retain; older ones are pruned after each run
    /// (0 → keep all).
    pub keep: u32,
    /// Whether to include object blobs (§9) in the backup (default `true`); set
    /// `false` for a Postgres-only backup.
    pub include_objects: bool,
    /// Where backups are written — the same shape as `[storage]` (a local
    /// directory, an S3/MinIO bucket, or a WebDAV collection), but a *separate*
    /// backend from the live data. Empty = no destination (backups disabled even
    /// when `enabled`).
    pub destination: StorageConfig,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 0,
            prefix: String::new(),
            keep: 7,
            include_objects: true,
            destination: StorageConfig::default(),
        }
    }
}

/// The default backup cadence (24 hours) when `interval_secs` is unset/0.
pub const DEFAULT_BACKUP_INTERVAL_SECS: u64 = 24 * 60 * 60;

impl BackupConfig {
    /// Whether a destination backend is configured (a backup can be written).
    #[must_use]
    pub fn has_destination(&self) -> bool {
        self.destination.enabled()
    }

    /// The effective backup interval ([`DEFAULT_BACKUP_INTERVAL_SECS`] when
    /// unset/0; floored to 60s by the worker).
    #[must_use]
    pub fn interval(&self) -> std::time::Duration {
        let secs = if self.interval_secs == 0 {
            DEFAULT_BACKUP_INTERVAL_SECS
        } else {
            self.interval_secs
        };
        std::time::Duration::from_secs(secs)
    }

    /// The effective destination prefix (`"backups"` when unset).
    #[must_use]
    pub fn prefix_name(&self) -> &str {
        let p = self.prefix.trim();
        if p.is_empty() {
            "backups"
        } else {
            p
        }
    }

    /// Retention count as a `usize` (0 → keep all).
    #[must_use]
    pub fn keep(&self) -> usize {
        self.keep as usize
    }
}

/// Outbound messaging channels (SOUL §25). Configured channels are the delivery
/// targets for the `notify` tool (§7) and the `Notify { channel }` automation
/// action (§11), routed by **name** across providers. Empty = no channel (the
/// `notify` tool isn't registered).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Discord incoming-webhook URLs keyed by channel name (the `Notify`/`notify`
    /// `channel`). The `default` entry delivers when no channel is named. TOML:
    /// `[channels.discord]` with `name = "https://discord.com/api/webhooks/…"` pairs.
    /// Outbound only (a webhook can't receive).
    pub discord: std::collections::HashMap<String, String>,
    /// Slack incoming-webhook URLs keyed by channel name — the same shape as
    /// `discord`. TOML: `[channels.slack]` with `name = "https://hooks.slack.com/…"`
    /// pairs. Outbound only.
    pub slack: std::collections::HashMap<String, String>,
    /// Telegram bot channels keyed by channel name. Each delivers via a bot's
    /// `sendMessage` to one chat. TOML: a `[channels.telegram.<name>]` table per
    /// channel with `bot_token` + `chat_id` (+ optional `base_url`, `inbound`).
    pub telegram: std::collections::HashMap<String, TelegramChannelConfig>,
    /// Matrix bot channels keyed by channel name. Each sends to + (with `inbound`)
    /// receives from one room over a bot's access token. TOML: a
    /// `[channels.matrix.<name>]` table with `homeserver` + `access_token` +
    /// `room_id` (+ `user_id` for inbound echo-filtering, `inbound`).
    pub matrix: std::collections::HashMap<String, MatrixChannelConfig>,
}

/// A Telegram bot delivery target (SOUL §25): a bot token + the chat it posts to.
/// `base_url` overrides the Bot API host (default `https://api.telegram.org`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TelegramChannelConfig {
    /// Bot token from BotFather (`<id>:<secret>`).
    pub bot_token: Secret,
    /// Destination chat id (a user, group, or channel).
    pub chat_id: String,
    /// Override the Bot API base URL; empty = the public `https://api.telegram.org`.
    pub base_url: String,
    /// Receive inbound messages from this chat via `getUpdates` long-polling, firing
    /// `ChannelMessage` triggers (SOUL §11/§25). Off by default (a poll loop holds a
    /// connection + a single `getUpdates` consumer, so it is opt-in).
    pub inbound: bool,
}

impl TelegramChannelConfig {
    /// Whether this entry has the credentials to deliver.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.bot_token.is_empty() && !self.chat_id.trim().is_empty()
    }
}

/// A Matrix bot channel (SOUL §25): a homeserver + bot access token + a room, used
/// for both delivery (`/send`) and — when `inbound` — receive (`/sync`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MatrixChannelConfig {
    /// Homeserver base URL (e.g. `https://matrix.org`).
    pub homeserver: String,
    /// The bot user's access token.
    pub access_token: Secret,
    /// The room id to send to / receive from (e.g. `!abc:example.org`).
    pub room_id: String,
    /// The bot's own Matrix user id (e.g. `@assistant:example.org`). Required for
    /// `inbound` — it filters the bot's own messages so a reply never re-triggers
    /// the agent (an echo loop). Optional for delivery-only.
    pub user_id: String,
    /// Receive inbound messages from `room_id` via `/sync` long-polling, firing
    /// `ChannelMessage` triggers (SOUL §11/§25). Off by default; ignored (with a
    /// warning) when `user_id` is empty.
    pub inbound: bool,
}

impl MatrixChannelConfig {
    /// Whether this entry has the credentials to deliver.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.homeserver.trim().is_empty()
            && !self.access_token.is_empty()
            && !self.room_id.trim().is_empty()
    }
}

impl ChannelsConfig {
    /// Whether any channel is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.discord.values().all(|u| u.trim().is_empty())
            && self.slack.values().all(|u| u.trim().is_empty())
            && self.telegram.values().all(|t| !t.is_configured())
            && self.matrix.values().all(|m| !m.is_configured())
    }
}

/// Command/code execution (SOUL §20). **Protected, opt-in.** When `enabled`
/// (default off) the binary builds a local `Executor` and registers the
/// `run_command` tool — which still requires the `exec:run` capability (no base
/// role grants it, §19). `allow` restricts which program names may run (empty →
/// any program; still capability-gated). The Local backend is the highest blast
/// radius; container/bao backends land later.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExecConfig {
    /// Whether to enable the executor + `run_command` / terminal tools.
    pub enabled: bool,
    /// Allowed program names (basename or exact argv[0]); empty = any.
    pub allow: Vec<String>,
    /// Default terminal/exec backend when a workdir doesn't pin one:
    /// `local` | `sandbox` | `container` | `kubernetes`. Empty → `local`.
    pub backend: String,
    /// Shell to launch for interactive terminals (e.g. `/bin/bash`); may carry
    /// arguments (whitespace-separated), e.g. `/usr/bin/env bash`. Empty → a
    /// deterministic, wizard-free default: `/usr/bin/env bash` on Unix and
    /// PowerShell on Windows (`pwsh` when on `PATH`, else `powershell`) for the
    /// local/sandbox host backend, `/bin/sh` inside a container/k8s session. The
    /// user's interactive `$SHELL` is deliberately **not** used — its first-run
    /// setup (zsh/fish new-user wizard) can block the PTY and wedge the session.
    pub shell: String,
    /// Root directory for ephemeral terminal temp dirs (local/sandbox backends).
    /// Empty → the system temp dir.
    pub ephemeral_root: String,
    /// Podman/Docker container backend settings (SOUL §20).
    pub podman: PodmanExecConfig,
    /// Kubernetes backend settings (SOUL §20).
    pub k8s: K8sExecConfig,
    /// Run **one long-lived sandbox per workspace** (the operator posture, SOUL
    /// §20) for the container/kubernetes backends: terminal sessions and
    /// `run_command` `exec` into a single `catalerum-ws-<id>` container/Pod with a
    /// shared persistent `/work` volume, instead of a fresh container per call.
    /// No effect on the local/sandbox backends.
    pub per_workspace: bool,
    /// Destroy an idle per-workspace container after this many seconds with no
    /// live sessions (podman backend; the k8s operator GCs its own). `0` → the
    /// built-in default (1800s). Only meaningful with `per_workspace`.
    pub idle_timeout_secs: u64,
    /// Persistent `/work` volume size for the per-workspace sandbox (k8s PVC size,
    /// e.g. `10Gi`); empty → `10Gi`. Ignored by podman (named volumes are unsized).
    pub volume_size: String,
    /// Network policy for the per-workspace sandbox; empty → **full** network
    /// (the per-workspace default, distinct from the per-session container
    /// backend's `none`). Set e.g. `none` to isolate it.
    pub sandbox_network: String,
}

impl ExecConfig {
    /// The parsed default [`ExecutorKind`](catalerum_core::model::ExecutorKind)
    /// backend (`Local` when unset/unknown).
    #[must_use]
    pub fn backend_kind(&self) -> catalerum_core::model::ExecutorKind {
        catalerum_core::model::ExecutorKind::parse_token(&self.backend)
            .unwrap_or(catalerum_core::model::ExecutorKind::Local)
    }

    /// Idle timeout for the per-workspace sandbox (`0` → 1800s default).
    #[must_use]
    pub fn sandbox_idle_timeout_secs(&self) -> u64 {
        if self.idle_timeout_secs == 0 {
            1800
        } else {
            self.idle_timeout_secs
        }
    }

    /// Persistent `/work` volume size for the per-workspace sandbox (empty →
    /// `10Gi`).
    #[must_use]
    pub fn sandbox_volume_size(&self) -> String {
        let v = self.volume_size.trim();
        if v.is_empty() {
            "10Gi".to_string()
        } else {
            v.to_string()
        }
    }

    /// The default [`SandboxSpec`](catalerum_exec::SandboxSpec) for the
    /// per-workspace sandbox, derived from this config: backend-appropriate
    /// image, full network unless `sandbox_network` narrows it, and the `/work`
    /// volume size. Shared by the binary (sandbox construction) and `AppState`
    /// (the manager's default spec).
    #[must_use]
    pub fn sandbox_spec(&self) -> catalerum_exec::SandboxSpec {
        use catalerum_core::model::ExecutorKind;
        let net = self.sandbox_network.trim();
        let image = match self.backend_kind() {
            ExecutorKind::Kubernetes => self.k8s.image.clone(),
            _ => self.podman.image.clone(),
        };
        catalerum_exec::SandboxSpec {
            image: (!image.trim().is_empty()).then_some(image),
            limits: catalerum_core::provider::ResourceLimits {
                network: (!net.is_empty()).then(|| net.to_string()),
                ..catalerum_core::provider::ResourceLimits::default()
            },
            volume_size: Some(self.sandbox_volume_size()),
        }
    }
}

/// Podman/Docker container terminal backend (SOUL §20): a long-lived container
/// per session, the workdir bind-mounted / a named volume, CPU/mem/net limits.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PodmanExecConfig {
    /// CLI binary to drive: `podman` (default) or `docker`.
    pub binary: String,
    /// Default container image for a terminal session.
    pub image: String,
    /// Network policy passed to the runtime (`none` default = no egress).
    pub network: String,
}

impl Default for PodmanExecConfig {
    fn default() -> Self {
        Self {
            binary: "podman".to_string(),
            image: "docker.io/library/debian:stable-slim".to_string(),
            network: "none".to_string(),
        }
    }
}

impl PodmanExecConfig {
    /// The CLI binary to invoke (`podman` when unset).
    #[must_use]
    pub fn binary_name(&self) -> &str {
        let b = self.binary.trim();
        if b.is_empty() {
            "podman"
        } else {
            b
        }
    }
}

/// Kubernetes terminal backend (SOUL §20): a long-lived Pod per session,
/// `exec`/`attach` for the PTY, a PVC-mounted workdir. Not a full CRD operator.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct K8sExecConfig {
    /// Namespace to create session Pods in (default `default`).
    pub namespace: String,
    /// Default container image for a session Pod.
    pub image: String,
    /// StorageClass for the per-workdir PVC (empty → the cluster default).
    pub storage_class: String,
}

impl Default for K8sExecConfig {
    fn default() -> Self {
        Self {
            namespace: "default".to_string(),
            image: "docker.io/library/debian:stable-slim".to_string(),
            storage_class: String::new(),
        }
    }
}

/// External MCP servers catalerum connects to **as a client** (SOUL §26 — the
/// inbound half of principle 15). Each entry spawns a stdio MCP server (e.g.
/// `npx @playwright/mcp`) at startup; its tools join the §7 registry as
/// `{server}_{tool}`, each gated on `mcp:use@{server}` — a protected scope no
/// base role holds (§19), so remote tools are reachable only by an owner or an
/// explicitly granted agent, exactly like `run_command`. A server that fails to
/// start is logged and skipped, never blocking boot.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpConfig {
    /// The external servers to connect to (each `[[mcp.servers]]`).
    pub servers: Vec<McpServerConfig>,
}

/// One external MCP server to import tools from, over **stdio** (spawn a child
/// process) or **http** (the streamable-HTTP transport, with optional auth).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct McpServerConfig {
    /// Stable name; prefixes the server's tools and scopes the `mcp:use@{name}`
    /// capability. Empty → the entry is skipped.
    pub name: String,
    /// `"stdio"` (default — spawn `command`) or `"http"` (connect to `url`).
    pub transport: String,
    /// Program to spawn (stdio transport, e.g. `npx`, `uvx`, or an absolute path).
    pub command: String,
    /// Arguments passed to `command` (stdio transport).
    pub args: Vec<String>,
    /// Extra environment variables for the child process (stdio transport).
    pub env: BTreeMap<String, String>,
    /// The endpoint URL (http transport, e.g. `https://host/mcp`).
    pub url: String,
    /// How to authenticate (http transport). Default: none.
    pub auth: McpAuthConfig,
    /// Whether to connect this server (default `true`; lets one be disabled
    /// without deleting its config).
    pub enabled: bool,
    /// Optional allow-list of remote tool names to import; empty → import all.
    pub tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: "stdio".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            url: String::new(),
            auth: McpAuthConfig::default(),
            // Listing a server is itself opt-in; default it on so `enabled` only
            // ever needs setting to turn one *off*.
            enabled: true,
            tools: Vec::new(),
        }
    }
}

impl McpServerConfig {
    /// Whether this transport is HTTP (`"http"`/`"sse"`/`"streamable-http"`),
    /// else stdio.
    #[must_use]
    pub fn is_http(&self) -> bool {
        matches!(
            self.transport.trim().to_ascii_lowercase().as_str(),
            "http" | "https" | "sse" | "streamable-http"
        )
    }

    /// Whether this entry is usable: named, with a command (stdio) or URL (http).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        if self.name.trim().is_empty() {
            return false;
        }
        if self.is_http() {
            !self.url.trim().is_empty()
        } else {
            !self.command.trim().is_empty()
        }
    }

    /// The env map as ordered `(key, value)` pairs for the spawn API.
    #[must_use]
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// Authentication for an HTTP MCP server (SOUL §26). `kind` selects the mode;
/// only that mode's fields are read. Secrets use [`Secret`] (redacted in `Debug`,
/// supplied via env, never logged).
///
/// - `none` — no credential.
/// - `bearer` — `Authorization: Bearer {token}`.
/// - `header` — a static `{header_name}: {header_value}` (e.g. `X-Api-Key`).
/// - `oauth2` — machine-to-machine SSO: a `client_credentials` service account or
///   a `refresh_token` grant against `token_url`; access tokens are fetched and
///   refreshed automatically.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpAuthConfig {
    /// `none` (default) | `bearer` | `header` | `oauth2`.
    pub kind: String,
    /// Bearer token (`kind = "bearer"`).
    pub token: Secret,
    /// Header name (`kind = "header"`).
    pub header_name: String,
    /// Header value (`kind = "header"`).
    pub header_value: Secret,
    /// OAuth2 token endpoint (`kind = "oauth2"`).
    pub token_url: String,
    /// OAuth2 grant: `client_credentials` (default) or `refresh_token`.
    pub grant_type: String,
    /// OAuth2 client id.
    pub client_id: String,
    /// OAuth2 client secret.
    pub client_secret: Secret,
    /// OAuth2 refresh token (`grant_type = "refresh_token"`).
    pub refresh_token: Secret,
    /// OAuth2 scopes, space-separated.
    pub scope: String,
}

/// Background memory auto-curation (SOUL §22). When `enabled` (default off), each
/// chat turn enqueues an `extract_memories` job and the binary attaches a curate
/// context to the worker. `model` overrides the LLM used to extract (empty →
/// fall back to `[llm].default_model`).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CurationConfig {
    /// Whether to mine conversations for durable memories.
    pub enabled: bool,
    /// Extraction model; empty falls back to `[llm].default_model`.
    pub model: String,
}

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

/// The deployment mode (SOUL §13/§18). One instance-level knob whose invariant
/// is: **mode changes presentation, defaults, and optimizations — never the
/// model.** Schema, API, authorization, and org/workspace tenancy are identical in
/// both; a single-user instance is a multi-tenant instance with one human in it,
/// and flipping the mode later is a config edit, not a migration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    /// Optimizes for one person (what `just dev` runs): the UI surfaces full
    /// per-user settings depth, hides member/role/SSO chrome, auto-selects the
    /// default organisation/workspace, and the runtime may cache around the sole
    /// user. Creation policies default to `members`. This is the default when the
    /// knob is unset — a fresh, zero-config instance is single-operator.
    #[default]
    SingleUser,
    /// Optimizes for a shared deployment: admins preconfigure defaults and members
    /// see fewer knobs, while member-management + SSO surfaces appear. Creation
    /// policies default to `admins`.
    MultiUser,
}

impl DeploymentMode {
    /// The lowercase token (`single_user` | `multi_user`) — the presentation-only
    /// value the web app reads to shape nav/settings chrome (SOUL §18).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DeploymentMode::SingleUser => "single_user",
            DeploymentMode::MultiUser => "multi_user",
        }
    }

    /// Whether the runtime may cache around a **sole user** (SOUL §18/§29): true
    /// only in `single_user`, where one human means a per-workspace personalization
    /// cache (the profile snapshot) is coherent and process-local. `multi_user`
    /// never consults it (mode gates optimizations only, never correctness).
    #[must_use]
    pub fn caches_sole_user(self) -> bool {
        matches!(self, DeploymentMode::SingleUser)
    }

    /// The mode's default creation policy, applied to both the instance
    /// `organisation_creation` policy and a newly-created org's `workspace_creation`
    /// policy when neither is set explicitly: `Members` in single-user, `Admins`
    /// in multi-user (SOUL §18).
    #[must_use]
    pub fn default_creation_policy(self) -> catalerum_core::model::CreationPolicy {
        use catalerum_core::model::CreationPolicy;
        match self {
            DeploymentMode::SingleUser => CreationPolicy::Members,
            DeploymentMode::MultiUser => CreationPolicy::Admins,
        }
    }
}

/// The Axum API listen address and the externally-visible base URL used for
/// magic-link URLs (SOUL §17/§18).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Deployment mode (SOUL §18) — presentation/defaults/optimizations only,
    /// never schema/API/authz/tenancy. Defaults to `single_user` when unset.
    pub mode: DeploymentMode,
    /// Instance policy: who may create **new organisations**
    /// (`disabled` | `admins` | `members`, SOUL §18). Deny-by-default; unset falls
    /// back to the deployment mode's default (`members` in single-user, `admins`
    /// in multi-user) via [`effective_organisation_creation`](Self::effective_organisation_creation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organisation_creation: Option<catalerum_core::model::CreationPolicy>,
    /// Socket address the API binds, e.g. `0.0.0.0:8787`.
    pub listen: String,
    /// This pod's **peer-reachable IP** for cross-pod session forwarding (SOUL
    /// §16 M7): combined with `listen`'s port and announced on the bus registry
    /// so a peer can route a pod-local terminal request to its owner. On k8s,
    /// inject the downward-API pod IP (`CATALERUM_SERVER__POD_IP` from
    /// `status.podIP`). Empty (the default) auto-detects the primary local IP;
    /// forwarding degrades gracefully when no address can be determined.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pod_ip: String,
    /// Public base URL the API + magic links are rendered against. If unset the
    /// service falls back to `http://<listen>`.
    pub base_url: Option<String>,
    /// Public URL of the Leptos web workbench (the SPA origin). The dev
    /// magic-link redeem endpoint 302-redirects here with a one-time handoff
    /// code so dev login is a single click (SOUL §17). Defaults to
    /// `http://localhost:8080` (the `trunk serve` address). This origin is
    /// always on the CORS allow-list.
    pub web_url: String,
    /// Additional origins on the CORS allow-list (beyond `web_url`), e.g. a
    /// second SPA or an admin console served from another origin. The API
    /// answers cross-origin browser requests **only** from these origins
    /// (deny-by-default; non-browser clients are unaffected by CORS).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cors_extra_origins: Vec<String>,
    /// Secret that signs `download_link` tokens (SOUL §9) — the short-lived,
    /// single-file/-directory links the agent hands the user (`GET /download/{token}`).
    /// Set it (any string) so links survive a restart and verify across pods; leave
    /// it unset and each process signs with a fresh random key (links then die with
    /// the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_secret: Option<Secret>,
    /// Secret that signs **trigger-fire** links (SOUL §11/§12) — the short-lived
    /// public URLs (`POST /triggers/fire/{token}`) an external service uses to fire
    /// one named automation signal without a login. Set it (any string) so links
    /// survive a restart and verify across pods; leave it unset and each process
    /// signs with a fresh random key (links then die with the process — fine for
    /// single-pod dev). Independent of [`download_secret`](Self::download_secret) so
    /// the two can be rotated separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_secret: Option<Secret>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            mode: DeploymentMode::default(),
            organisation_creation: None,
            listen: "0.0.0.0:8787".to_string(),
            pod_ip: String::new(),
            base_url: None,
            web_url: "http://localhost:8080".to_string(),
            cors_extra_origins: Vec::new(),
            download_secret: None,
            trigger_secret: None,
        }
    }
}

impl ServerConfig {
    /// The effective instance `organisation_creation` policy (SOUL §18): the
    /// explicit config value, else the deployment mode's default (`members` in
    /// single-user, `admins` in multi-user). Deny-by-default gating consumes this.
    #[must_use]
    pub fn effective_organisation_creation(&self) -> catalerum_core::model::CreationPolicy {
        self.organisation_creation
            .unwrap_or_else(|| self.mode.default_creation_policy())
    }

    /// The default `workspace_creation` policy stamped onto a **newly-created**
    /// organisation (SOUL §18) — the deployment mode's default. Existing orgs keep
    /// their stored policy; this only seeds new ones at creation time.
    #[must_use]
    pub fn default_workspace_creation(&self) -> catalerum_core::model::CreationPolicy {
        self.mode.default_creation_policy()
    }

    /// The effective base URL: explicit `base_url` or `http://<listen>`.
    #[must_use]
    pub fn effective_base_url(&self) -> String {
        match &self.base_url {
            Some(b) => b.trim_end_matches('/').to_string(),
            None => format!("http://{}", self.listen),
        }
    }

    /// The web workbench (SPA) origin, trailing slash trimmed. The magic-link
    /// redeem endpoint 302-redirects here with a one-time handoff code.
    #[must_use]
    pub fn effective_web_url(&self) -> String {
        self.web_url.trim_end_matches('/').to_string()
    }

    /// The full CORS allow-list: the SPA origin plus any configured extras,
    /// trailing slashes trimmed, blanks dropped.
    #[must_use]
    pub fn cors_allowed_origins(&self) -> Vec<String> {
        std::iter::once(self.effective_web_url())
            .chain(
                self.cors_extra_origins
                    .iter()
                    .map(|o| o.trim().trim_end_matches('/').to_string()),
            )
            .filter(|o| !o.is_empty())
            .collect()
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

/// llmleaf endpoint (SOUL §7). llmleaf is multi-modal: chat, embeddings, TTS, and
/// STT all go through this one OpenRouter-shaped endpoint (`base_url` + `api_key`);
/// only the per-modality model differs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    /// llmleaf origin, **without** the `/v1` version prefix (e.g.
    /// `http://localhost:8088`). The `llmleaf-client` SDK appends the versioned
    /// path itself (`/v1/responses`, `/v1/embeddings`, …), so a `…/v1`
    /// here double-prefixes (`/v1/v1/…`) and every call 404s.
    pub base_url: String,
    pub api_key: Secret,
    /// Bearer accepted by the loopback llmleaf topology pull endpoint. Empty
    /// disables that machine endpoint; inject it in all-in-one/operator setups.
    pub control_token: Secret,
    /// Enable Catalerum's dynamic llmleaf provider/route control plane. Off by
    /// default: ordinary deployments keep llmleaf topology in its own immutable
    /// configuration. The all-in-one profile opts in so it can offer guided
    /// provider and route management from the workbench.
    pub control_plane_enabled: bool,
    /// Model for chat (`/responses`).
    pub default_model: String,
    /// Model for embeddings (`/embeddings`); the vectors feed Qdrant (SOUL §6.4).
    pub embedding_model: String,
    /// Optional embedding output dimensionality (Matryoshka truncation); `None`
    /// (or `0` in TOML via the absence of the key) uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_dimensions: Option<u32>,
    /// Model for text-to-speech (`/audio/speech`).
    pub speech_model: String,
    /// Default voice for text-to-speech (e.g. `alloy`).
    pub speech_voice: String,
    /// Model for speech-to-text (`/audio/transcriptions`).
    pub transcription_model: String,
    /// Model ids to force-treat as accepting **image** input even when the gateway
    /// catalog doesn't advertise it (SOUL §7/§9) — the admin/global layer of the
    /// per-user `llm_settings.image_input_models` override. A chat inlines an image
    /// attachment for a model in this list (or in the user's own) regardless of what
    /// the catalog reports. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_input_models: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        // Dev defaults target the bundled echo-llmleaf, whose echo provider serves
        // every modality deterministically with no keys (config/llmleaf.dev.toml).
        Self {
            base_url: "http://localhost:8088".to_string(),
            // Base64("catalerum-dev:dev-echo-key") — the HTTP-Basic-shaped bearer
            // the bundled echo-llmleaf expects (config/llmleaf.dev.toml [[keys]]).
            api_key: "Y2F0YWxlcnVtLWRldjpkZXYtZWNoby1rZXk=".into(),
            control_token: Secret::default(),
            control_plane_enabled: false,
            default_model: "echo".to_string(),
            embedding_model: "echo".to_string(),
            embedding_dimensions: None,
            speech_model: "echo".to_string(),
            speech_voice: "alloy".to_string(),
            transcription_model: "echo".to_string(),
            image_input_models: Vec::new(),
        }
    }
}

/// OCR engines (SOUL §7/§10): image/PDF objects → searchable text. Engines are
/// chained **mistral → vision → tesseract**; each document is served by the
/// first configured engine that supports its type and succeeds. All engines off
/// (the default) preserves the pre-OCR behavior exactly: binary objects
/// catalogue no text. Ingest always uses this config-level chain (jobs carry no
/// acting user); a per-user `llm_settings.ocr_model` additionally routes
/// user-invoked OCR (the `ocr_document` tool, `POST /ocr`) through the vision
/// engine with that model.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OcrConfig {
    /// Vision chat model for OCR via llmleaf (a chat model advertising `image`
    /// input, e.g. a pixtral/gpt-4o-class model). Empty = the vision engine is
    /// off. Deliberately **not** defaulted to `echo` — the dev echo model would
    /// catalogue prompt echoes as document text for every uploaded image.
    pub vision_model: String,
    /// Byte cap for OCR-ing a raster image. Oversized documents are **skipped,
    /// never truncated** — truncated image bytes are corrupt, unlike text.
    /// The base64 data URI expands this ×4/3 on the wire.
    pub max_image_bytes: usize,
    /// Byte cap for OCR-ing a PDF (the Mistral-dialect engine only).
    pub max_document_bytes: usize,
    /// The dedicated Mistral-dialect `/v1/ocr` API (cloud or compatible
    /// self-hosted). Enabled iff its `api_key` is set.
    pub mistral: MistralOcrConfig,
    /// The offline `tesseract`-CLI fallback.
    pub tesseract: TesseractOcrConfig,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            vision_model: String::new(),
            max_image_bytes: 8 * 1024 * 1024,
            max_document_bytes: 32 * 1024 * 1024,
            mistral: MistralOcrConfig::default(),
            tesseract: TesseractOcrConfig::default(),
        }
    }
}

/// Preview rendering (SOUL §9/§10): a stored object → a raster image preview
/// (the first page of a PDF/office document, a rendered spreadsheet/
/// presentation, or a resized image thumbnail). Rendering runs in the
/// **standalone preview service** (`catalerum-preview-service`, its own slim
/// container image with LibreOffice + poppler); the distroless API just POSTs
/// the document to it over HTTP. With no `service_url` set, previews are off and
/// the routes report "not configured".
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PreviewConfig {
    /// Master switch. `false` disables the preview routes entirely.
    pub enabled: bool,
    /// Base URL of the preview render service, e.g. `http://catalerum-preview`
    /// (in-cluster Service) or `http://localhost:8790` (dev). Empty → previews
    /// are disabled (the API carries no render toolchain of its own).
    pub service_url: String,
    /// Optional bearer token the service requires (matches its `PREVIEW_TOKEN`).
    /// Empty → no `Authorization` header is sent. Set via env, not committed.
    pub service_token: Secret,
    /// Default output image format when the request names none: `webp` (the
    /// default, smallest), `png`, or `jpeg`.
    pub default_format: String,
    /// Default longest-side pixel bound when the request names no size.
    pub max_dimension: u32,
    /// Hard ceiling on the requested longest-side bound — a request asking for
    /// more is clamped to this (bounds render cost + response size).
    pub hard_max_dimension: u32,
    /// HTTP request timeout to the preview service (seconds). `0` → a built-in
    /// default generous enough for a cold LibreOffice start plus conversion.
    pub timeout_secs: u64,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            service_url: String::new(),
            service_token: Secret::default(),
            default_format: "webp".to_string(),
            max_dimension: catalerum_core::preview::DEFAULT_MAX_DIMENSION,
            hard_max_dimension: 1600,
            timeout_secs: 0,
        }
    }
}

/// Mistral-dialect OCR API settings. The API key is a secret — supply it via
/// `CATALERUM_OCR__MISTRAL__API_KEY` rather than committing it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MistralOcrConfig {
    /// Base URL; empty uses the Mistral cloud (`https://api.mistral.ai`). Any
    /// endpoint speaking the same `/v1/ocr` dialect plugs in here.
    pub base_url: String,
    /// API key; the engine is enabled iff non-empty.
    pub api_key: Secret,
    /// OCR model id; empty uses `mistral-ocr-latest`.
    pub model: String,
}

impl MistralOcrConfig {
    /// True when a key is configured (the engine is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Offline `tesseract` fallback settings. A **runtime** binary, not a build
/// dep: enabled-by-default but probed at startup — no binary (or a missing
/// language pack) just means the engine stays out of the chain.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TesseractOcrConfig {
    /// Probe for (and chain) the binary at startup.
    pub enabled: bool,
    /// Binary path or name on `$PATH`.
    pub path: String,
    /// `-l` language pack(s), `+`-joined (tesseract codes, e.g. `deu+eng`).
    pub languages: String,
}

impl Default for TesseractOcrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "tesseract".to_string(),
            languages: "eng".to_string(),
        }
    }
}

/// Auth toggles (SOUL §18). `dev_login` seeds the admin + prints a magic link.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct AuthConfig {
    pub dev_login: bool,
    /// Enable instance-local Argon2id credentials and the first-boot setup API.
    /// Off by default so existing distributed/SSO deployments are unchanged.
    pub password_login: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            dev_login: true,
            password_login: false,
        }
    }
}

/// Single sign-on — the **OIDC first cut** (SOUL §18/§16 M7/§29). SOUL §29's open
/// question (OIDC vs SAML to ship first) is resolved here: **OIDC** ships;
/// **SAML is deferred**. This section configures the Authorization Code + PKCE
/// flow. When [`issuer`](SsoConfig::issuer) + [`client_id`](SsoConfig::client_id)
/// are unset the whole feature is **off** and the `/auth/sso/*` routes return
/// `404` — dev magic-link login is unaffected.
///
/// Secrets (`client_secret`, `state_secret`) use [`Secret`] — set them via env
/// (`CATALERUM_SSO__CLIENT_SECRET=…`), never the committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SsoConfig {
    /// The OIDC **issuer** URL (e.g. `https://accounts.example.com`). Discovery reads
    /// `{issuer}/.well-known/openid-configuration`; every `id_token`'s `iss` must
    /// equal this. Empty ⇒ SSO disabled.
    pub issuer: String,
    /// The OAuth **client id** registered with the IdP (also the expected `aud`).
    /// Empty ⇒ SSO disabled.
    pub client_id: String,
    /// The confidential-client **secret**. Sent at the token endpoint via
    /// `client_secret_post` (or `client_secret_basic` when
    /// [`token_auth_basic`](SsoConfig::token_auth_basic)).
    pub client_secret: Secret,
    /// The exact **redirect URL** registered with the IdP. Empty ⇒ derived as
    /// `{[server].base_url}/auth/sso/callback`.
    pub redirect_url: String,
    /// The **browser-facing** origin of this API's `/auth/sso/*` routes (e.g.
    /// `https://catalerum-api.example.com`). The SPA's login button navigates
    /// here to start the OIDC dance — set it when the API is not reachable at
    /// the SPA's derived `api.<spa-host>` origin (e.g. a Kubernetes ingress on
    /// a different domain). Empty ⇒ `[server].base_url`, else the SPA derives
    /// the origin itself.
    pub public_url: String,
    /// Space-separated **scopes**. Empty ⇒ `openid email profile`.
    pub scopes: String,
    /// JIT-provisioning policy: `"disabled"` (default, deny-by-default) or
    /// `"enabled"`. When disabled, an SSO login that matches no existing user is
    /// refused with a friendly 403 — an admin must invite the user first.
    pub jit_provisioning: String,
    /// The organisation **slug** a JIT-provisioned user joins. Empty ⇒ the default
    /// organisation (`default`). Without [`jit_workspace`](Self::jit_workspace) JIT
    /// users get **org** membership only — no workspace until an admin adds them
    /// (fail-closed).
    pub jit_organisation: String,
    /// The org **role** a JIT-provisioned user receives (`owner`|`admin`|`member`).
    /// Empty ⇒ `member`.
    pub jit_org_role: String,
    /// The workspace **slug** an SSO user who belongs to no live workspace is
    /// auto-joined to at login (requires JIT provisioning enabled). This is the
    /// "SSO logins just work" knob: it covers both freshly JIT-provisioned users
    /// and previously provisioned ones an admin never placed. Empty ⇒ off (the
    /// fail-closed default): such logins are refused with the no-workspace error.
    pub jit_workspace: String,
    /// The workspace **role** granted by the auto-join
    /// (`owner`|`admin`|`member`|`viewer`). Empty ⇒ `member`.
    pub jit_workspace_role: String,
    /// Trust the IdP's `email` claim for first-login account linking / JIT even when
    /// `email_verified` is absent/false. **Off by default** — only a verified email
    /// links an SSO identity to an existing local account.
    pub trust_email: bool,
    /// Authenticate at the token endpoint with HTTP Basic (`client_secret_basic`)
    /// instead of the request body (`client_secret_post`, the default).
    pub token_auth_basic: bool,
    /// Secret that signs the short-lived **state cookie** (carrying `state`/`nonce`/
    /// PKCE verifier). Set it (any string) so an in-flight login survives a restart /
    /// spans pods; unset ⇒ a fresh random per-process key (in-flight logins then die
    /// with the process — fine for single-pod dev). Independent of the download /
    /// trigger link secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
    /// Clock-skew leeway (seconds) tolerated on the `id_token` `exp`/`iat`. `0` ⇒ 60s.
    pub leeway_secs: u64,
}

impl SsoConfig {
    /// Whether SSO is configured (issuer + client id set) — else the routes 404.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.issuer.trim().is_empty() && !self.client_id.trim().is_empty()
    }

    /// Whether JIT provisioning is turned on (deny-by-default: only the literal
    /// `"enabled"` opts in).
    #[must_use]
    pub fn jit_enabled(&self) -> bool {
        self.jit_provisioning.trim().eq_ignore_ascii_case("enabled")
    }

    /// The effective scope string (`openid email profile` when unset).
    #[must_use]
    pub fn effective_scopes(&self) -> String {
        let s = self.scopes.trim();
        if s.is_empty() {
            "openid email profile".to_string()
        } else {
            s.to_string()
        }
    }

    /// The advertised full browser-facing `GET /auth/sso/login` URL, when config
    /// pins one: built from the explicit [`public_url`](Self::public_url), else
    /// the server's **explicit** `base_url` (never its `http://<listen>` fallback
    /// — that origin is meaningless to a browser). `None` when neither is set;
    /// the SPA then derives the API origin from its own host (`api.<host>`).
    #[must_use]
    pub fn public_login_url(&self, server_base_url: Option<&str>) -> Option<String> {
        let origin = Some(self.public_url.trim())
            .filter(|s| !s.is_empty())
            .or_else(|| server_base_url.map(str::trim).filter(|s| !s.is_empty()))?;
        Some(format!("{}/auth/sso/login", origin.trim_end_matches('/')))
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/sso/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!("{}/auth/sso/callback", api_base_url.trim_end_matches('/'))
        } else {
            explicit.to_string()
        }
    }

    /// The org slug JIT users join (`default` when unset).
    #[must_use]
    pub fn jit_org_slug(&self) -> &str {
        let s = self.jit_organisation.trim();
        if s.is_empty() {
            "default"
        } else {
            s
        }
    }

    /// The org role token JIT users receive (`member` when unset).
    #[must_use]
    pub fn jit_org_role_token(&self) -> &str {
        let s = self.jit_org_role.trim();
        if s.is_empty() {
            "member"
        } else {
            s
        }
    }

    /// The workspace slug SSO users without a live workspace are auto-joined to,
    /// or `None` when the knob is unset (the fail-closed default).
    #[must_use]
    pub fn jit_workspace_slug(&self) -> Option<&str> {
        let s = self.jit_workspace.trim();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// The workspace role token the auto-join grants (`member` when unset).
    #[must_use]
    pub fn jit_workspace_role_token(&self) -> &str {
        let s = self.jit_workspace_role.trim();
        if s.is_empty() {
            "member"
        } else {
            s
        }
    }

    /// The effective `id_token` clock-skew leeway in seconds (60 when unset/0).
    #[must_use]
    pub fn leeway(&self) -> u64 {
        if self.leeway_secs == 0 {
            60
        } else {
            self.leeway_secs
        }
    }
}

/// Google OAuth (SOUL §16 M7) — one confidential-client app used to connect a
/// user's Google Calendar via the `/auth/google/*` web flow. The client id +
/// secret are shared across all Google connections in the deployment; each
/// connection's access/refresh tokens are stored **encrypted** per-connection
/// (SOUL §13), not here.
///
/// Empty [`client_id`](GoogleConfig::client_id) ⇒ the whole feature is **off** and
/// the `/auth/google/*` routes return `404`. Secrets ([`client_secret`],
/// [`state_secret`]) use [`Secret`] — set them via env
/// (`CATALERUM_GOOGLE__CLIENT_SECRET=…`), never the committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GoogleConfig {
    /// The OAuth **client id** of the catalerum Google app. Empty ⇒ feature off.
    pub client_id: String,
    /// The confidential-client **secret**, sent at the token endpoint.
    pub client_secret: Secret,
    /// The exact **redirect URL** registered with Google. Empty ⇒ derived as
    /// `{[server].base_url}/auth/google/callback`.
    pub redirect_url: String,
    /// Secret that signs the short-lived Google-OAuth **state cookie** (carrying
    /// the CSRF `state` + the workspace/connection the callback attaches tokens
    /// to). Independent of `[sso].state_secret` and the download/trigger link
    /// secrets, so it rotates separately; unset ⇒ a fresh random per-process key
    /// (in-flight connects then die with the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
    /// Opt-in **push notifications** (SOUL §8/§16 M7 push half): when `true`, the
    /// ingest side registers a Google `events.watch` channel per Google-calendar
    /// connection that has a `CollectCalendar` automation, so a calendar change
    /// triggers a collect promptly instead of waiting for the poll cadence. Off by
    /// default because watching requires a **publicly reachable** `[server].base_url`
    /// (Google POSTs to `{base_url}/webhooks/google/calendar`); with no automation on
    /// a connection nothing is watched (the dormant-connection model). The poll
    /// cadence still runs as the correctness fallback — push is a latency optimization.
    #[serde(default)]
    pub push: bool,
    /// Secret that signs the per-channel **push token** (the `X-Goog-Channel-Token`
    /// Google echoes on every notification — the channel's authorization, SOUL §11
    /// house style). Independent of [`state_secret`](Self::state_secret) so it
    /// rotates separately; unset ⇒ a fresh random per-process key (channels created
    /// before a restart then stop verifying — their notifications 404 and fall back
    /// to the poll until the scan renews them; set it for restart/multi-pod
    /// durability, exactly like `[server].trigger_secret`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_secret: Option<Secret>,
}

impl GoogleConfig {
    /// Whether Google OAuth is configured (client id + secret set) — else the
    /// `/auth/google/*` routes `404`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.is_empty()
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/google/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!(
                "{}/auth/google/callback",
                api_base_url.trim_end_matches('/')
            )
        } else {
            explicit.to_string()
        }
    }
}

/// Microsoft OAuth (SOUL §8) — one confidential-client Entra app used to connect
/// a user's Outlook / Microsoft 365 calendar via the `/auth/microsoft/*` web
/// flow, [`GoogleConfig`]'s exact twin. The client id + secret are shared across
/// all Microsoft connections in the deployment; each connection's access/refresh
/// tokens are stored **encrypted** per-connection (SOUL §13), not here.
///
/// Empty [`client_id`](MicrosoftConfig::client_id) ⇒ the feature is **off** and
/// the `/auth/microsoft/*` routes return `404`. Secrets use [`Secret`] — set
/// them via env (`CATALERUM_MICROSOFT__CLIENT_SECRET=…`), never committed TOML.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MicrosoftConfig {
    /// The OAuth **client (application) id** of the catalerum Entra app. Empty ⇒
    /// feature off.
    pub client_id: String,
    /// The confidential-client **secret**, sent at the token endpoint.
    pub client_secret: Secret,
    /// The Entra **tenant** the app authenticates against: a tenant id/domain
    /// for single-tenant apps, empty ⇒ the multi-tenant `common` endpoint
    /// (work/school + personal accounts).
    pub tenant: String,
    /// The exact **redirect URL** registered with the Entra app. Empty ⇒ derived
    /// as `{[server].base_url}/auth/microsoft/callback`.
    pub redirect_url: String,
    /// Secret that signs the short-lived Microsoft-OAuth **state cookie**
    /// (independent of `[google].state_secret` and `[sso].state_secret`, so it
    /// rotates separately); unset ⇒ a fresh random per-process key (in-flight
    /// connects then die with the process — fine for single-pod dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_secret: Option<Secret>,
}

impl MicrosoftConfig {
    /// Whether Microsoft OAuth is configured (client id + secret set) — else the
    /// `/auth/microsoft/*` routes `404`.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.client_id.trim().is_empty() && !self.client_secret.is_empty()
    }

    /// The effective redirect URL: the explicit value, else
    /// `{api_base_url}/auth/microsoft/callback`.
    #[must_use]
    pub fn effective_redirect_url(&self, api_base_url: &str) -> String {
        let explicit = self.redirect_url.trim();
        if explicit.is_empty() {
            format!(
                "{}/auth/microsoft/callback",
                api_base_url.trim_end_matches('/')
            )
        } else {
            explicit.to_string()
        }
    }
}

/// Web fetching & browsing (SOUL §27). Selects the default backend and carries
/// the egress safety policy plus per-backend settings. The plain-HTTP backend is
/// always available (local-first); Firecrawl and the browser/CDP backend activate
/// when their settings are present.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FetchConfig {
    /// Backend an `auto` fetch resolves to: `http`, `browser`, or `firecrawl`.
    pub backend: String,
    /// Override the User-Agent sent by the HTTP backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Default per-request timeout, in seconds.
    pub timeout_secs: u64,
    /// Cap on response-body bytes read (guards against huge pages).
    pub max_bytes: u64,
    /// Allow reaching private/loopback addresses. A protected scope (SOUL §19):
    /// off by default to block SSRF; enable only for trusted internal targets.
    pub allow_private_hosts: bool,
    /// Firecrawl backend settings.
    pub firecrawl: FirecrawlConfig,
    /// Browser/CDP backend settings.
    pub browser: BrowserConfig,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            backend: "http".to_string(),
            user_agent: None,
            timeout_secs: 30,
            max_bytes: 5 * 1024 * 1024,
            allow_private_hosts: false,
            firecrawl: FirecrawlConfig::default(),
            browser: BrowserConfig::default(),
        }
    }
}

/// Firecrawl scrape API settings (self-hosted or cloud). The API key is a secret
/// — supply it via `CATALERUM_FETCH__FIRECRAWL__API_KEY` rather than committing it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct FirecrawlConfig {
    /// Base URL; empty uses the Firecrawl cloud (`https://api.firecrawl.dev`).
    pub base_url: String,
    /// API key (or any token a self-hosted deployment accepts).
    pub api_key: Secret,
}

impl FirecrawlConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Browser/CDP backend settings. `cdp_url` is the Chrome DevTools Protocol
/// WebSocket of an external browser (headless Chrome `--remote-debugging-port`,
/// a Playwright server, or Browserless). Requires the `browser` build feature.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// CDP WebSocket endpoint, e.g. `ws://localhost:9222/devtools/browser/<id>`.
    pub cdp_url: String,
}

impl BrowserConfig {
    /// True when an endpoint is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.cdp_url.trim().is_empty()
    }
}

/// Web search backends (SOUL §27). Selects the default provider a no-`provider`
/// search resolves to, and carries each provider's credentials. A provider
/// activates only when its credential is set (for SearXNG, its `base_url`).
///
/// These are **billed infrastructure secrets shared by the workspace**, not
/// per-user personalization — they live here (and in env), redacted by [`Secret`],
/// exactly like the Firecrawl key, and are never exposed through the web UI.
/// Supply each via `CATALERUM_SEARCH__<PROVIDER>__API_KEY` rather than committing it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Provider a search with no explicit `provider` resolves to (e.g. `brave`).
    pub backend: String,
    /// Brave Search.
    pub brave: BraveConfig,
    /// Tavily (LLM/RAG-optimized).
    pub tavily: TavilyConfig,
    /// Exa (neural + keyword).
    pub exa: ExaConfig,
    /// SearXNG (self-hosted; no key).
    pub searxng: SearxngConfig,
    /// Google Programmable Search Engine (CSE).
    pub google: GoogleCseConfig,
    /// SerpAPI (Google/Bing SERP scrape).
    pub serpapi: SerpApiConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            backend: "brave".to_string(),
            brave: BraveConfig::default(),
            tavily: TavilyConfig::default(),
            exa: ExaConfig::default(),
            searxng: SearxngConfig::default(),
            google: GoogleCseConfig::default(),
            serpapi: SerpApiConfig::default(),
        }
    }
}

impl SearchConfig {
    /// Each provider id paired with whether it is configured (usable). The single
    /// source of truth the settings UI iterates to show which engines are live;
    /// order matches `catalerum_search::PROVIDER_IDS`.
    #[must_use]
    pub fn provider_status(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("brave", self.brave.is_enabled()),
            ("tavily", self.tavily.is_enabled()),
            ("exa", self.exa.is_enabled()),
            ("searxng", self.searxng.is_enabled()),
            ("google", self.google.is_enabled()),
            ("serpapi", self.serpapi.is_enabled()),
        ]
    }

    /// True if at least one provider is configured (so a searcher can be built).
    #[must_use]
    pub fn any_enabled(&self) -> bool {
        self.provider_status().iter().any(|(_, on)| *on)
    }
}

/// Brave Search settings. Supply the subscription token via
/// `CATALERUM_SEARCH__BRAVE__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BraveConfig {
    /// Brave subscription token.
    pub api_key: Secret,
}

impl BraveConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Tavily settings. Supply the key (`tvly-…`) via `CATALERUM_SEARCH__TAVILY__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TavilyConfig {
    /// Tavily API key.
    pub api_key: Secret,
}

impl TavilyConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Exa settings. Supply the key via `CATALERUM_SEARCH__EXA__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExaConfig {
    /// Exa API key.
    pub api_key: Secret,
}

impl ExaConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// SearXNG settings — a self-hosted metasearch instance; no key, just the base
/// URL of a deployment that has the JSON output format enabled.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SearxngConfig {
    /// Base URL of the SearXNG instance, e.g. `https://searx.example.org`.
    pub base_url: String,
}

impl SearxngConfig {
    /// True when a base URL is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
}

/// Google Programmable Search Engine settings — needs **both** an API `key` and a
/// search engine id (`cx`). Supply the key via `CATALERUM_SEARCH__GOOGLE__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GoogleCseConfig {
    /// Google API key.
    pub api_key: Secret,
    /// Programmable Search Engine id (`cx`).
    pub cx: String,
}

impl GoogleCseConfig {
    /// True when both the key and the engine id are configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty() && !self.cx.trim().is_empty()
    }
}

/// SerpAPI settings. Supply the key via `CATALERUM_SEARCH__SERPAPI__API_KEY`; the
/// `engine` selects the SERP source (default `google`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SerpApiConfig {
    /// SerpAPI key.
    pub api_key: Secret,
    /// SERP engine, e.g. `google`, `bing`, `duckduckgo`.
    pub engine: String,
}

impl Default for SerpApiConfig {
    fn default() -> Self {
        Self {
            api_key: Secret::default(),
            engine: "google".to_string(),
        }
    }
}

impl SerpApiConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

impl Config {
    /// Apply `CATALERUM_`-prefixed environment overrides over this config. The
    /// section delimiter is `__`; only the fields the API needs are wired:
    ///
    /// - `CATALERUM_SERVER__LISTEN`, `CATALERUM_SERVER__POD_IP`,
    ///   `CATALERUM_SERVER__BASE_URL`, `CATALERUM_SERVER__WEB_URL`
    /// - `CATALERUM_SERVER__DOWNLOAD_SECRET`, `CATALERUM_SERVER__TRIGGER_SECRET`
    ///   (shared link-signing keys — set them so links verify across pods, SOUL §16 M7)
    /// - `CATALERUM_DATABASE__URL`
    /// - `CATALERUM_VALKEY__URL`, `CATALERUM_VALKEY__ENABLED`
    /// - `CATALERUM_LLM__BASE_URL`, `CATALERUM_LLM__API_KEY`, `CATALERUM_LLM__CONTROL_TOKEN`,
    ///   `CATALERUM_LLM__CONTROL_PLANE_ENABLED`, `CATALERUM_LLM__DEFAULT_MODEL`,
    ///   `CATALERUM_LLM__EMBEDDING_MODEL`, `CATALERUM_LLM__EMBEDDING_DIMENSIONS`,
    ///   `CATALERUM_LLM__SPEECH_MODEL`, `CATALERUM_LLM__SPEECH_VOICE`,
    ///   `CATALERUM_LLM__TRANSCRIPTION_MODEL`
    /// - `CATALERUM_TELEMETRY__SERVICE_NAME`, `CATALERUM_TELEMETRY__SAMPLE_RATIO`,
    ///   `CATALERUM_TELEMETRY__OTLP__ENABLED`, `CATALERUM_TELEMETRY__OTLP__ENDPOINT`,
    ///   `CATALERUM_TELEMETRY__OTLP__CONTENT`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__ENABLED`, `CATALERUM_TELEMETRY__LANGFUSE__ENDPOINT`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__PUBLIC_KEY`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__SECRET_KEY`,
    ///   `CATALERUM_TELEMETRY__LANGFUSE__CONTENT`
    /// - `CATALERUM_OCR__VISION_MODEL`, `CATALERUM_OCR__MISTRAL__BASE_URL`,
    ///   `CATALERUM_OCR__MISTRAL__API_KEY`, `CATALERUM_OCR__MISTRAL__MODEL`,
    ///   `CATALERUM_OCR__TESSERACT__ENABLED`, `CATALERUM_OCR__TESSERACT__PATH`,
    ///   `CATALERUM_OCR__TESSERACT__LANGUAGES`
    /// - `CATALERUM_AUTH__DEV_LOGIN`
    /// - `CATALERUM_SSO__ISSUER`, `CATALERUM_SSO__CLIENT_ID`, `CATALERUM_SSO__CLIENT_SECRET`,
    ///   `CATALERUM_SSO__REDIRECT_URL`, `CATALERUM_SSO__SCOPES`, `CATALERUM_SSO__STATE_SECRET`,
    ///   `CATALERUM_SSO__JIT_PROVISIONING`, `CATALERUM_SSO__JIT_ORGANISATION`,
    ///   `CATALERUM_SSO__JIT_ORG_ROLE`, `CATALERUM_SSO__TRUST_EMAIL`,
    ///   `CATALERUM_SSO__TOKEN_AUTH_BASIC`, `CATALERUM_SSO__LEEWAY_SECS`
    /// - `CATALERUM_FETCH__BACKEND`, `CATALERUM_FETCH__USER_AGENT`,
    ///   `CATALERUM_FETCH__TIMEOUT_SECS`, `CATALERUM_FETCH__MAX_BYTES`,
    ///   `CATALERUM_FETCH__ALLOW_PRIVATE_HOSTS`,
    ///   `CATALERUM_FETCH__FIRECRAWL__BASE_URL`, `CATALERUM_FETCH__FIRECRAWL__API_KEY`,
    ///   `CATALERUM_FETCH__BROWSER__CDP_URL`
    /// - `CATALERUM_SEARCH__BACKEND`, `CATALERUM_SEARCH__BRAVE__API_KEY`,
    ///   `CATALERUM_SEARCH__TAVILY__API_KEY`, `CATALERUM_SEARCH__EXA__API_KEY`,
    ///   `CATALERUM_SEARCH__SEARXNG__BASE_URL`, `CATALERUM_SEARCH__GOOGLE__API_KEY`,
    ///   `CATALERUM_SEARCH__GOOGLE__CX`, `CATALERUM_SEARCH__SERPAPI__API_KEY`,
    ///   `CATALERUM_SEARCH__SERPAPI__ENGINE`
    /// - `CATALERUM_QDRANT__URL`, `CATALERUM_QDRANT__ENABLED`
    /// - `CATALERUM_NEO4J__URL`, `CATALERUM_NEO4J__USER`, `CATALERUM_NEO4J__PASSWORD`,
    ///   `CATALERUM_NEO4J__DATABASE`, `CATALERUM_NEO4J__ENABLED`
    /// - `CATALERUM_CURATION__ENABLED`, `CATALERUM_CURATION__MODEL`
    /// - `CATALERUM_EXEC__ENABLED`, `CATALERUM_EXEC__ALLOW` (comma-separated)
    /// - `CATALERUM_CHANNELS__DISCORD_WEBHOOK_URL` (the `default` Discord channel)
    /// - `CATALERUM_CHANNELS__TELEGRAM_BOT_TOKEN`, `CATALERUM_CHANNELS__TELEGRAM_CHAT_ID`,
    ///   `CATALERUM_CHANNELS__TELEGRAM_BASE_URL` (the `default` Telegram channel)
    /// - `CATALERUM_STORAGE__LOCAL_PATH`, `CATALERUM_STORAGE__BUCKET`,
    ///   `CATALERUM_STORAGE__MAX_OBJECT_BYTES`
    /// - `CATALERUM_STORAGE__S3__ENDPOINT`, `CATALERUM_STORAGE__S3__REGION`,
    ///   `CATALERUM_STORAGE__S3__ACCESS_KEY`, `CATALERUM_STORAGE__S3__SECRET_KEY`,
    ///   `CATALERUM_STORAGE__S3__PATH_STYLE`
    #[must_use]
    pub fn with_env_overrides(mut self) -> Self {
        use std::env::var;
        if let Ok(v) = var("CATALERUM_SERVER__LISTEN") {
            self.server.listen = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__POD_IP") {
            self.server.pod_ip = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__BASE_URL") {
            self.server.base_url = Some(v);
        }
        if let Ok(v) = var("CATALERUM_SERVER__WEB_URL") {
            self.server.web_url = v;
        }
        if let Ok(v) = var("CATALERUM_SERVER__CORS_EXTRA_ORIGINS") {
            // Comma-separated origin list (env has no TOML arrays).
            self.server.cors_extra_origins = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        // The link-signing secrets (SOUL §9/§11): under multiple pods every pod must
        // sign with the SAME key or a link minted on one pod fails to verify on
        // another (each else falls back to a fresh per-process random key). Sourced
        // from the env (a k8s Secret) like `[secrets].master_key`, never the shared
        // committed TOML.
        if let Ok(v) = var("CATALERUM_SERVER__DOWNLOAD_SECRET") {
            self.server.download_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_SERVER__TRIGGER_SECRET") {
            self.server.trigger_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_DATABASE__URL") {
            self.database.url = v;
        }
        if let Ok(v) = var("CATALERUM_VALKEY__URL") {
            self.valkey.url = v;
        }
        if let Ok(v) = var("CATALERUM_VALKEY__ENABLED") {
            if let Ok(b) = v.parse() {
                self.valkey.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_LLM__BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__API_KEY") {
            self.llm.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_LLM__CONTROL_TOKEN") {
            self.llm.control_token = v.into();
        }
        if let Ok(v) = var("CATALERUM_LLM__CONTROL_PLANE_ENABLED") {
            if let Ok(b) = v.parse() {
                self.llm.control_plane_enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_LLM__DEFAULT_MODEL") {
            self.llm.default_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__EMBEDDING_MODEL") {
            self.llm.embedding_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__EMBEDDING_DIMENSIONS") {
            // An explicit 0 (or an unparseable value) means "provider default".
            self.llm.embedding_dimensions = v.parse().ok().filter(|&n| n != 0);
        }
        if let Ok(v) = var("CATALERUM_LLM__SPEECH_MODEL") {
            self.llm.speech_model = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__SPEECH_VOICE") {
            self.llm.speech_voice = v;
        }
        if let Ok(v) = var("CATALERUM_LLM__TRANSCRIPTION_MODEL") {
            self.llm.transcription_model = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__SERVICE_NAME") {
            self.telemetry.service_name = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__SAMPLE_RATIO") {
            if let Ok(n) = v.parse() {
                self.telemetry.sample_ratio = n;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__ENABLED") {
            if let Ok(b) = v.parse() {
                self.telemetry.otlp.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__ENDPOINT") {
            self.telemetry.otlp.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__OTLP__CONTENT") {
            if let Ok(content) = v.parse() {
                self.telemetry.otlp.content = content;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__ENABLED") {
            if let Ok(b) = v.parse() {
                self.telemetry.langfuse.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__ENDPOINT") {
            self.telemetry.langfuse.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__PUBLIC_KEY") {
            self.telemetry.langfuse.public_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__SECRET_KEY") {
            self.telemetry.langfuse.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_TELEMETRY__LANGFUSE__CONTENT") {
            if let Ok(content) = v.parse() {
                self.telemetry.langfuse.content = content;
            }
        }
        if let Ok(v) = var("CATALERUM_OCR__VISION_MODEL") {
            self.ocr.vision_model = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__BASE_URL") {
            self.ocr.mistral.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__API_KEY") {
            self.ocr.mistral.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_OCR__MISTRAL__MODEL") {
            self.ocr.mistral.model = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__ENABLED") {
            if let Ok(b) = v.parse() {
                self.ocr.tesseract.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__PATH") {
            self.ocr.tesseract.path = v;
        }
        if let Ok(v) = var("CATALERUM_OCR__TESSERACT__LANGUAGES") {
            self.ocr.tesseract.languages = v;
        }
        if let Ok(v) = var("CATALERUM_AUTH__DEV_LOGIN") {
            if let Ok(b) = v.parse() {
                self.auth.dev_login = b;
            }
        }
        if let Ok(v) = var("CATALERUM_AUTH__PASSWORD_LOGIN") {
            if let Ok(b) = v.parse() {
                self.auth.password_login = b;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__BACKEND") {
            self.fetch.backend = v;
        }
        if let Ok(v) = var("CATALERUM_FETCH__USER_AGENT") {
            self.fetch.user_agent = Some(v);
        }
        if let Ok(v) = var("CATALERUM_FETCH__TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.fetch.timeout_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__MAX_BYTES") {
            if let Ok(n) = v.parse() {
                self.fetch.max_bytes = n;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__ALLOW_PRIVATE_HOSTS") {
            if let Ok(b) = v.parse() {
                self.fetch.allow_private_hosts = b;
            }
        }
        if let Ok(v) = var("CATALERUM_FETCH__FIRECRAWL__BASE_URL") {
            self.fetch.firecrawl.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_FETCH__FIRECRAWL__API_KEY") {
            self.fetch.firecrawl.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_FETCH__BROWSER__CDP_URL") {
            self.fetch.browser.cdp_url = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__BACKEND") {
            self.search.backend = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__BRAVE__API_KEY") {
            self.search.brave.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__TAVILY__API_KEY") {
            self.search.tavily.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__EXA__API_KEY") {
            self.search.exa.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SEARXNG__BASE_URL") {
            self.search.searxng.base_url = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__GOOGLE__API_KEY") {
            self.search.google.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__GOOGLE__CX") {
            self.search.google.cx = v;
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SERPAPI__API_KEY") {
            self.search.serpapi.api_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_SEARCH__SERPAPI__ENGINE") {
            self.search.serpapi.engine = v;
        }
        if let Ok(v) = var("CATALERUM_QDRANT__URL") {
            self.qdrant.url = v;
        }
        if let Ok(v) = var("CATALERUM_QDRANT__ENABLED") {
            if let Ok(b) = v.parse() {
                self.qdrant.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_NEO4J__URL") {
            self.neo4j.url = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__USER") {
            self.neo4j.user = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__PASSWORD") {
            self.neo4j.password = v.into();
        }
        if let Ok(v) = var("CATALERUM_NEO4J__DATABASE") {
            self.neo4j.database = v;
        }
        if let Ok(v) = var("CATALERUM_NEO4J__ENABLED") {
            if let Ok(b) = v.parse() {
                self.neo4j.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_CURATION__ENABLED") {
            if let Ok(b) = v.parse() {
                self.curation.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_CURATION__MODEL") {
            self.curation.model = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__ENABLED") {
            if let Ok(b) = v.parse() {
                self.exec.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__ALLOW") {
            // Comma-separated program names.
            self.exec.allow = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Ok(v) = var("CATALERUM_EXEC__BACKEND") {
            self.exec.backend = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__SHELL") {
            self.exec.shell = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__EPHEMERAL_ROOT") {
            self.exec.ephemeral_root = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__BINARY") {
            self.exec.podman.binary = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__IMAGE") {
            self.exec.podman.image = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PODMAN__NETWORK") {
            self.exec.podman.network = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__NAMESPACE") {
            self.exec.k8s.namespace = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__IMAGE") {
            self.exec.k8s.image = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__K8S__STORAGE_CLASS") {
            self.exec.k8s.storage_class = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__PER_WORKSPACE") {
            if let Ok(b) = v.parse() {
                self.exec.per_workspace = b;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__IDLE_TIMEOUT_SECS") {
            if let Ok(n) = v.parse() {
                self.exec.idle_timeout_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXEC__VOLUME_SIZE") {
            self.exec.volume_size = v;
        }
        if let Ok(v) = var("CATALERUM_EXEC__SANDBOX_NETWORK") {
            self.exec.sandbox_network = v;
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__DISCORD_WEBHOOK_URL") {
            // The env shortcut configures the `default` Discord channel; named
            // channels are TOML-only (`[channels.discord]`).
            self.channels.discord.insert("default".to_string(), v);
        }
        // Env shortcuts for the `default` Telegram channel; named channels are
        // TOML-only (`[channels.telegram.<name>]`). The entry is created lazily so
        // a partial override (just a base_url, say) still composes with TOML.
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_BOT_TOKEN") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .bot_token = v.into();
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_CHAT_ID") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .chat_id = v;
        }
        if let Ok(v) = var("CATALERUM_CHANNELS__TELEGRAM_BASE_URL") {
            self.channels
                .telegram
                .entry("default".to_string())
                .or_default()
                .base_url = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__LOCAL_PATH") {
            self.storage.local_path = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__BUCKET") {
            self.storage.bucket = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__MAX_OBJECT_BYTES") {
            if let Ok(n) = v.parse() {
                self.storage.max_object_bytes = n;
            }
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__ENDPOINT") {
            self.storage.s3.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__REGION") {
            self.storage.s3.region = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__ACCESS_KEY") {
            self.storage.s3.access_key = v;
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__SECRET_KEY") {
            self.storage.s3.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_STORAGE__S3__PATH_STYLE") {
            if let Ok(b) = v.parse() {
                self.storage.s3.path_style = b;
            }
        }
        // Backup (SOUL §30). The destination mirrors the `[storage]` env shape so
        // production injects its S3/WebDAV credentials via the environment, not the
        // committed TOML.
        if let Ok(v) = var("CATALERUM_BACKUP__ENABLED") {
            if let Ok(b) = v.parse() {
                self.backup.enabled = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__INTERVAL_SECS") {
            if let Ok(n) = v.parse() {
                self.backup.interval_secs = n;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__PREFIX") {
            self.backup.prefix = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__KEEP") {
            if let Ok(n) = v.parse() {
                self.backup.keep = n;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__INCLUDE_OBJECTS") {
            if let Ok(b) = v.parse() {
                self.backup.include_objects = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__LOCAL_PATH") {
            self.backup.destination.local_path = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__BUCKET") {
            self.backup.destination.bucket = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__ENDPOINT") {
            self.backup.destination.s3.endpoint = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__REGION") {
            self.backup.destination.s3.region = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__ACCESS_KEY") {
            self.backup.destination.s3.access_key = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__SECRET_KEY") {
            self.backup.destination.s3.secret_key = v.into();
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__S3__PATH_STYLE") {
            if let Ok(b) = v.parse() {
                self.backup.destination.s3.path_style = b;
            }
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__URL") {
            self.backup.destination.webdav.url = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__USERNAME") {
            self.backup.destination.webdav.username = v;
        }
        if let Ok(v) = var("CATALERUM_BACKUP__DESTINATION__WEBDAV__PASSWORD") {
            self.backup.destination.webdav.password = v.into();
        }
        // Encrypted secret store (SOUL §13) — set the master key via env, never TOML.
        if let Ok(v) = var("CATALERUM_SECRETS__MASTER_KEY") {
            self.secrets.master_key = v.into();
        }
        // SSO / OIDC (SOUL §18/§29) — issuer + client id enable it; secrets via env.
        if let Ok(v) = var("CATALERUM_SSO__ISSUER") {
            self.sso.issuer = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__CLIENT_ID") {
            self.sso.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__CLIENT_SECRET") {
            self.sso.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_SSO__REDIRECT_URL") {
            self.sso.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__SCOPES") {
            self.sso.scopes = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__STATE_SECRET") {
            self.sso.state_secret = Some(v.into());
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_PROVISIONING") {
            self.sso.jit_provisioning = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_ORGANISATION") {
            self.sso.jit_organisation = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_ORG_ROLE") {
            self.sso.jit_org_role = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_WORKSPACE") {
            self.sso.jit_workspace = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__JIT_WORKSPACE_ROLE") {
            self.sso.jit_workspace_role = v;
        }
        if let Ok(v) = var("CATALERUM_SSO__TRUST_EMAIL") {
            if let Ok(b) = v.parse() {
                self.sso.trust_email = b;
            }
        }
        if let Ok(v) = var("CATALERUM_SSO__TOKEN_AUTH_BASIC") {
            if let Ok(b) = v.parse() {
                self.sso.token_auth_basic = b;
            }
        }
        if let Ok(v) = var("CATALERUM_SSO__LEEWAY_SECS") {
            if let Ok(n) = v.parse() {
                self.sso.leeway_secs = n;
            }
        }
        // Google OAuth (SOUL §16 M7) — secrets via env, never the committed TOML.
        if let Ok(v) = var("CATALERUM_GOOGLE__CLIENT_ID") {
            self.google.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__CLIENT_SECRET") {
            self.google.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__REDIRECT_URL") {
            self.google.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_GOOGLE__STATE_SECRET") {
            self.google.state_secret = Some(v.into());
        }
        // Microsoft OAuth (SOUL §8) — secrets via env, never the committed TOML.
        if let Ok(v) = var("CATALERUM_MICROSOFT__CLIENT_ID") {
            self.microsoft.client_id = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__CLIENT_SECRET") {
            self.microsoft.client_secret = v.into();
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__TENANT") {
            self.microsoft.tenant = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__REDIRECT_URL") {
            self.microsoft.redirect_url = v;
        }
        if let Ok(v) = var("CATALERUM_MICROSOFT__STATE_SECRET") {
            self.microsoft.state_secret = Some(v.into());
        }
        // External-Postgres caps (SOUL §11/§19).
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__POOL_MAX_CONNECTIONS") {
            if let Ok(n) = v.parse() {
                self.external_db.pool_max_connections = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__STATEMENT_TIMEOUT_MS") {
            if let Ok(n) = v.parse() {
                self.external_db.statement_timeout_ms = n;
            }
        }
        if let Ok(v) = var("CATALERUM_EXTERNAL_DB__MAX_ROWS") {
            if let Ok(n) = v.parse() {
                self.external_db.max_rows = n;
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_well_formed() {
        let c = Config::default();
        assert_eq!(c.server.listen, "0.0.0.0:8787");
        assert!(c.auth.dev_login);
        assert!(c.valkey.enabled);
    }

    #[test]
    fn only_single_user_mode_caches_the_sole_user() {
        // The mode gate for the §29 sole-user personalization cache: single_user
        // may cache, multi_user never consults it (default is single_user).
        assert!(DeploymentMode::default().caches_sole_user());
        assert!(DeploymentMode::SingleUser.caches_sole_user());
        assert!(!DeploymentMode::MultiUser.caches_sole_user());
    }

    #[test]
    fn google_oauth_disabled_when_absent_and_derives_redirect() {
        // No [google] section → the feature is off (the /auth/google/* routes 404).
        let c = Config::default();
        assert!(!c.google.is_enabled());
        // The redirect URL derives off the API base URL when not set explicitly.
        assert_eq!(
            c.google.effective_redirect_url("https://app.example.com"),
            "https://app.example.com/auth/google/callback"
        );
        // With id + secret set, the feature enables; an explicit redirect wins.
        let g = GoogleConfig {
            client_id: "cid".into(),
            client_secret: Secret::from("sec"),
            redirect_url: "https://x/cb".into(),
            state_secret: None,
            ..Default::default()
        };
        assert!(g.is_enabled());
        assert_eq!(g.effective_redirect_url("https://ignored"), "https://x/cb");
        // Push is opt-in (off by default).
        assert!(!g.push);
    }

    #[test]
    fn sso_disabled_when_absent() {
        // No [sso] section → the whole feature is off (routes 404), and every knob
        // has its safe deny-by-default value.
        let c = Config::default();
        assert!(!c.sso.is_enabled());
        assert!(!c.sso.jit_enabled(), "JIT is deny-by-default");
        assert!(!c.sso.trust_email);
        assert_eq!(c.sso.effective_scopes(), "openid email profile");
        assert_eq!(c.sso.jit_org_slug(), "default");
        assert_eq!(c.sso.jit_org_role_token(), "member");
        assert_eq!(
            c.sso.jit_workspace_slug(),
            None,
            "workspace auto-join is off by default"
        );
        assert_eq!(c.sso.jit_workspace_role_token(), "member");
        assert_eq!(c.sso.leeway(), 60);
    }

    #[test]
    fn sso_parses_and_enables_from_toml() {
        let toml = r#"
            [sso]
            issuer = "https://accounts.example.com"
            client_id = "catalerum"
            client_secret = "shhh"
            scopes = "openid email"
            jit_provisioning = "enabled"
            jit_organisation = "acme"
            jit_org_role = "admin"
            jit_workspace = "default"
            jit_workspace_role = "member"
            trust_email = true
            leeway_secs = 120
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.sso.is_enabled());
        assert!(c.sso.jit_enabled());
        assert!(c.sso.trust_email);
        assert_eq!(c.sso.effective_scopes(), "openid email");
        assert_eq!(c.sso.jit_org_slug(), "acme");
        assert_eq!(c.sso.jit_org_role_token(), "admin");
        assert_eq!(c.sso.jit_workspace_slug(), Some("default"));
        assert_eq!(c.sso.jit_workspace_role_token(), "member");
        assert_eq!(c.sso.leeway(), 120);
        // client_secret is a redacting Secret, exposed only where needed.
        assert_eq!(c.sso.client_secret.expose(), "shhh");
        // Redirect URL derives from the API base when unset.
        assert_eq!(
            c.sso.effective_redirect_url("https://api.example.com/"),
            "https://api.example.com/auth/sso/callback"
        );
    }

    #[test]
    fn sso_needs_both_issuer_and_client_id() {
        let only_issuer: Config = toml::from_str("[sso]\nissuer = \"https://x\"\n").unwrap();
        assert!(!only_issuer.sso.is_enabled());
        let only_client: Config = toml::from_str("[sso]\nclient_id = \"c\"\n").unwrap();
        assert!(!only_client.sso.is_enabled());
    }

    #[test]
    fn sso_public_login_url_prefers_public_url_then_server_base() {
        // Explicit [sso].public_url wins (trailing slash trimmed).
        let c: Config =
            toml::from_str("[sso]\npublic_url = \"https://sso.example.com/\"\n").unwrap();
        assert_eq!(
            c.sso.public_login_url(Some("https://api.internal")),
            Some("https://sso.example.com/auth/sso/login".to_string())
        );
        // Unset → the server's explicit base_url.
        let d = Config::default();
        assert_eq!(
            d.sso.public_login_url(Some("https://api.example.com/")),
            Some("https://api.example.com/auth/sso/login".to_string())
        );
        // Neither set (blank counts as unset) → None; the SPA derives the origin.
        assert_eq!(d.sso.public_login_url(None), None);
        assert_eq!(d.sso.public_login_url(Some("   ")), None);
    }

    #[test]
    fn llm_modality_defaults_target_echo() {
        // Every modality shares the echo endpoint by default (offline dev).
        let c = LlmConfig::default();
        assert_eq!(c.default_model, "echo");
        assert_eq!(c.embedding_model, "echo");
        assert_eq!(c.speech_model, "echo");
        assert_eq!(c.transcription_model, "echo");
        assert_eq!(c.speech_voice, "alloy");
        assert_eq!(c.embedding_dimensions, None);
        assert!(!c.control_plane_enabled);
    }

    #[test]
    fn all_in_one_enables_llm_control_plane() {
        let c: Config = toml::from_str(include_str!("../../../config/all-in-one.toml")).unwrap();
        assert!(c.llm.control_plane_enabled);
    }

    #[test]
    fn llm_modality_models_parse_from_toml() {
        let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "gpt-4o"
            embedding_model = "text-embedding-3-small"
            embedding_dimensions = 256
            speech_model = "tts-1"
            speech_voice = "nova"
            transcription_model = "gpt-4o-transcribe"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.llm.embedding_model, "text-embedding-3-small");
        assert_eq!(c.llm.embedding_dimensions, Some(256));
        assert_eq!(c.llm.speech_model, "tts-1");
        assert_eq!(c.llm.speech_voice, "nova");
        assert_eq!(c.llm.transcription_model, "gpt-4o-transcribe");
    }

    #[test]
    fn llm_partial_toml_keeps_modality_defaults() {
        // A pre-existing config that only sets chat fields still gets working
        // modality defaults (serde(default) per field).
        let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.llm.embedding_model, "echo");
        assert_eq!(c.llm.speech_voice, "alloy");
    }

    #[test]
    fn effective_base_url_prefers_explicit() {
        let mut s = ServerConfig::default();
        assert_eq!(s.effective_base_url(), "http://0.0.0.0:8787");
        s.base_url = Some("https://app.example/".to_string());
        assert_eq!(s.effective_base_url(), "https://app.example");
    }

    #[test]
    fn effective_web_url_defaults_and_trims() {
        let mut s = ServerConfig::default();
        assert_eq!(s.effective_web_url(), "http://localhost:8080");
        s.web_url = "https://app.example/".to_string();
        assert_eq!(s.effective_web_url(), "https://app.example");
    }

    #[test]
    fn fetch_defaults_are_safe() {
        let f = FetchConfig::default();
        assert_eq!(f.backend, "http");
        assert!(!f.allow_private_hosts, "SSRF guard must default closed");
        assert_eq!(f.timeout_secs, 30);
        assert!(!f.firecrawl.is_enabled());
        assert!(!f.browser.is_enabled());
    }

    #[test]
    fn fetch_section_parses_from_toml() {
        let toml = r#"
            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"

            [fetch]
            backend = "firecrawl"
            timeout_secs = 45
            allow_private_hosts = true

            [fetch.firecrawl]
            base_url = "http://firecrawl.internal:3002"
            api_key = "fc-secret"

            [fetch.browser]
            cdp_url = "ws://localhost:9222/devtools/browser/abc"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.fetch.backend, "firecrawl");
        assert_eq!(c.fetch.timeout_secs, 45);
        assert!(c.fetch.allow_private_hosts);
        assert!(c.fetch.firecrawl.is_enabled());
        assert_eq!(c.fetch.firecrawl.base_url, "http://firecrawl.internal:3002");
        assert!(c.fetch.browser.is_enabled());
    }

    #[test]
    fn fetch_partial_toml_keeps_defaults() {
        // A config with no [fetch] section still gets safe defaults.
        let c: Config = toml::from_str("[server]\nlisten = \"127.0.0.1:1\"\n").unwrap();
        assert_eq!(c.fetch.backend, "http");
        assert!(!c.fetch.allow_private_hosts);
    }

    #[test]
    fn channels_telegram_parses_named_tables_and_gates_is_empty() {
        // Each `[channels.telegram.<name>]` is a bot_token + chat_id (+base_url?).
        let toml = r#"
            [channels.telegram.default]
            bot_token = "123:secret"
            chat_id = "42"

            [channels.telegram.ops]
            bot_token = "9:k"
            chat_id = "-100500"
            base_url = "http://localhost:9099"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.channels.telegram.len(), 2);
        let ops = c.channels.telegram.get("ops").unwrap();
        assert_eq!(ops.chat_id, "-100500");
        assert_eq!(ops.base_url, "http://localhost:9099");
        assert!(ops.is_configured());
        assert!(!c.channels.is_empty());

        // A telegram entry missing credentials does not count as configured.
        let blank: Config =
            toml::from_str("[channels.telegram.x]\nbot_token = \"\"\nchat_id = \"\"\n").unwrap();
        assert!(blank.channels.is_empty());
    }

    #[test]
    fn channels_discord_parses_named_map_and_env_sets_default() {
        // `[channels.discord]` is a name → webhook-url map (SOUL §25).
        let toml = r#"
            [channels.discord]
            default = "https://discord/default"
            ops = "https://discord/ops"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            c.channels.discord.get("ops").unwrap(),
            "https://discord/ops"
        );
        assert_eq!(c.channels.discord.len(), 2);
        assert!(!c.channels.is_empty());
        // No [channels] → empty (the notify tool isn't registered).
        let none: Config = toml::from_str("[server]\nlisten=\"127.0.0.1:1\"\n").unwrap();
        assert!(none.channels.is_empty());
    }

    #[test]
    fn storage_s3_parses_and_precedence_holds() {
        // `[storage.s3]` with creds → S3 backend enabled; bucket from `[storage]`.
        let toml = r#"
            [storage]
            bucket = "objects"
            local_path = "/var/lib/cat"

            [storage.s3]
            endpoint = "http://localhost:9000"
            access_key = "ak"
            secret_key = "sk"
            path_style = true
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert!(c.storage.enabled());
        assert!(c.storage.s3.enabled(), "creds present → S3 enabled");
        assert_eq!(c.storage.bucket_name(), "objects");
        assert_eq!(c.storage.s3.endpoint, "http://localhost:9000");
        assert!(c.storage.s3.path_style);
        // Region defaults to us-east-1 when unset.
        assert_eq!(c.storage.s3.region_name(), "us-east-1");

        // No creds → S3 disabled (falls back to local_path); storage still enabled.
        let local_only: Config = toml::from_str("[storage]\nlocal_path = \"/tmp/x\"\n").unwrap();
        assert!(local_only.storage.enabled() && !local_only.storage.s3.enabled());

        // `[storage.webdav]` with a url → WebDAV enabled (no S3 creds).
        let dav: Config =
            toml::from_str("[storage.webdav]\nurl = \"http://dav:8788/\"\nusername = \"u\"\n")
                .unwrap();
        assert!(dav.storage.enabled() && dav.storage.webdav.enabled());
        assert!(!dav.storage.s3.enabled());
        assert_eq!(dav.storage.webdav.url, "http://dav:8788/");

        // Neither → storage disabled.
        let none: Config = toml::from_str("[server]\nlisten=\"127.0.0.1:1\"\n").unwrap();
        assert!(!none.storage.enabled());

        // `max_object_bytes`: unset → the default; explicit → honored.
        assert_eq!(none.storage.max_object_bytes(), DEFAULT_MAX_OBJECT_BYTES);
        let capped: Config = toml::from_str("[storage]\nmax_object_bytes = 1048576\n").unwrap();
        assert_eq!(capped.storage.max_object_bytes(), 1_048_576);
    }

    #[test]
    fn storage_named_backends_resolve_with_default_and_kinds() {
        // The legacy `[storage]` default + two named backends, one of the same
        // kind as another (two local folders) — the multi-backend case (SOUL §9).
        let toml = r#"
            [storage]
            local_path = "/data/files"

            [storage.backends.archive]
            local_path = "/mnt/archive"

            [storage.backends.minio]
            [storage.backends.minio.s3]
            endpoint = "http://localhost:9000"
            access_key = "ak"
            secret_key = "sk"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        let backends = c.storage.resolved_backends();
        // default + archive + minio, named ones sorted, default first.
        let names: Vec<&str> = backends.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["default", "archive", "minio"]);
        // Kinds resolve by precedence.
        let kind = |n: &str| {
            backends
                .iter()
                .find(|(name, _)| name == n)
                .and_then(|(_, c)| c.kind())
                .unwrap()
        };
        assert_eq!(kind("default"), "local");
        assert_eq!(kind("archive"), "local");
        assert_eq!(kind("minio"), "s3");
        assert!(c.storage.enabled());

        // An explicit `[storage.backends.default]` overrides the legacy default
        // (it isn't listed twice).
        let override_default: Config = toml::from_str(
            "[storage]\nlocal_path = \"/a\"\n[storage.backends.default]\nlocal_path = \"/b\"\n",
        )
        .unwrap();
        let rb = override_default.storage.resolved_backends();
        assert_eq!(rb.iter().filter(|(n, _)| n == "default").count(), 1);
        assert_eq!(rb[0].1.local_path, "/b");

        // Named-only (no legacy default) still enables storage; an empty backend
        // table is ignored.
        let named_only: Config = toml::from_str(
            "[storage.backends.box]\nlocal_path = \"/x\"\n[storage.backends.empty]\n",
        )
        .unwrap();
        assert!(named_only.storage.enabled());
        let rb2 = named_only.storage.resolved_backends();
        assert_eq!(rb2.len(), 1, "the empty backend table is skipped");
        assert_eq!(rb2[0].0, "box");
    }

    #[test]
    fn storage_workspace_assignment_parses_and_matches() {
        use catalerum_core::model::Workspace;
        use catalerum_core::{OrganisationId, WorkspaceId};

        let ws = Workspace {
            id: WorkspaceId::new(),
            organisation_id: OrganisationId::new(),
            name: "Team A".to_string(),
            slug: "team-a".to_string(),
            archived_at: None,
        };
        let other = Workspace {
            id: WorkspaceId::new(),
            organisation_id: ws.organisation_id,
            name: "Team B".to_string(),
            slug: "team-b".to_string(),
            archived_at: None,
        };

        // Unassigned (the default) admits every workspace — the pre-feature
        // behavior, so existing configs are unaffected.
        let toml = r#"
            [storage.backends.shared]
            local_path = "/x"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        let shared = &c.storage.backends["shared"];
        assert!(shared.workspaces.is_empty());
        assert!(shared.assigned_to(&ws) && shared.assigned_to(&other));

        // Assigned by slug (case-insensitive) or UUID; everything else is out.
        let toml = format!(
            r#"
            [storage]
            local_path = "/data/files"
            workspaces = ["Team-A"]

            [storage.backends.dav]
            workspaces = ["{}"]
            [storage.backends.dav.webdav]
            url = "http://dav:8788/"
        "#,
            ws.id
        );
        let c: Config = toml::from_str(&toml).unwrap();
        // The default backend carries the top-level assignment.
        let default = c.storage.default_backend();
        assert!(default.assigned_to(&ws), "slug matches case-insensitively");
        assert!(!default.assigned_to(&other));
        let dav = &c.storage.backends["dav"];
        assert!(dav.assigned_to(&ws), "UUID entry matches by id");
        assert!(!dav.assigned_to(&other));

        // The bare predicate ignores whitespace padding around entries.
        assert!(workspace_assigned(&[" team-a ".to_string()], &ws));
        assert!(!workspace_assigned(&["team-a".to_string()], &other));
    }

    #[test]
    fn secrets_redact_in_debug_but_round_trip() {
        let toml = r#"
            [llm]
            api_key = "super-secret-key"
            [neo4j]
            password = "neo-pw"
            [storage.s3]
            access_key = "ak"
            secret_key = "s3-secret"
            [fetch.firecrawl]
            api_key = "fc-secret"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        // Transparent deserialize: the values are present via `expose()`.
        assert_eq!(c.llm.api_key.expose(), "super-secret-key");
        assert_eq!(c.neo4j.password.expose(), "neo-pw");
        assert_eq!(c.storage.s3.secret_key.expose(), "s3-secret");
        assert_eq!(c.fetch.firecrawl.api_key.expose(), "fc-secret");

        // `Debug` of the whole config redacts EVERY secret — no value leaks.
        let dbg = format!("{c:?}");
        for leaked in ["super-secret-key", "neo-pw", "s3-secret", "fc-secret"] {
            assert!(
                !dbg.contains(leaked),
                "secret `{leaked}` leaked in Debug output"
            );
        }
        assert!(
            dbg.contains("Secret(\"***\")"),
            "secrets render as redacted"
        );

        // Transparent serialize: a `Secret` still round-trips its value (config
        // persistence), so redaction is Debug-only, not data loss.
        assert_eq!(serde_json::to_string(&Secret::from("x")).unwrap(), "\"x\"");
    }

    #[test]
    fn search_config_parses_status_and_redacts_keys() {
        let toml = r#"
            [search]
            backend = "tavily"
            [search.brave]
            api_key = "brave-secret"
            [search.tavily]
            api_key = "tvly-secret"
            [search.searxng]
            base_url = "https://searx.example.org"
            [search.google]
            api_key = "g-secret"
            cx = "0123:abc"
            [search.serpapi]
            api_key = "serp-secret"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.search.backend, "tavily");
        // Values present via expose(); keys redacted in Debug.
        assert_eq!(c.search.brave.api_key.expose(), "brave-secret");
        assert_eq!(c.search.serpapi.engine, "google"); // default applied
        let dbg = format!("{:?}", c.search);
        for leaked in ["brave-secret", "tvly-secret", "g-secret", "serp-secret"] {
            assert!(!dbg.contains(leaked), "search secret `{leaked}` leaked");
        }
        // is_enabled / provider_status: configured providers on, Exa (unset) off.
        let status = c.search.provider_status();
        let on: std::collections::BTreeMap<_, _> = status.into_iter().collect();
        assert!(on["brave"] && on["tavily"] && on["searxng"] && on["google"] && on["serpapi"]);
        assert!(!on["exa"]);
        assert!(c.search.any_enabled());
        // Google needs BOTH key and cx; default config has neither -> off.
        assert!(!SearchConfig::default().any_enabled());
        assert_eq!(SearchConfig::default().backend, "brave");
    }

    #[test]
    fn deserializes_full_config_ignoring_unknown_sections() {
        // Mirrors config/catalerum.toml: unknown sections must be ignored.
        let toml = r#"
            [server]
            listen = "127.0.0.1:9000"

            [database]
            url = "postgres://x"

            [llm]
            base_url = "http://llm/v1"
            api_key = "k"
            default_model = "echo"

            [neo4j]
            url = "bolt://localhost"

            [mcp]
            enabled = false
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.server.listen, "127.0.0.1:9000");
        assert_eq!(c.database.url, "postgres://x");
        assert_eq!(c.llm.default_model, "echo");
        // Untouched section keeps its default.
        assert!(c.valkey.enabled);
    }

    #[test]
    fn secrets_master_key_decodes_and_validates() {
        use base64::Engine as _;
        // Unset → None (feature disabled, not an error).
        assert!(SecretsConfig::default()
            .master_key_bytes()
            .unwrap()
            .is_none());

        // A valid 32-byte base64 key → Some(key).
        let key = [7u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(key);
        let cfg = SecretsConfig {
            master_key: Secret::from(b64.as_str()),
        };
        assert_eq!(cfg.master_key_bytes().unwrap(), Some(key));

        // Wrong length → Err (fail loud, never silently disable a set-but-broken key).
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let cfg = SecretsConfig {
            master_key: Secret::from(short.as_str()),
        };
        assert!(cfg.master_key_bytes().is_err());

        // Not base64 → Err.
        let cfg = SecretsConfig {
            master_key: Secret::from("not base64!!!"),
        };
        assert!(cfg.master_key_bytes().is_err());
    }

    #[test]
    fn external_db_defaults_are_bounded() {
        let c = ExternalDbConfig::default();
        assert_eq!(c.pool_max_connections, 5);
        assert_eq!(c.statement_timeout_ms, 15_000);
        assert_eq!(c.max_rows, 1000);
        assert!(c.connections.is_empty());
    }

    #[test]
    fn external_db_connections_deserialize_and_are_workspace_scoped() {
        let workspace_id = catalerum_core::WorkspaceId::new();
        let toml = format!(
            r#"
            [external_db.connections.reporting]
            host = "db.internal"
            database = "reports"
            username = "reader"
            password = "do-not-log"
            sslmode = "require"
            schema = "analytics"
            pool_max = 2
            workspaces = ["Team-A", "{workspace_id}"]
            "#
        );
        let c: Config = toml::from_str(&toml).unwrap();
        let reporting = &c.external_db.connections["reporting"];
        assert_eq!(reporting.port, 5432);
        assert_eq!(reporting.resolved_password().unwrap(), "do-not-log");
        assert!(!format!("{reporting:?}").contains("do-not-log"));

        let by_slug = catalerum_core::model::Workspace {
            id: catalerum_core::WorkspaceId::new(),
            organisation_id: catalerum_core::OrganisationId::new(),
            name: "Team A".to_string(),
            slug: "team-a".to_string(),
            archived_at: None,
        };
        let by_id = catalerum_core::model::Workspace {
            id: workspace_id,
            organisation_id: catalerum_core::OrganisationId::new(),
            name: "Other".to_string(),
            slug: "other".to_string(),
            archived_at: None,
        };
        let excluded = catalerum_core::model::Workspace {
            id: catalerum_core::WorkspaceId::new(),
            organisation_id: catalerum_core::OrganisationId::new(),
            name: "Excluded".to_string(),
            slug: "excluded".to_string(),
            archived_at: None,
        };
        assert!(reporting.assigned_to(&by_slug));
        assert!(reporting.assigned_to(&by_id));
        assert!(!reporting.assigned_to(&excluded));

        let pg = reporting.postgres_config();
        assert_eq!(pg.host, "db.internal");
        assert_eq!(pg.sslmode.as_deref(), Some("require"));
        assert_eq!(pg.schema.as_deref(), Some("analytics"));
        assert_eq!(pg.pool_max, Some(2));
    }

    #[test]
    fn env_overrides_the_shared_link_signing_secrets() {
        // The multi-pod HA fix (SOUL §16 M7): the link-signing secrets must be
        // injectable from the environment (a k8s Secret) so every pod signs with
        // the SAME key and links verify across pods — otherwise each pod falls
        // back to a fresh per-process random key and links minted on one pod fail
        // on another. Unset ⇒ still `None` (the per-process-random fallback).
        assert!(Config::default().server.download_secret.is_none());
        assert!(Config::default().server.trigger_secret.is_none());

        // Safe on edition 2021; the only production caller (`main.rs`) reads the env
        // once at single-threaded boot, and no other test asserts on these fields.
        std::env::set_var("CATALERUM_SERVER__DOWNLOAD_SECRET", "dl-shared-key");
        std::env::set_var("CATALERUM_SERVER__TRIGGER_SECRET", "tr-shared-key");
        let cfg = Config::default().with_env_overrides();
        std::env::remove_var("CATALERUM_SERVER__DOWNLOAD_SECRET");
        std::env::remove_var("CATALERUM_SERVER__TRIGGER_SECRET");

        assert_eq!(
            cfg.server.download_secret.as_ref().map(Secret::expose),
            Some("dl-shared-key")
        );
        assert_eq!(
            cfg.server.trigger_secret.as_ref().map(Secret::expose),
            Some("tr-shared-key")
        );
    }

    #[test]
    fn env_overrides_configure_sso() {
        // SSO is env-driven in prod (issuer/client from the deployment, secrets from a
        // k8s Secret). Without env handling the whole feature stays off and the web
        // login view shows no "Sign in with SSO" button — regression guard for that.
        assert!(!Config::default().sso.is_enabled());

        std::env::set_var(
            "CATALERUM_SSO__ISSUER",
            "https://id.example.com/realms/default",
        );
        std::env::set_var("CATALERUM_SSO__CLIENT_ID", "catalerum");
        std::env::set_var("CATALERUM_SSO__CLIENT_SECRET", "cs-secret");
        std::env::set_var("CATALERUM_SSO__JIT_PROVISIONING", "enabled");
        let cfg = Config::default().with_env_overrides();
        std::env::remove_var("CATALERUM_SSO__ISSUER");
        std::env::remove_var("CATALERUM_SSO__CLIENT_ID");
        std::env::remove_var("CATALERUM_SSO__CLIENT_SECRET");
        std::env::remove_var("CATALERUM_SSO__JIT_PROVISIONING");

        assert!(cfg.sso.is_enabled());
        assert_eq!(cfg.sso.issuer, "https://id.example.com/realms/default");
        assert_eq!(cfg.sso.client_id, "catalerum");
        assert_eq!(cfg.sso.client_secret.expose(), "cs-secret");
        assert!(cfg.sso.jit_enabled());
    }

    #[test]
    fn telemetry_defaults_are_private_and_exporters_are_disabled() {
        let telemetry = Config::default().telemetry;
        assert!(!telemetry.otlp.enabled);
        assert!(!telemetry.langfuse.enabled);
        assert_eq!(telemetry.otlp.content, TelemetryContent::MetadataOnly);
        assert_eq!(telemetry.langfuse.content, TelemetryContent::MetadataOnly);
        assert_eq!(telemetry.sample_ratio, 1.0);
    }

    #[test]
    fn telemetry_toml_parses_independent_content_policies() {
        let config: Config = toml::from_str(
            r#"
            [telemetry]
            service_name = "catalog-test"
            sample_ratio = 0.25
            [telemetry.otlp]
            enabled = true
            endpoint = "http://collector:4318"
            content = "everything"
            headers = { x-api-key = "secret" }
            [telemetry.langfuse]
            enabled = true
            public_key = "pk-lf-test"
            secret_key = "sk-lf-test"
            content = "all-except-system-prompts"
            "#,
        )
        .unwrap();
        assert_eq!(config.telemetry.service_name, "catalog-test");
        assert_eq!(config.telemetry.sample_ratio, 0.25);
        assert_eq!(config.telemetry.otlp.content, TelemetryContent::Everything);
        assert_eq!(
            config.telemetry.langfuse.content,
            TelemetryContent::AllExceptSystemPrompts
        );
        assert_eq!(
            config.telemetry.otlp.headers["x-api-key"].expose(),
            "secret"
        );
    }
}
