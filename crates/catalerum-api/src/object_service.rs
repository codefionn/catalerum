//! Object storage core shared by the `/storage` routes, the storage and terminal
//! tools, the action runner and the storage watcher (SOUL §9/§10): store
//! resolution, object read/write/copy/delete, cataloguing + `StorageObject`
//! trigger dispatch, and the catalogue/backend reconcile scan. Free of HTTP
//! handlers so non-route callers need not depend on `routes::storage`.

use futures::stream::{self, StreamExt};
use serde::Serialize;

use std::collections::{HashMap, HashSet};

use catalerum_core::model::{Bucket, Connection, ConnectionKind, StoredObject, Workspace};
use catalerum_core::provider::{ObjectMeta, PutMeta};
use catalerum_core::{Error as CoreError, ObjectId, UserId, WorkspaceId};
use catalerum_store::{Store, StoreError, UpsertObject, DEFAULT_OBJECT_LIMIT};

use crate::error::{ApiError, ApiResult};
use crate::state::{AppState, StorageHandle, StorageRegistry};

/// Reject an object key the storage contract forbids regardless of backend:
/// empty, or any non-normal path component (absolute / `..` / `.`). Validating at
/// the API boundary (before [`StorageHandle::physical_key`](crate::state::StorageHandle::physical_key)
/// namespaces it) makes the
/// accepted-key contract identical whichever backend is deployed — the local-fs
/// backend already rejects these, so without this the *same* request would succeed
/// on S3 but 400 on local. Also defence-in-depth: a `..` key can't produce a
/// `<ws>/../…` physical key that a future canonicalizing layer might mis-resolve.
pub(crate) fn validate_object_key(key: &str) -> ApiResult<()> {
    use std::path::{Component, Path};
    if key.is_empty() {
        return Err(ApiError::bad_request("object key must not be empty"));
    }
    if !Path::new(key)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(ApiError::bad_request(
            "object key must be a relative path with no '.', '..', or absolute segments",
        ));
    }
    Ok(())
}

/// Map a backend error to an API error: `NotFound` / a bad key (`Invalid`) stay
/// precise, the rest are `500`.
pub(crate) fn map_storage_err(e: CoreError) -> ApiError {
    match e {
        CoreError::NotFound => ApiError::NotFound,
        CoreError::Invalid(m) => ApiError::bad_request(m),
        other => ApiError::internal(format!("storage error: {other}")),
    }
}

/// Resolve the store a request targets to a live [`StorageHandle`] (SOUL §9): the
/// `?store=` name, or the default store when omitted, looked up first among the
/// config-defined backends and then among the workspace's **runtime** (user-added)
/// storage `Connection`s. `404` when nothing resolves (storage disabled / unknown
/// store); `400` when no store is named and several exist with no default.
///
/// Thin wrapper over [`resolve_store`] for HTTP handlers — passes the registry and
/// DB store off [`AppState`]. `user_id` lets the "no `?store=`" path honour the
/// caller's per-user **default files** override (SOUL §9; [`StorageSettings`]).
pub(crate) async fn resolve(
    state: &AppState,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    store: Option<&str>,
) -> ApiResult<StorageHandle> {
    resolve_store(state.storage(), state.store(), workspace_id, user_id, store).await
}

/// The workspace row config-store **visibility** checks against (SOUL §9/§18) —
/// fetched only when some config store carries a `workspaces` assignment, so the
/// common unassigned config never pays the lookup. A load failure propagates
/// (assigned stores fail closed rather than open).
pub(crate) async fn visibility_workspace(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
) -> ApiResult<Option<Workspace>> {
    if !registry.has_assignments() {
        return Ok(None);
    }
    store_db
        .workspaces()
        .get(workspace_id)
        .await
        .map(Some)
        .map_err(|e| ApiError::internal(format!("loading workspace: {e}")))
}

