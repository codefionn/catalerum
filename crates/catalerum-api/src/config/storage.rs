//! Object storage backends and backups (SOUL §9).

use serde::{Deserialize, Serialize};

use super::Secret;

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
