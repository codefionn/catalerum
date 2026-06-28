//! catalerum-backup — disaster-recovery backup + restore (SOUL §30).
//!
//! catalerum's recoverable state is **Postgres** (the single source of truth,
//! SOUL §6.1) plus the **object blobs** in the storage backend (bucket bytes that
//! deliberately never land in the DB, SOUL §9/§14). Neo4j (graph) and Qdrant
//! (vectors) are *derived* indexes — fully rebuildable from Postgres — and Valkey
//! is ephemeral, so none of the three are backed up: they are reconstructed by
//! re-ingest after a restore (SOUL §6.3/§6.4/§6.6).
//!
//! A backup is written to **any [`StorageBackend`]** — an S3/MinIO bucket, a
//! WebDAV collection, or a local directory — the same trait the live storage
//! layer uses (SOUL §9). So "back up to S3" is just pointing the destination at a
//! *different* bucket than the live data. The artifact under
//! `<prefix>/<backup-id>/` is:
//!
//! ```text
//! backups/2026-06-25T19-30-00Z/
//!   manifest.json                 # format, timestamp, schema version, table + object inventory
//!   postgres/<table>.copy.gz      # one gzipped `COPY … TO STDOUT` (text format) per table
//!   objects/<store>/<workspace-key>  # verbatim copy of every live blob, per source store (when include_objects)
//! ```
//!
//! The Postgres dump is a **pure-Rust logical dump** via the sqlx `COPY` protocol
//! (`COPY … TO STDOUT` / `FROM STDIN`) — no external `pg_dump`/`pg_restore`
//! binary, so it runs anywhere the binary runs and is testable against the same
//! ephemeral Postgres the repo tests use. Restore loads every table back with
//! `COPY … FROM STDIN`, picking a strategy from the restoring role's privilege: a
//! superuser disables foreign-key triggers (`session_replication_role = replica`)
//! for an order-independent load; a **non-superuser** (common under a hardened,
//! least-privilege role, SOUL §14) loads the tables in foreign-key dependency
//! order — parents before children — since it cannot set that superuser-only GUC.
//! Either way it needs only that the schema already exist (the binary migrates on
//! boot, SOUL §6.1).
//!
//! The [`BackupEngine`] performs one backup ([`run`](BackupEngine::run)), applies
//! retention ([`prune`](BackupEngine::prune)), and restores
//! ([`restore`](BackupEngine::restore)); [`BackupWorker`] drives `run` + `prune`
//! on a schedule, single-firing across pods via the bus lock (SOUL §11/§6.2).

#![forbid(unsafe_code)]

mod objects;
mod pg;
mod worker;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{PutMeta, StorageBackend};

pub use worker::BackupWorker;

/// sqlx's pooled Postgres handle — the dump/restore source.
pub use sqlx::PgPool;

/// The on-disk backup format version. v2 added the per-store blob layout
/// (`objects/<store>/<key>`); restore still reads a v1 flat `objects/<key>`
/// artifact. Bumped on a breaking artifact-layout change; restore refuses only a
/// *newer* version than it understands.
pub const FORMAT_VERSION: u32 = 2;

/// The source-store name a backup uses for the legacy single `[storage]` backend
/// (via [`BackupEngine::with_source_storage`]). Must match catalerum-api's
/// `DEFAULT_STORE_NAME` so a restore reunites the default store's blobs with the
/// right live backend.
pub const DEFAULT_SOURCE_NAME: &str = "default";

/// The default destination prefix (the "directory" backups live under).
pub const DEFAULT_PREFIX: &str = "backups";

/// The default number of backups to retain (older ones are pruned).
pub const DEFAULT_KEEP: usize = 7;