/// The store resolver, free of [`AppState`] so a [`Tool`](catalerum_core::tool::Tool)
/// (which holds only `Arc<StorageRegistry>` + `Store`) can resolve a store the same
/// way an HTTP handler does (`copy_object`/`stage_object`). See [`resolve`].
///
/// Default order when no `?store=` is named: the caller's per-user
/// [`StorageSettings::default_store`] (when it still resolves) → the `[storage]`
/// config default → the sole store across config + runtime sources. Config stores
/// assigned to other workspaces (`workspaces` in the backend's config) are
/// invisible here throughout — they neither resolve by name nor count as
/// defaults (SOUL §9/§18).
pub(crate) async fn resolve_store(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    store: Option<&str>,
) -> ApiResult<StorageHandle> {
    let vis_ws = visibility_workspace(registry, store_db, workspace_id).await?;
    let name = match store.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            // The caller's per-user default files store (SOUL §9), used only when it
            // still resolves to a real backend — a stale pick falls through to the
            // config default rather than erroring every bare op.
            let user_default = match user_id {
                Some(uid) => store_db
                    .storage_settings()
                    .get(workspace_id, uid)
                    .await
                    .map_err(|e| ApiError::internal(format!("loading storage settings: {e}")))?
                    .default_store
                    .filter(|d| !d.trim().is_empty()),
                None => None,
            };
            let user_default = match user_default {
                Some(d)
                    if registry.visible(&d, vis_ws.as_ref())
                        || find_storage_connection(store_db, workspace_id, &d)
                            .await?
                            .is_some() =>
                {
                    Some(d)
                }
                _ => None,
            };
            match user_default {
                Some(d) => d,
                None => match registry
                    .default_name()
                    .filter(|d| registry.visible(d, vis_ws.as_ref()))
                {
                    Some(d) => d.to_string(),
                    // No config default (or it isn't visible here). Use the sole store
                    // across *both* sources (config + runtime); ask the caller to pick
                    // when several exist; 404 when none do.
                    None => {
                        let config_names: Vec<String> = registry
                            .infos_for(vis_ws.as_ref())
                            .into_iter()
                            .map(|(n, _)| n)
                            .collect();
                        let runtime = runtime_stores(registry, store_db, workspace_id).await?;
                        let total = config_names.len() + runtime.len();
                        match total {
                            0 => return Err(ApiError::NotFound),
                            1 => config_names
                                .into_iter()
                                .next()
                                .or_else(|| runtime.into_iter().next().map(|r| r.name))
                                .unwrap_or_default(),
                            _ => {
                                return Err(ApiError::bad_request(
                                    "multiple storage backends configured; specify ?store=",
                                ))
                            }
                        }
                    }
                },
            }
        }
    };
    // A config-defined backend wins (its name shadows any runtime connection) —
    // but only where it is visible: a store assigned to other workspaces is as if
    // it didn't exist here, so the lookup falls through to the runtime layer
    // (and 404s below when no runtime connection carries the name).
    if registry.visible(&name, vis_ws.as_ref()) {
        if let Some(cs) = registry.get(&name) {
            return Ok(cs.handle(name));
        }
    }
    // Otherwise a runtime (user-added) storage backend built from its connection.
    let Some(conn) = find_storage_connection(store_db, workspace_id, &name).await? else {
        return Err(ApiError::NotFound);
    };
    let row = store_db
        .connections()
        .get_row(workspace_id, conn.id)
        .await
        .map_err(|e| ApiError::internal(format!("loading storage connection: {e}")))?;
    let backend =
        catalerum_storage::backend_from_connection(&conn, row.config()).map_err(map_storage_err)?;
    // Catalogue under the backend's **configured** bucket (its real S3 bucket /
    // WebDAV collection), so the catalogue label + the `StorageObject` trigger's
    // `bucket` match the physical store — identical to config-defined backends
    // (state.rs `bcfg.bucket_name(&name)`). Local / WebDAV stores have no bucket,
    // so this falls back to the store name.
    let bucket = runtime_bucket_name(row.config(), &name);
    Ok(StorageHandle {
        backend,
        store: name.clone(),
        connection: name,
        bucket,
        // A browse store exposes its raw root (no `<workspace_id>/` prefix) so an
        // existing directory's files are listable as-is (§9/§18); default isolated.
        namespaced: !runtime_browse(row.config()),
    })
}

/// The catalogue bucket name for a runtime store: its config's `bucket` field (the
/// actual S3 bucket), or the store name when absent (local / WebDAV backends have
/// no bucket). Mirrors a config backend's
/// [`bucket_name`](crate::config::StorageBackendConfig::bucket_name) fallback so
/// runtime and config stores catalogue identically.
pub(crate) fn runtime_bucket_name(config: &serde_json::Value, store_name: &str) -> String {
    config
        .get("bucket")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| store_name.to_string(), str::to_string)
}