/// What restore does *not* rebuild — surfaced in the manifest so an operator
/// knows the derived stores are stale until re-ingest repopulates them.
const DERIVED_NOTE: &str =
    "Neo4j (graph) and Qdrant (vectors) are derived indexes, not backed up; \
     rebuild them from Postgres via re-ingest after a restore (SOUL §6.3/§6.4).";

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// The `manifest.json` at the root of a backup — the self-describing index that
/// ties the per-table dumps and copied blobs together and gates restore.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    /// The artifact-layout version ([`FORMAT_VERSION`]).
    pub format_version: u32,
    /// When the backup started (UTC).
    pub created_at: DateTime<Utc>,
    /// The `catalerum` binary version that wrote it.
    pub catalerum_version: String,
    /// The Postgres schema version (max applied `_sqlx_migrations.version`).
    /// Restore refuses a mismatch unless forced, so data never loads into a
    /// schema it was not dumped from.
    pub schema_version: Option<i64>,
    /// The Postgres table inventory.
    pub postgres: PgManifest,
    /// The object-blob inventory.
    pub objects: ObjectsManifest,
    /// Human note on the derived stores that are intentionally not included.
    pub derived_note: String,
}

/// The Postgres half of a [`Manifest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PgManifest {
    /// One entry per dumped table, in dump (and restore) order.
    pub tables: Vec<TableManifest>,
}

/// A single dumped table's manifest entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableManifest {
    /// The table name (in the `public` schema).
    pub name: String,
    /// The columns, in the dump's column order — used verbatim in the restore
    /// `COPY` so a column reorder across versions cannot misalign the load.
    pub columns: Vec<String>,
    /// Row count (informational; counted from the `COPY` text stream).
    pub rows: u64,
    /// Compressed (`.copy.gz`) byte length of the dump.
    pub bytes: u64,
    /// SHA-256 of the compressed dump, hex-encoded — restore verifies it.
    pub sha256: String,
}

/// The object-blob half of a [`Manifest`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ObjectsManifest {
    /// Whether blobs were copied (false when objects are excluded or no live
    /// storage backend is configured).
    pub included: bool,
    /// Number of blobs copied (aggregate across all stores).
    pub count: u64,
    /// Total uncompressed bytes copied (aggregate across all stores).
    pub bytes: u64,
    /// Per-source-store breakdown (format v2+). Empty for a legacy v1 artifact,
    /// whose blobs live flat at `objects/<key>` and restore into the default
    /// store.
    #[serde(default)]
    pub stores: Vec<StoreObjects>,
}

/// One source store's slice of the object inventory (format v2): its logical
/// `name` — matched to a live backend on restore — the `segment` its blobs were
/// written under (`objects/<segment>/`), and its blob counts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoreObjects {
    /// The store's logical name (e.g. `default`, or a `[storage.backends.<name>]`).
    pub name: String,
    /// The filesystem-safe path segment its blobs live under in the artifact.
    pub segment: String,
    /// Number of blobs copied from this store.
    pub count: u64,
    /// Total uncompressed bytes copied from this store.
    pub bytes: u64,
}

// ---------------------------------------------------------------------------
// Summaries
// ---------------------------------------------------------------------------

/// The outcome of a successful [`BackupEngine::run`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BackupSummary {
    /// The backup id (its `<prefix>/<id>/` directory name; a UTC timestamp).
    pub id: String,
    /// Number of tables dumped.
    pub tables: usize,
    /// Total rows dumped across all tables.
    pub rows: u64,
    /// Number of object blobs copied.
    pub objects: u64,
    /// Compressed Postgres dump size in bytes (sum of all `.copy.gz`).
    pub postgres_bytes: u64,
}

/// The outcome of a successful [`BackupEngine::restore`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreSummary {
    /// The backup id restored.
    pub id: String,
    /// Total rows loaded across all tables.
    pub rows: u64,
    /// Number of object blobs restored to the live storage backend.
    pub objects: u64,
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Performs backups and restores against a Postgres [`PgPool`] and a destination
/// [`StorageBackend`].
///
/// Built with [`BackupEngine::new`], then refined with the `with_*` builders. The
/// destination is where artifacts are written/read; the **source stores** are the
/// live blob backends (SOUL §9) whose objects are copied into the backup (and
/// restored back out) — one default store via
/// [`with_source_storage`](Self::with_source_storage) or several named ones via
/// [`with_named_source`](Self::with_named_source). Cloning is cheap (all handles
/// are `Arc`/pool).
#[derive(Clone)]
pub struct BackupEngine {
    pool: PgPool,
    dest: Arc<dyn StorageBackend>,
    /// The live blob backends to mirror, as `(store name, backend)`. A backup
    /// copies each under `objects/<segment>/`; restore routes each store's blobs
    /// back to the backend registered under the same name. Empty → Postgres-only.
    sources: Vec<(String, Arc<dyn StorageBackend>)>,
    prefix: String,
    include_objects: bool,
    keep: usize,
    version: String,
    /// Force the dependency-ordered (non-superuser) Postgres restore path even
    /// when the connecting role is a superuser. Off by default (superuser roles
    /// take the faster trigger-disabling load); flipped on to exercise the
    /// ordered path deterministically. See [`with_force_ordered_restore`].
    ///
    /// [`with_force_ordered_restore`]: BackupEngine::with_force_ordered_restore
    force_ordered_restore: bool,
}