/// Whether a runtime store's config opts into **browse mode** (`"browse": true`):
/// expose its raw root with no `<workspace_id>/` namespacing (SOUL §9/§18). Absent
/// or non-true → `false` (the default isolated behavior). Mirrors a config
/// backend's [`browse`](crate::config::StorageBackendConfig::browse) flag.
pub(crate) fn runtime_browse(config: &serde_json::Value) -> bool {
    config
        .get("browse")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Whether a runtime store's config opts into **watching** (`"watch": true`): keep
/// its §10 index in sync with the backend (SOUL §9/§10). Absent/non-true → `false`.
/// Mirrors a config backend's [`watch`](crate::config::StorageBackendConfig::watch).
pub(crate) fn runtime_watch(config: &serde_json::Value) -> bool {
    config
        .get("watch")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// One workspace's **runtime** (user-added) storage backends as `(name, kind)`:
/// every kind = Storage [`Connection`] whose name is **not** a config backend's
/// catalogue connection (those are represented by the registry instead, so they
/// aren't double-counted).
pub(crate) async fn runtime_stores(
    registry: &StorageRegistry,
    store: &Store,
    workspace_id: WorkspaceId,
) -> ApiResult<Vec<RuntimeStore>> {
    let config_conn_names: HashSet<String> = registry
        .infos()
        .into_iter()
        .filter_map(|(n, _)| registry.get(&n).map(|s| s.connection.clone()))
        .collect();
    let conns = store
        .connections()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("listing connections: {e}")))?;
    let mut out = Vec::new();
    for c in conns {
        if c.kind != ConnectionKind::Storage || config_conn_names.contains(&c.name) {
            continue;
        }
        let row = store.connections().get_row(workspace_id, c.id).await.ok();
        let kind = row
            .as_ref()
            .and_then(|r| {
                r.config()
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "unknown".to_string());
        let watch = row.as_ref().is_some_and(|r| runtime_watch(r.config()));
        out.push(RuntimeStore {
            name: c.name,
            kind,
            watch,
        });
    }
    Ok(out)
}

/// A workspace's storage [`Connection`] of name `name`, if any.
pub(crate) async fn find_storage_connection(
    store: &Store,
    workspace_id: WorkspaceId,
    name: &str,
) -> ApiResult<Option<Connection>> {
    let conns = store
        .connections()
        .list_by_workspace(workspace_id)
        .await
        .map_err(|e| ApiError::internal(format!("listing connections: {e}")))?;
    Ok(conns
        .into_iter()
        .find(|c| c.kind == ConnectionKind::Storage && c.name == name))
}

/// A runtime storage backend, name + kind + watch flag, for [`runtime_stores`].
pub(crate) struct RuntimeStore {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) watch: bool,
}

/// Ensure the catalogue rows backing a store's bucket exist — a storage-kind
/// [`catalerum_core::model::Connection`] (`connection_name`) and its [`Bucket`]
/// (`bucket_name`) — and return the bucket. The blob backend owns the bytes; these
/// Postgres rows are the *queryable* catalogue (§1/§6.1/§9). Both are found-or-
/// created idempotently (§3.4), so repeatedly cataloguing never duplicates and a
/// runtime store's existing connection (and its config/secrets) is preserved (the
/// `ensure` passes `None`, a COALESCE no-op on config).
async fn ensure_bucket(
    store: &Store,
    workspace_id: WorkspaceId,
    connection_name: &str,
    bucket_name: &str,
) -> Result<Bucket, StoreError> {
    let connection = store
        .connections()
        .ensure(
            workspace_id,
            ConnectionKind::Storage,
            connection_name,
            None,
            None,
        )
        .await?;
    store
        .buckets()
        .ensure(workspace_id, connection.id, bucket_name, None)
        .await
}

/// Best-effort: catalogue `object` (Postgres truth) under the workspace's bucket
/// so it's queryable (`query_structured`, §6.5) and ingestable (§10). Returns the
/// catalogued object's id **and whether the key was newly created** (`true`) rather
/// than an existing row updated (`false`) — so the caller fires the matching
/// `StorageObject` `"created"`/`"updated"` trigger — or `None` on failure. The blob
/// is already persisted, so a catalogue failure is logged and swallowed — it must
/// never fail a store whose bytes landed (§9).
pub(crate) async fn catalogue_object(
    store: &Store,
    workspace_id: WorkspaceId,
    connection_name: &str,
    bucket_name: &str,
    object: &ObjectMeta,
) -> Option<(ObjectId, bool)> {
    let bucket = match ensure_bucket(store, workspace_id, connection_name, bucket_name).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, key = %object.key,
                "failed to ensure bucket for object catalogue (bytes stored)");
            return None;
        }
    };
    // Whether a catalogue row already existed decides "created" vs "updated" for the
    // trigger; a lookup error (absent row / transient) reads as "new" — a best-effort
    // signal, never worth failing a stored blob over.
    let was_created = store
        .objects()
        .get_by_key(workspace_id, bucket.id, &object.key)
        .await
        .is_err();
    let up = UpsertObject {
        workspace_id,
        bucket_id: bucket.id,
        key: &object.key,
        size: object.size,
        content_type: object.content_type.as_deref(),
        etag: object.etag.as_deref(),
        last_modified: object.last_modified,
        sha256: None,
    };
    match store.objects().upsert(&up).await {
        Ok(stored) => Some((stored.id, was_created)),
        Err(e) => {
            tracing::warn!(error = %e, key = %object.key,
                "failed to catalogue uploaded object (bytes stored)");
            None
        }
    }
}