impl BackupEngine {
    /// A new engine dumping `pool` to `dest`, stamping artifacts with
    /// `catalerum_version`. Defaults: prefix [`DEFAULT_PREFIX`], objects included,
    /// retention [`DEFAULT_KEEP`], no source storage (Postgres-only until
    /// [`with_source_storage`](Self::with_source_storage) supplies one).
    #[must_use]
    pub fn new(
        pool: PgPool,
        dest: Arc<dyn StorageBackend>,
        catalerum_version: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            dest,
            sources: Vec::new(),
            prefix: DEFAULT_PREFIX.to_string(),
            include_objects: true,
            keep: DEFAULT_KEEP,
            version: catalerum_version.into(),
            force_ordered_restore: false,
        }
    }

    /// Set the destination prefix the backup directory lives under (empty →
    /// [`DEFAULT_PREFIX`]).
    #[must_use]
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        let p = prefix.into();
        self.prefix = if p.trim().is_empty() {
            DEFAULT_PREFIX.to_string()
        } else {
            p.trim().trim_matches('/').to_string()
        };
        self
    }

    /// Attach the **default** live object-storage backend (SOUL §9) whose blobs
    /// the backup copies in and a restore copies back out. Sugar for
    /// [`with_named_source`](Self::with_named_source) under [`DEFAULT_SOURCE_NAME`].
    #[must_use]
    pub fn with_source_storage(mut self, source: Arc<dyn StorageBackend>) -> Self {
        self.add_source(DEFAULT_SOURCE_NAME.to_string(), source);
        self
    }

    /// Attach a **named** live object-storage backend (SOUL §9) to mirror. A
    /// backup copies every attached store's blobs under `objects/<name>/`; a
    /// restore routes each store's blobs back to the backend registered under the
    /// same name. Re-attaching a name replaces the prior backend.
    #[must_use]
    pub fn with_named_source(
        mut self,
        name: impl Into<String>,
        source: Arc<dyn StorageBackend>,
    ) -> Self {
        self.add_source(name.into(), source);
        self
    }

    /// Insert or replace a named source (last write wins per name).
    fn add_source(&mut self, name: String, source: Arc<dyn StorageBackend>) {
        if let Some(slot) = self.sources.iter_mut().find(|(n, _)| *n == name) {
            slot.1 = source;
        } else {
            self.sources.push((name, source));
        }
    }

    /// The live backend registered under `name`, if any.
    fn source_by_name(&self, name: &str) -> Option<Arc<dyn StorageBackend>> {
        self.sources
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, b)| b.clone())
    }

    /// Whether to copy object blobs (default `true`). When `false` (or no source
    /// storage is attached), a backup is Postgres-only.
    #[must_use]
    pub fn with_include_objects(mut self, include: bool) -> Self {
        self.include_objects = include;
        self
    }

    /// How many recent backups [`prune`](Self::prune) keeps (0 → keep all).
    #[must_use]
    pub fn with_keep(mut self, keep: usize) -> Self {
        self.keep = keep;
        self
    }

    /// Force the **dependency-ordered** Postgres restore path — the one a
    /// non-superuser role must use — even when the connecting role *is* a
    /// superuser (which would otherwise take the faster trigger-disabling load).
    ///
    /// Restore normally detects the role's privilege and picks the strategy
    /// itself, so this is not needed in production. It exists so the ordered path
    /// can be exercised deterministically (e.g. in CI on a superuser dev role)
    /// and for operators who want to prove a least-privilege restore end-to-end.
    #[must_use]
    pub fn with_force_ordered_restore(mut self, force: bool) -> Self {
        self.force_ordered_restore = force;
        self
    }

    /// The destination prefix backups are written under.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Take a full backup: dump every Postgres table, copy the live blobs (when
    /// enabled), and write the `manifest.json`. Returns a [`BackupSummary`]. The
    /// backup id is a UTC timestamp (`YYYY-MM-DDThh-mm-ssZ`), so listings sort
    /// chronologically by name.
    pub async fn run(&self) -> Result<BackupSummary> {
        let created_at = Utc::now();
        let id = created_at.format("%Y-%m-%dT%H-%M-%SZ").to_string();
        tracing::info!(%id, prefix = %self.prefix, "starting backup");

        // --- Postgres -------------------------------------------------------
        let specs = pg::list_tables(&self.pool).await?;
        let mut tables = Vec::with_capacity(specs.len());
        let mut postgres_bytes = 0u64;
        for spec in &specs {
            let entry =
                pg::dump_table(&self.pool, self.dest.as_ref(), &self.prefix, &id, spec).await?;
            postgres_bytes += entry.bytes;
            tables.push(entry);
        }
        let schema_version = pg::schema_version(&self.pool).await?;
        let rows = tables.iter().map(|t| t.rows).sum();

        // --- Object blobs ---------------------------------------------------
        // Mirror every attached store under its own `objects/<segment>/` sub-tree
        // so a restore can reunite each store's blobs with the right backend.
        let objects = if self.include_objects && !self.sources.is_empty() {
            let segments = assign_segments(&self.sources);
            let mut stores = Vec::with_capacity(self.sources.len());
            let mut total_count = 0u64;
            let mut total_bytes = 0u64;
            for ((name, backend), segment) in self.sources.iter().zip(segments) {
                let (count, bytes) = objects::copy_store_objects(
                    backend.as_ref(),
                    self.dest.as_ref(),
                    &self.prefix,
                    &id,
                    &segment,
                )
                .await?;
                total_count += count;
                total_bytes += bytes;
                stores.push(StoreObjects {
                    name: name.clone(),
                    segment,
                    count,
                    bytes,
                });
            }
            ObjectsManifest {
                included: true,
                count: total_count,
                bytes: total_bytes,
                stores,
            }
        } else {
            ObjectsManifest::default()
        };

        // --- Manifest -------------------------------------------------------
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            created_at,
            catalerum_version: self.version.clone(),
            schema_version,
            postgres: PgManifest { tables },
            objects: objects.clone(),
            derived_note: DERIVED_NOTE.to_string(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        put_bytes(
            self.dest.as_ref(),
            &self.manifest_key(&id),
            manifest_bytes,
            "application/json",
        )
        .await?;

        let summary = BackupSummary {
            id,
            tables: specs.len(),
            rows,
            objects: objects.count,
            postgres_bytes,
        };
        tracing::info!(
            id = %summary.id,
            tables = summary.tables,
            rows = summary.rows,
            objects = summary.objects,
            "backup written"
        );
        Ok(summary)
    }

    /// Restore a backup by id (destructive — it **replaces** the current
    /// Postgres contents and live blobs). The Postgres schema must already match
    /// the backup's `schema_version` (the binary migrates on boot); `force`
    /// bypasses that guard. The load strategy is chosen automatically from the
    /// connecting role's privilege (superuser → trigger-disabled load;
    /// non-superuser → foreign-key dependency-ordered load; see
    /// [`with_force_ordered_restore`]). The derived stores (Neo4j/Qdrant) are left
    /// stale — rebuild them via re-ingest (SOUL §6.3/§6.4).
    ///
    /// [`with_force_ordered_restore`]: BackupEngine::with_force_ordered_restore
    pub async fn restore(&self, id: &str, force: bool) -> Result<RestoreSummary> {
        let manifest = self.read_manifest(id).await?;
        if manifest.format_version > FORMAT_VERSION {
            return Err(Error::invalid(format!(
                "backup `{id}` is format v{} but this build reads up to v{FORMAT_VERSION}",
                manifest.format_version
            )));
        }
        let live = pg::schema_version(&self.pool).await?;
        if manifest.schema_version != live && !force {
            return Err(Error::invalid(format!(
                "backup `{id}` schema_version {:?} != live {:?}; migrate to a matching build \
                 (or pass force to override — data may not load into a changed schema)",
                manifest.schema_version, live
            )));
        }

        let rows = pg::restore_postgres(
            &self.pool,
            self.dest.as_ref(),
            &self.prefix,
            id,
            &manifest,
            self.force_ordered_restore,
        )
        .await?;

        let objects = if manifest.objects.included && manifest.objects.count > 0 {
            if self.sources.is_empty() {
                return Err(Error::invalid(
                    "backup includes object blobs but no live storage backend ([storage]) is \
                     configured to restore them into",
                ));
            }
            if manifest.objects.stores.is_empty() {
                // Legacy v1 artifact: blobs are flat at `objects/<key>`; restore
                // them into the default store (or the sole store if no "default").
                let src = self
                    .source_by_name(DEFAULT_SOURCE_NAME)
                    .or_else(|| self.sources.first().map(|(_, b)| b.clone()))
                    .ok_or_else(|| {
                        Error::invalid("no live storage backend to restore blobs into")
                    })?;
                objects::restore_legacy_objects(self.dest.as_ref(), src.as_ref(), &self.prefix, id)
                    .await?
            } else {
                // v2: route each store's blobs back to its like-named live backend.
                let mut total = 0u64;
                for store in &manifest.objects.stores {
                    let src = self.source_by_name(&store.name).ok_or_else(|| {
                        Error::invalid(format!(
                            "backup `{id}` holds blobs for store `{}` but no live storage backend \
                             by that name is configured to restore them into",
                            store.name
                        ))
                    })?;
                    total += objects::restore_store_objects(
                        self.dest.as_ref(),
                        src.as_ref(),
                        &self.prefix,
                        id,
                        &store.segment,
                    )
                    .await?;
                }
                total
            }
        } else {
            0
        };

        tracing::info!(%id, rows, objects, "restore complete");
        Ok(RestoreSummary {
            id: id.to_string(),
            rows,
            objects,
        })
    }

    /// The *complete* backup ids present at the destination (those with a written
    /// `manifest.json`), sorted oldest→newest (ids are UTC timestamps, so lexical
    /// order is chronological). A directory left by a crashed backup is omitted.
    pub async fn list(&self) -> Result<Vec<String>> {
        list_backup_ids(self.dest.as_ref(), &self.prefix).await
    }

    /// Delete all but the newest [`keep`](Self::with_keep) backups. Returns the
    /// number of backups removed (0 when retention is disabled or nothing is
    /// over the limit).
    pub async fn prune(&self) -> Result<usize> {
        if self.keep == 0 {
            return Ok(0);
        }
        let ids = self.list().await?;
        if ids.len() <= self.keep {
            return Ok(0);
        }
        let stale = &ids[..ids.len() - self.keep];
        for id in stale {
            self.delete_backup(id).await?;
            tracing::info!(%id, "pruned old backup");
        }
        Ok(stale.len())
    }

    /// Read + parse a backup's `manifest.json`.
    pub async fn read_manifest(&self, id: &str) -> Result<Manifest> {
        let bytes = get_all(self.dest.as_ref(), &self.manifest_key(id)).await?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        Ok(manifest)
    }

    /// Delete every object under a backup's directory.
    async fn delete_backup(&self, id: &str) -> Result<()> {
        let dir = format!("{}/{id}/", self.prefix);
        let mut stream = self.dest.list(&dir).await?;
        let mut keys = Vec::new();
        while let Some(meta) = stream.next().await {
            keys.push(meta?.key);
        }
        drop(stream);
        for key in keys {
            self.dest.delete(&key).await?;
        }
        Ok(())
    }

    fn manifest_key(&self, id: &str) -> String {
        format!("{}/{id}/manifest.json", self.prefix)
    }
}