/// Whether a [`scan_store`] pass fires `StorageObject` automation triggers for the
/// changes it reconciles (SOUL §9/§11). The initial catalogue-population scan (on
/// store creation) runs [`Silent`](ScanEvents::Silent) — the files already sitting
/// on a freshly-attached backend are the *baseline*, not a burst of `created`
/// events — while the watch worker and manual re-scans run [`Fire`](ScanEvents::Fire),
/// so files that genuinely appear, change, or vanish after that baseline head their
/// automations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScanEvents {
    /// Dispatch a `created`/`updated`/`deleted` trigger per reconciled change.
    Fire,
    /// Reconcile the catalogue silently (no triggers) — the store-creation baseline.
    Silent,
}

/// Fire the `StorageObject` automation trigger for `event` (`"created"` /
/// `"updated"` / `"deleted"`) on the user-facing `key` in `bucket`, carrying the
/// object's `content_type` so a matching automation's downstream nodes can key on
/// the file type (SOUL §9/§11). Wholly best-effort: a dispatch failure is logged,
/// never propagated — the storage mutation that prompted it has already committed.
/// Returns how many automations were enqueued (0 on failure).
pub(crate) async fn dispatch_storage_event(
    store_db: &Store,
    workspace_id: WorkspaceId,
    bucket: &str,
    key: &str,
    content_type: Option<&str>,
    event: &str,
) -> usize {
    let ev = catalerum_automation::TriggerEvent::StorageObject {
        event: event.to_string(),
        bucket: bucket.to_string(),
        key: key.to_string(),
        content_type: content_type.map(str::to_string),
    };
    match catalerum_ingest::dispatch_trigger_event(store_db, workspace_id, &ev).await {
        Ok(jobs) => jobs.len(),
        Err(e) => {
            tracing::warn!(error = %e, %key, event,
                "failed to dispatch StorageObject automations");
            0
        }
    }
}

/// Finalise a freshly-`put` object exactly like an upload does (SOUL §9):
/// [`catalogue`](catalogue_object) it (Postgres truth, so it's queryable/searchable),
/// enqueue its §10 ingest, and fire the `StorageObject` `created`/`updated` trigger
/// (by whether the key was new) so it can head a downstream automation. `object`
/// must carry the **user-facing** key (never the physical `<workspace_id>/…`
/// namespaced one, §18). Wholly best-effort — the bytes are already durable, so
/// every step logs-and-continues rather than unwinding a completed store. Shared by
/// [`copy_object_between`], [`write_object_bytes`], and the terminal `store_object`
/// tool.
pub(crate) async fn catalogue_and_notify(
    store_db: &Store,
    workspace_id: WorkspaceId,
    connection: &str,
    bucket: &str,
    object: &ObjectMeta,
) {
    // The returned flag decides created vs updated. A catalogue failure still fires
    // the trigger (the bytes are durable) — as `created`, the safe default when the
    // key's priorness couldn't be determined.
    let was_created =
        match catalogue_object(store_db, workspace_id, connection, bucket, object).await {
            Some((object_id, was_created)) => {
                if let Err(e) =
                    catalerum_ingest::enqueue_ingest_object(store_db, workspace_id, object_id).await
                {
                    tracing::warn!(error = %e, %object_id,
                        "failed to enqueue stored object ingest (object catalogued)");
                }
                was_created
            }
            None => true,
        };
    let event = if was_created { "created" } else { "updated" };
    dispatch_storage_event(
        store_db,
        workspace_id,
        bucket,
        &object.key,
        object.content_type.as_deref(),
        event,
    )
    .await;
}

/// Copy a stored object from one store to another (the `copy_object` tool's core,
/// SOUL §9). Streams the source's bytes straight into the destination — across
/// backends (S3 ↔ local ↔ WebDAV) so no full buffering — then catalogues, enqueues
/// §10 ingest, and fires the `StorageObject` trigger on the destination exactly
/// like an upload, so the copy is queryable/searchable. Each store defaults to the
/// caller's per-user default files store when unnamed (via [`resolve_store`]).
/// Returns the destination object's metadata (the user-facing key).
pub(crate) async fn copy_object_between(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    from: (Option<&str>, &str),
    to: (Option<&str>, &str),
) -> ApiResult<ObjectMeta> {
    let (from_store, from_key) = from;
    let (to_store, to_key) = to;
    validate_object_key(from_key)?;
    validate_object_key(to_key)?;
    let src = resolve_store(registry, store_db, workspace_id, user_id, from_store).await?;
    let dst = resolve_store(registry, store_db, workspace_id, user_id, to_store).await?;
    let src_phys = src.physical_key(workspace_id, from_key);
    let dst_phys = dst.physical_key(workspace_id, to_key);
    if src.store == dst.store && src_phys == dst_phys {
        return Err(ApiError::bad_request(
            "source and destination are the same object",
        ));
    }
    // Confirm the source exists (404 → never a silent empty copy), then stream it
    // into the destination without buffering the whole object in memory.
    let src_meta = src.backend.stat(&src_phys).await.map_err(map_storage_err)?;
    let data = src.backend.get(&src_phys).await.map_err(map_storage_err)?;
    let meta = PutMeta {
        content_type: src_meta.content_type.clone(),
        content_length: Some(src_meta.size),
    };
    dst.backend
        .put(&dst_phys, data, meta)
        .await
        .map_err(map_storage_err)?;
    // Re-stat the destination for the authoritative size/etag, and catalogue the
    // user-facing key (never the physical namespaced one), mirroring `upload`.
    let mut object = dst.backend.stat(&dst_phys).await.map_err(map_storage_err)?;
    object.key = to_key.to_string();
    catalogue_and_notify(
        store_db,
        workspace_id,
        &dst.connection,
        &dst.bucket,
        &object,
    )
    .await;
    Ok(object)
}

/// Read a stored object's full bytes (plus its stored content type) from `store`
/// — the caller's per-user default files store when unnamed — workspace-namespaced
/// exactly like [`download`]. The audio `speech_to_text` tool's byte source (and a
/// reusable byte-level reader for any tool that needs the raw blob, not the §10
/// extracted text `read_object` returns). `404` when the object is absent.
pub(crate) async fn read_object_bytes(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    at: (Option<&str>, &str),
) -> ApiResult<(Vec<u8>, Option<String>)> {
    let (store, key) = at;
    validate_object_key(key)?;
    let storage = resolve_store(registry, store_db, workspace_id, user_id, store).await?;
    let physical = storage.physical_key(workspace_id, key);
    let meta = storage
        .backend
        .stat(&physical)
        .await
        .map_err(map_storage_err)?;
    let mut stream = storage
        .backend
        .get(&physical)
        .await
        .map_err(map_storage_err)?;
    let mut bytes = Vec::with_capacity(meta.size as usize);
    while let Some(chunk) = stream.next().await {
        bytes.extend(chunk.map_err(map_storage_err)?);
    }
    Ok((bytes, meta.content_type))
}

/// Write `bytes` to `store` under `key` (the caller's per-user default files store
/// when unnamed), then catalogue + enqueue §10 ingest + fire the `StorageObject`
/// trigger exactly like [`upload`] — so a synthesized file is queryable/searchable
/// and can head a downstream automation. The audio `text_to_speech` tool's sink.
/// `content_type` is the provider-reported MIME (e.g. `audio/mpeg`); the S3 backend
/// persists it, local/WebDAV re-guess from the key's extension on `stat`.
pub(crate) async fn write_object_bytes(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    at: (Option<&str>, &str),
    bytes: Vec<u8>,
    content_type: Option<String>,
) -> ApiResult<ObjectMeta> {
    let (store, key) = at;
    validate_object_key(key)?;
    let storage = resolve_store(registry, store_db, workspace_id, user_id, store).await?;
    let physical = storage.physical_key(workspace_id, key);
    let meta = PutMeta {
        content_type,
        content_length: Some(bytes.len() as u64),
    };
    let data = stream::once(async move { Ok(bytes) }).boxed();
    storage
        .backend
        .put(&physical, data, meta)
        .await
        .map_err(map_storage_err)?;
    let mut object = storage
        .backend
        .stat(&physical)
        .await
        .map_err(map_storage_err)?;
    // Present + catalogue the user-facing key, never the physical namespaced one.
    object.key = key.to_string();
    catalogue_and_notify(
        store_db,
        workspace_id,
        &storage.connection,
        &storage.bucket,
        &object,
    )
    .await;
    Ok(object)
}