// ---------------------------------------------------------------------------
// Shared storage helpers
// ---------------------------------------------------------------------------

/// Assign each source a unique, filesystem-safe path segment for
/// `objects/<segment>/`, aligned with `sources` order. Names that sanitize to
/// the same segment get a numeric suffix so two stores never collide.
fn assign_segments(sources: &[(String, Arc<dyn StorageBackend>)]) -> Vec<String> {
    segments_for(sources.iter().map(|(n, _)| n.as_str()))
}

/// The unique-segment assignment over bare names (the testable core of
/// [`assign_segments`]).
fn segments_for<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut used = std::collections::HashSet::new();
    let mut out = Vec::new();
    for name in names {
        let base = safe_segment(name);
        let mut seg = base.clone();
        let mut n = 1u32;
        while !used.insert(seg.clone()) {
            seg = format!("{base}-{n}");
            n += 1;
        }
        out.push(seg);
    }
    out
}

/// A store name reduced to a safe path segment: ASCII alphanumerics and `.`/`_`/`-`
/// are kept, everything else becomes `_`. An empty result — **or** a path-special
/// all-dots segment (`.`, `..`, `...`), which would resolve away or traverse out of
/// the backup's `objects/<segment>/` root — falls back to `store`.
fn safe_segment(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.bytes().all(|b| b == b'.') {
        "store".to_string()
    } else {
        s
    }
}