/// Delete one object from a resolved store and reconcile everything that referenced
/// it (SOUL §9/§11): remove the backend blob, then — best-effort and idempotently —
/// drop its catalogue row, enqueue a §10 purge of its extracted text/vectors, fire
/// the `StorageObject` "deleted" trigger, and purge its labels so a tag can't
/// outlive its bytes. Idempotent: the backend `delete` is a no-op on an absent key
/// and the catalogue steps skip a row that was never there, so a repeated delete
/// succeeds quietly (no trigger). The shared core of the
/// `DELETE /storage/objects/{key}` route and the `delete_object` tool. Returns
/// whether the key was actually catalogued (so a batch caller can count real files).
pub(crate) async fn delete_object_at(
    store_db: &Store,
    handle: &StorageHandle,
    workspace_id: WorkspaceId,
    key: &str,
) -> ApiResult<bool> {
    validate_object_key(key)?;
    // Delete only within this workspace's namespace — a tenant can never remove
    // another's blob (SOUL §18); a browse store deletes the raw on-disk key.
    let physical = handle.physical_key(workspace_id, key);
    handle
        .backend
        .delete(&physical)
        .await
        .map_err(map_storage_err)?;

    // Keep the catalogue in sync with the bytes (best-effort, idempotent): drop the
    // object row so a deleted blob doesn't linger in `query_structured`, and enqueue
    // a §10 ingest so the worker **purges** its extracted-text document + vectors
    // (the object is gone → the handler reconciles by purging).
    let mut was_catalogued = false;
    if let Ok(bucket) =
        ensure_bucket(store_db, workspace_id, &handle.connection, &handle.bucket).await
    {
        // Grab the catalogue row before purging it: its id enqueues the §10 purge and
        // its content type rides the `deleted` trigger. Only a key we actually had
        // catalogued fires the trigger, so an idempotent re-DELETE stays quiet.
        let existing = store_db
            .objects()
            .get_by_key(workspace_id, bucket.id, key)
            .await
            .ok();
        if let Err(e) = store_db
            .objects()
            .delete_by_key(workspace_id, bucket.id, key)
            .await
        {
            tracing::warn!(error = %e, %key, "failed to remove object from catalogue (blob deleted)");
        }
        if let Some(obj) = existing {
            was_catalogued = true;
            if let Err(e) =
                catalerum_ingest::enqueue_ingest_object(store_db, workspace_id, obj.id).await
            {
                tracing::warn!(error = %e, object_id = %obj.id, "failed to enqueue object purge (blob deleted)");
            }
            // Fire `StorageObject` "deleted" automations (best-effort; the blob is gone).
            dispatch_storage_event(
                store_db,
                workspace_id,
                &handle.bucket,
                key,
                obj.content_type.as_deref(),
                "deleted",
            )
            .await;
        }
    }

    // Purge any labels on the deleted file so a label can't outlive its bytes
    // (best-effort, idempotent — keyed on the resolved store the file lived on).
    if let Err(e) = store_db
        .object_labels()
        .delete_for_path(workspace_id, &handle.store, key)
        .await
    {
        tracing::warn!(error = %e, %key, "failed to purge labels for deleted object");
    }
    Ok(was_catalogued)
}

/// The zero-byte placeholder written under a directory so an object store shows the
/// (otherwise empty) folder: S3/local/WebDAV have no real directories — a folder
/// exists only as the shared prefix of the keys under it — so `create_directory`
/// writes `<dir>/.keep` and the Files tree synthesizes the folder from that key.
pub(crate) const DIRECTORY_KEEP_FILE: &str = ".keep";

/// Create an (empty) directory on a resolved store by writing its `<dir>/.keep`
/// placeholder straight to the backend (SOUL §9) — the caller's per-user default
/// files store when unnamed. Object stores synthesize a folder from the keys under
/// it, so an empty directory needs at least one object to exist; the marker is
/// written **uncatalogued** (unlike [`write_object_bytes`]) so it never surfaces in
/// search / `query_structured` — it's directory scaffolding, not content. Idempotent:
/// re-creating an existing directory just rewrites the empty marker. Returns the
/// resolved store name and the marker's user-facing key. The `create_directory`
/// tool's core.
pub(crate) async fn create_directory(
    registry: &StorageRegistry,
    store_db: &Store,
    workspace_id: WorkspaceId,
    user_id: Option<UserId>,
    at: (Option<&str>, &str),
) -> ApiResult<(String, String)> {
    let (store, dir) = at;
    let dir = dir.trim_matches('/');
    validate_object_key(dir)?;
    let storage = resolve_store(registry, store_db, workspace_id, user_id, store).await?;
    let marker = format!("{dir}/{DIRECTORY_KEEP_FILE}");
    let physical = storage.physical_key(workspace_id, &marker);
    let data = stream::once(async move { Ok(Vec::<u8>::new()) }).boxed();
    storage
        .backend
        .put(
            &physical,
            data,
            PutMeta {
                content_type: None,
                content_length: Some(0),
            },
        )
        .await
        .map_err(map_storage_err)?;
    Ok((storage.store, marker))
}

/// List the **user-facing** keys of every object under `prefix` on a resolved store,
/// bounded by [`DEFAULT_OBJECT_LIMIT`]; the returned bool is whether that cap was hit
/// (so more objects may exist under the prefix than were listed). Physical
/// `<workspace_id>/…` prefixes are stripped back to the user-facing key via the
/// handle. Shared by the `delete_object` tool's recursive (directory) branch.
pub(crate) async fn list_object_keys(
    handle: &StorageHandle,
    workspace_id: WorkspaceId,
    prefix: &str,
) -> ApiResult<(Vec<String>, bool)> {
    let scoped = handle.physical_key(workspace_id, prefix);
    let stream = handle
        .backend
        .list(&scoped)
        .await
        .map_err(map_storage_err)?;
    let metas: Vec<ObjectMeta> = stream
        .filter_map(|r| async move { r.ok() })
        .take(DEFAULT_OBJECT_LIMIT as usize)
        .collect()
        .await;
    let truncated = metas.len() >= DEFAULT_OBJECT_LIMIT as usize;
    let keys = metas
        .iter()
        .map(|m| handle.user_key(workspace_id, &m.key))
        .collect();
    Ok((keys, truncated))
}

/// Whether a backend object differs from its catalogue row (so it needs
/// (re-)ingesting): a brand-new object (`None`), or one whose size, etag, or
/// last-modified changed. The etag (which the local backend derives from
/// size+mtime) is the primary signal; size/last-modified cover backends without one.
pub(crate) fn object_changed(prev: Option<&StoredObject>, meta: &ObjectMeta) -> bool {
    match prev {
        None => true,
        Some(p) => {
            p.size != meta.size || p.etag != meta.etag || p.last_modified != meta.last_modified
        }
    }
}

/// What a [`scan_store`] pass reconciled.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ScanReport {
    /// Backend objects seen (capped at [`DEFAULT_OBJECT_LIMIT`]).
    pub scanned: usize,
    /// New or changed objects (re-)catalogued and enqueued for §10 ingest.
    pub indexed: usize,
    /// Already-catalogued, unchanged objects (no re-ingest).
    pub unchanged: usize,
    /// Catalogue rows purged because their key no longer exists on the backend
    /// (the file was deleted out-of-band). The blob is **not** touched.
    pub removed: usize,
    /// Whether the backend listing hit [`DEFAULT_OBJECT_LIMIT`] (so deletions past
    /// the cap can't be reconciled this pass — a no-purge guard, see below).
    pub truncated: bool,
}