/// Collect a backend `get` stream into a single buffer.
pub(crate) async fn get_all(backend: &dyn StorageBackend, key: &str) -> Result<Vec<u8>> {
    let mut stream = backend.get(key).await?;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf)
}

/// Write `bytes` to `backend` at `key` (the backends buffer the whole object, so
/// a single-chunk stream matches their `put` shape).
pub(crate) async fn put_bytes(
    backend: &dyn StorageBackend,
    key: &str,
    bytes: Vec<u8>,
    content_type: &str,
) -> Result<()> {
    let len = bytes.len() as u64;
    let stream = futures::stream::once(async move { Ok(bytes) }).boxed();
    backend
        .put(
            key,
            stream,
            PutMeta {
                content_type: Some(content_type.to_string()),
                content_length: Some(len),
            },
        )
        .await
}

/// The distinct **complete** backup ids under `<prefix>/`, sorted ascending. An id
/// is the first path segment after the prefix (e.g. `backups/<id>/manifest.json`);
/// only ids that carry a `manifest.json` are returned, so a partially-written
/// (crashed) backup is skipped rather than counted as restorable.
pub(crate) async fn list_backup_ids(
    dest: &dyn StorageBackend,
    prefix: &str,
) -> Result<Vec<String>> {
    let root = format!("{prefix}/");
    let mut stream = dest.list(&root).await?;
    // Only ids with a written `manifest.json` count as complete, restorable
    // backups. `run()` writes the manifest **last**, so a directory left behind by
    // a backup that crashed mid-dump (table files but no manifest) must be ignored
    // — otherwise it would show up in `list()` as if restorable and, worse, occupy
    // a slot in `prune`'s retention window, letting a broken partial evict a good
    // complete backup. Detected in the same single list pass (no extra reads).
    let mut ids = std::collections::BTreeSet::new();
    while let Some(meta) = stream.next().await {
        let key = meta?.key;
        if let Some(rest) = key.strip_prefix(&root) {
            if let Some((id, tail)) = rest.split_once('/') {
                if !id.is_empty() && tail == "manifest.json" {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_segment_sanitizes_and_defaults() {
        assert_eq!(safe_segment("default"), "default");
        assert_eq!(safe_segment("my.store_1-x"), "my.store_1-x");
        // Anything outside [A-Za-z0-9._-] becomes `_`.
        assert_eq!(safe_segment("a/b c:d"), "a_b_c_d");
        // Empty-after-trim falls back to a literal segment.
        assert_eq!(safe_segment(""), "store");
        assert_eq!(safe_segment("   "), "store");
        // Path-special all-dots names can't become a traversing `..` segment.
        assert_eq!(safe_segment("."), "store");
        assert_eq!(safe_segment(".."), "store");
        assert_eq!(safe_segment("..."), "store");
        // A `/`-containing name with `..` is neutralised to underscores, not kept
        // as a path component, and a legit dotted name is still preserved.
        assert_eq!(safe_segment("a/../b"), "a_.._b");
        assert_eq!(safe_segment(".hidden"), ".hidden");
    }

    #[test]
    fn segments_for_dedupes_collisions() {
        // Exact-name collisions get numeric suffixes, in order.
        assert_eq!(
            segments_for(["a", "a", "b", "a"].into_iter()),
            vec!["a", "a-1", "b", "a-2"]
        );
        // Names that *sanitize* to the same segment also collide and are suffixed.
        assert_eq!(
            segments_for(["a/b", "a b"].into_iter()),
            vec!["a_b", "a_b-1"]
        );
        // Path-special names all fall back to `store` and then dedupe cleanly.
        assert_eq!(
            segments_for([".", "..", ""].into_iter()),
            vec!["store", "store-1", "store-2"]
        );
    }

    #[tokio::test]
    async fn list_backup_ids_returns_only_complete_backups_sorted() {
        use catalerum_storage::LocalFsBackend;

        let dir = tempfile::tempdir().unwrap();
        let dest = LocalFsBackend::new(dir.path());
        let prefix = "backups";
        let manifest = |id: &str| format!("{prefix}/{id}/manifest.json");
        let table = |id: &str| format!("{prefix}/{id}/postgres/notes.jsonl.gz");

        // Two complete backups (manifest written) + one partial newer backup that
        // crashed after a table dump but before its manifest.
        for id in ["2026-06-01T00-00-00Z", "2026-06-02T00-00-00Z"] {
            put_bytes(&dest, &table(id), b"data".to_vec(), "application/gzip")
                .await
                .unwrap();
            put_bytes(&dest, &manifest(id), b"{}".to_vec(), "application/json")
                .await
                .unwrap();
        }
        let partial = "2026-06-03T00-00-00Z";
        put_bytes(&dest, &table(partial), b"data".to_vec(), "application/gzip")
            .await
            .unwrap();

        let ids = list_backup_ids(&dest, prefix).await.unwrap();
        // The manifest-less partial is excluded even though it's the newest; the
        // rest come back sorted oldest→newest.
        assert_eq!(
            ids,
            vec![
                "2026-06-01T00-00-00Z".to_string(),
                "2026-06-02T00-00-00Z".to_string()
            ],
            "only complete backups, sorted; the partial `{partial}` is omitted",
        );
    }

    #[test]
    fn v1_manifest_without_stores_deserializes_with_empty_stores() {
        // A legacy v1 `objects` manifest had no `stores` field; it must still parse
        // (the `#[serde(default)]`) so restore can take the flat-layout path.
        let json = r#"{"included":true,"count":3,"bytes":120}"#;
        let m: ObjectsManifest = serde_json::from_str(json).expect("v1 objects manifest parses");
        assert!(m.included);
        assert_eq!(m.count, 3);
        assert!(m.stores.is_empty(), "v1 has no per-store breakdown");
    }
}