/// Reconcile a store's **catalogue** (Postgres truth) with its **backend**
/// filesystem (SOUL §9/§10) — the engine behind the manual "Scan / index" action,
/// the auto-scan on store creation, and the [`StorageWatchWorker`](crate::storage_watch).
///
/// Lists the backend (workspace-namespaced, or raw on a browse store, via the
/// `handle`), then for each object: **new or changed** (by size / etag /
/// last-modified) → upsert its catalogue row + enqueue §10 ingest (extract text →
/// embed); **unchanged** → left alone (so a steady-state re-scan does no writes).
/// Any catalogue row under this store's bucket whose key is **gone** from the
/// backend is purged (row deleted + a §10 purge enqueued) — keeping search in sync
/// when a file is removed on disk; the backend blob is never touched.
///
/// When `events` is [`ScanEvents::Fire`], each reconciled change also fires the
/// matching `StorageObject` automation trigger — `created` for a new key, `updated`
/// for a changed one, `deleted` for a purged one (SOUL §11) — so a file dropped on a
/// watched backend out-of-band heads its automation just like an API upload does.
/// The store-creation baseline scan passes [`ScanEvents::Silent`] so a freshly-
/// attached backend's pre-existing files don't each fire a spurious `created`.
///
/// Idempotent and safe to run repeatedly. Bounded by [`DEFAULT_OBJECT_LIMIT`]: if
/// the listing is truncated we **skip** delete-reconciliation (a key beyond the cap
/// would look "missing" and be wrongly purged), reporting `truncated`.
pub(crate) async fn scan_store(
    store: &Store,
    workspace_id: WorkspaceId,
    handle: &StorageHandle,
    prefix: &str,
    events: ScanEvents,
) -> ApiResult<ScanReport> {
    // List the backend under the store's (namespaced or raw) prefix.
    let scoped_prefix = handle.physical_key(workspace_id, prefix);
    let stream = handle
        .backend
        .list(&scoped_prefix)
        .await
        .map_err(|e| ApiError::internal(format!("scan: listing objects: {e}")))?;
    let backend_objs: Vec<ObjectMeta> = stream
        .filter_map(|r| async move { r.ok() })
        .take(DEFAULT_OBJECT_LIMIT as usize)
        .collect()
        .await;
    let truncated = backend_objs.len() >= DEFAULT_OBJECT_LIMIT as usize;

    // Ensure the catalogue bucket once and load its existing rows so we can detect
    // changes (avoid re-ingesting unchanged files) and reconcile deletions.
    let bucket = ensure_bucket(store, workspace_id, &handle.connection, &handle.bucket)
        .await
        .map_err(|e| ApiError::internal(format!("scan: ensure bucket: {e}")))?;
    let mut existing: HashMap<String, StoredObject> = store
        .objects()
        .list_by_bucket(workspace_id, bucket.id)
        .await
        .map_err(|e| ApiError::internal(format!("scan: listing catalogue: {e}")))?
        .into_iter()
        .map(|o| (o.key.clone(), o))
        .collect();

    let mut report = ScanReport {
        truncated,
        ..ScanReport::default()
    };
    for m in backend_objs {
        // The user-facing key (the physical `<ws>/…` prefix stripped, or raw on a
        // browse store) — what the catalogue keys on.
        let key = handle.user_key(workspace_id, &m.key);
        report.scanned += 1;
        // Absent row ⇒ `created`; present-but-changed (size/etag/mtime) ⇒ `updated`;
        // present-and-unchanged ⇒ nothing to do. `remove` also marks the key present
        // so it survives the delete pass below.
        let event = match existing.remove(&key) {
            None => "created",
            Some(prev) if object_changed(Some(&prev), &m) => "updated",
            Some(_) => {
                report.unchanged += 1;
                continue;
            }
        };
        let up = UpsertObject {
            workspace_id,
            bucket_id: bucket.id,
            key: &key,
            size: m.size,
            content_type: m.content_type.as_deref(),
            etag: m.etag.as_deref(),
            last_modified: m.last_modified,
            sha256: None,
        };
        match store.objects().upsert(&up).await {
            Ok(stored) => {
                report.indexed += 1;
                if let Err(e) =
                    catalerum_ingest::enqueue_ingest_object(store, workspace_id, stored.id).await
                {
                    tracing::warn!(error = %e, %key, "scan: failed to enqueue ingest (catalogued)");
                }
                if events == ScanEvents::Fire {
                    dispatch_storage_event(
                        store,
                        workspace_id,
                        &handle.bucket,
                        &key,
                        m.content_type.as_deref(),
                        event,
                    )
                    .await;
                }
            }
            Err(e) => tracing::warn!(error = %e, %key, "scan: failed to catalogue object"),
        }
    }

    // Whatever remains in `existing` was on the backend before but is gone now →
    // purge it from the catalogue + §10 index (the blob, if any, is left alone).
    // Skipped on a truncated listing: a key beyond the cap would look missing.
    if !truncated {
        for (key, obj) in existing {
            match store
                .objects()
                .delete_by_key(workspace_id, bucket.id, &key)
                .await
            {
                Ok(()) => {
                    report.removed += 1;
                    if let Err(e) =
                        catalerum_ingest::enqueue_ingest_object(store, workspace_id, obj.id).await
                    {
                        tracing::warn!(error = %e, %key, "scan: failed to enqueue purge (row removed)");
                    }
                    // Purge any labels on the vanished file (best-effort) so a label
                    // can't outlive its bytes — same cleanup as the DELETE route.
                    if let Err(e) = store
                        .object_labels()
                        .delete_for_path(workspace_id, &handle.store, &key)
                        .await
                    {
                        tracing::warn!(error = %e, %key, "scan: failed to purge labels (row removed)");
                    }
                    if events == ScanEvents::Fire {
                        dispatch_storage_event(
                            store,
                            workspace_id,
                            &handle.bucket,
                            &key,
                            obj.content_type.as_deref(),
                            "deleted",
                        )
                        .await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, %key, "scan: failed to purge vanished object"),
            }
        }
    }
    Ok(report)
}
