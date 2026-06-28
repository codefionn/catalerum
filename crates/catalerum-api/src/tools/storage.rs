//! Object-storage file tools: copy/download-link/delete/mkdir/write/move + read/search objects.

use super::*;

/// `copy_object` — copy a stored file from one files store to another (SOUL §9).
/// Registered whenever storage is configured; also reachable from Boa automation
/// code nodes via `catalerum.callTool("copy_object", …)` (the registry is the same
/// one [`CodeToolHost`](crate::action_runner) dispatches against). Holds the
/// [`StorageRegistry`] + [`Store`] so it can resolve each store (config or runtime,
/// honouring the caller's default) the same way the `/storage` routes do.
pub(crate) struct CopyObjectTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
}

impl CopyObjectTool {
    /// Resolve and run a single copy spec (`{from_key, from_store?, to_key?,
    /// to_store?}`), returning the destination object's metadata. Shared by the
    /// single-object form and each entry of the batch (`items`) form so both take
    /// exactly the same fields and defaulting (unnamed store → per-user default,
    /// `to_key` → `from_key`).
    async fn copy_one(&self, spec: &Json, ws: WorkspaceId, ctx: &ToolContext) -> Result<Json> {
        let str_arg = |k: &str| {
            spec.get(k)
                .and_then(Json::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        };
        let from_key =
            str_arg("from_key").ok_or_else(|| Error::invalid("`from_key` is required"))?;
        let to_key = str_arg("to_key").unwrap_or(from_key);
        let object = crate::routes::storage::copy_object_between(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (str_arg("from_store"), from_key),
            (str_arg("to_store"), to_key),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        Ok(json!({
            "key": object.key,
            "size": object.size,
            "content_type": object.content_type,
            "etag": object.etag,
        }))
    }
}

#[async_trait]
impl Tool for CopyObjectTool {
    fn name(&self) -> &str {
        "copy_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        // Single-capability dispatch: gate on the side-effecting `storage:write`
        // (the destination put), the same scope the upload route requires.
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Copy a stored file from one files store to another (e.g. from an S3 store \
         into a local store the terminal can reach, or to relocate a key). Streams \
         across backends — no size limit beyond the stores'. Omit `from_store`/\
         `to_store` to use your default files store; `to_key` defaults to `from_key`. \
         The copy is catalogued and ingested like an upload (so it's searchable). To \
         instead pull a file straight into a terminal workdir, use stage_object. Copy \
         several files at once by passing an `items` array instead of the top-level \
         from_key/… fields."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "from_key": { "type": "string", "description": "Source object key (store-relative). Single-copy form; omit when using `items`." },
                "from_store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                "to_key": { "type": "string", "description": "Destination object key; omitted → same as from_key." },
                "to_store": { "type": "string", "description": "Destination store name; omitted → your default files store." },
                "items": {
                    "type": "array",
                    "description": "Batch form: copy several objects in one call. Each entry takes the same from_key/from_store/to_key/to_store fields as a single copy. When present, the top-level from_key/… fields are ignored and the result is a `results` array (one entry per item, in order, each carrying an `ok` flag — failures don't abort the rest).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "from_key": { "type": "string", "description": "Source object key (store-relative)." },
                            "from_store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                            "to_key": { "type": "string", "description": "Destination object key; omitted → same as from_key." },
                            "to_store": { "type": "string", "description": "Destination store name; omitted → your default files store." }
                        },
                        "required": ["from_key"]
                    }
                }
            }
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        // Batch form: `items` copies each spec in order, reporting per-item success
        // so a bad entry surfaces without aborting (or hiding) the others — the copy
        // is not transactional across items.
        if let Some(items) = args.get("items").and_then(Json::as_array) {
            if items.is_empty() {
                return Err(Error::invalid("`items` must not be empty"));
            }
            let mut results = Vec::with_capacity(items.len());
            for (index, spec) in items.iter().enumerate() {
                let entry = match self.copy_one(spec, ws, ctx).await {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("ok".into(), Json::Bool(true));
                            obj.insert("index".into(), json!(index));
                        }
                        v
                    }
                    Err(e) => json!({ "ok": false, "index": index, "error": e.to_string() }),
                };
                results.push(entry);
            }
            return Ok(json!({ "results": results }));
        }
        // Single form (unchanged shape for existing callers).
        self.copy_one(&args, ws, ctx).await
    }
}

/// Register `copy_object` (SOUL §9). Called from `build_registry` whenever the
/// config storage registry is non-empty (runtime stores can still be targeted by
/// name through the resolver).
pub(crate) fn register_copy_object_tool(
    registry: &mut ToolRegistry,
    storage: StorageRegistry,
    store: Store,
) {
    registry.register(Arc::new(CopyObjectTool { storage, store }));
}

/// Default lifetime of a minted download link (1 hour) — long enough to click,
/// short enough that a leaked URL is stale fast (SOUL §9/§19).
pub(crate) const DEFAULT_DOWNLOAD_TTL_SECS: u64 = 60 * 60;
/// Floor on a link's lifetime — clamp anything shorter up so a link is always
/// clickable for at least a minute.
pub(crate) const MIN_DOWNLOAD_TTL_SECS: u64 = 60;
/// Ceiling on a link's lifetime (7 days) — an unauthenticated link should never be
/// effectively permanent.
pub(crate) const MAX_DOWNLOAD_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// `download_link` — mint a signed, short-lived URL the user can click to download a
/// stored file (or a whole directory as a `.tar.gz`), no login needed (SOUL §9).
/// The link points at the public `GET /download/{token}` route; the token is an
/// HMAC-signed claim naming exactly one workspace + store + key + expiry, so it
/// grants read of that one thing for a short window and nothing else (§18/§19).
/// Holds the [`StorageRegistry`] + [`Store`] to resolve + probe the object the same
/// way `/storage` does, the [`DownloadSigner`] to mint, and the API base URL the
/// link renders against. Boa-callable via the shared registry, like `copy_object`.
pub(crate) struct DownloadLinkTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
    pub(crate) signer: DownloadSigner,
    /// The API's public base URL (no trailing slash) links are rendered against.
    pub(crate) base_url: String,
}

impl DownloadLinkTool {
    /// Confirm a directory link's prefix names at least one object and isn't too
    /// large to archive, returning `(total_bytes, object_count)`. Lists the store
    /// under `<prefix>/` (a trailing slash, so a sibling prefix like `reports2/`
    /// can't bleed in) bounded by [`DEFAULT_OBJECT_LIMIT`], mirroring the redeem
    /// route's own guard so a link is never handed out for something that will fail
    /// to download.
    async fn measure_dir(
        &self,
        ws: WorkspaceId,
        handle: &crate::state::StorageHandle,
        prefix_slash: &str,
    ) -> Result<(u64, usize)> {
        use futures::StreamExt;
        let scoped = handle.physical_key(ws, prefix_slash);
        let stream = handle
            .backend
            .list(&scoped)
            .await
            .map_err(|e| Error::other(format!("listing `{prefix_slash}`: {e}")))?;
        let metas: Vec<_> = stream
            .filter_map(|r| async move { r.ok() })
            .take(catalerum_store::DEFAULT_OBJECT_LIMIT as usize)
            .collect()
            .await;
        if metas.is_empty() {
            return Err(Error::invalid(format!(
                "no file or directory at `{}`",
                prefix_slash.trim_end_matches('/')
            )));
        }
        let total: u64 = metas.iter().map(|m| m.size).sum();
        let max = crate::routes::download::MAX_ARCHIVE_BYTES;
        if total > max {
            return Err(Error::invalid(format!(
                "directory `{}` is too large to archive ({total} bytes > {max} limit)",
                prefix_slash.trim_end_matches('/')
            )));
        }
        Ok((total, metas.len()))
    }
}

#[async_trait]
impl Tool for DownloadLinkTool {
    fn name(&self) -> &str {
        "download_link"
    }
    fn required_capability(&self) -> Option<Capability> {
        // Read-only: a link only ever lets its holder *read* the one object/prefix.
        cap(Action::Read, "storage")
    }
    fn description(&self) -> &str {
        "Generate a short-lived, shareable download URL for a stored file — or a \
         whole directory, delivered as a `.tar.gz`. Use this to hand the user a link \
         they can click to download a file you created or found (e.g. after writing a \
         report with the terminal or copy_object). The link needs no login and \
         expires (default 1 hour). Give the `key` of the file (store-relative); a key \
         ending in `/`, or one that names a folder rather than a file, is archived as \
         a directory. Omit `store` to use your default files store. Returns the `url`, \
         its `kind` (file/directory), and `expires_at`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Object key (store-relative) of the file to link, or a directory prefix to archive. A key ending in `/` is always treated as a directory." },
                "store": { "type": "string", "description": "Store name; omitted → your default files store." },
                "ttl_secs": { "type": "integer", "description": "How long the link stays valid, in seconds. Default 3600 (1 hour); clamped to [60, 604800] (max 7 days)." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let raw = required_str(&args, "key")?;
        let store_name = opt_str_some(&args, "store");
        let ttl = opt_clamped_u64(
            &args,
            "ttl_secs",
            DEFAULT_DOWNLOAD_TTL_SECS,
            MAX_DOWNLOAD_TTL_SECS,
        )
        .max(MIN_DOWNLOAD_TTL_SECS);

        // Resolve the store now so the claim can pin its *resolved* name — the redeem
        // route has no acting user, so it can't re-run per-user default resolution.
        let handle = crate::routes::storage::resolve_store(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            store_name.as_deref(),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;

        let looks_dir = raw.ends_with('/');
        let trimmed = raw.trim_end_matches('/').to_string();
        if trimmed.is_empty() {
            return Err(Error::invalid("`key` must not be empty"));
        }

        // A file unless the key ends in `/` or turns out to name a prefix rather than
        // an object. `claim_key` is what the token carries: the bare key for a file,
        // or `<prefix>/` (trailing slash) for a directory so the redeem route scopes
        // its listing to exactly that folder.
        let (dir, size, count, claim_key) = if looks_dir {
            let prefix_slash = format!("{trimmed}/");
            let (total, n) = self.measure_dir(ws, &handle, &prefix_slash).await?;
            (true, total, n, prefix_slash)
        } else {
            crate::routes::storage::validate_object_key(&trimmed)
                .map_err(|e| Error::invalid(e.to_string()))?;
            let physical = handle.physical_key(ws, &trimmed);
            match handle.backend.stat(&physical).await {
                Ok(meta) => (false, meta.size, 1usize, trimmed.clone()),
                Err(Error::NotFound) => {
                    // Not a file — try it as a directory prefix before giving up.
                    let prefix_slash = format!("{trimmed}/");
                    let (total, n) = self.measure_dir(ws, &handle, &prefix_slash).await?;
                    (true, total, n, prefix_slash)
                }
                Err(e) => return Err(Error::other(format!("checking `{trimmed}`: {e}"))),
            }
        };

        let exp = chrono::Utc::now().timestamp() + ttl as i64;
        let claims = DownloadClaims {
            workspace_id: ws,
            store: Some(handle.store.clone()),
            key: claim_key,
            dir,
            exp,
        };
        let token = self.signer.mint(&claims);
        let url = format!("{}/download/{token}", self.base_url);
        let expires_at = chrono::DateTime::<chrono::Utc>::from_timestamp(exp, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default();
        Ok(json!({
            "url": url,
            "kind": if dir { "directory" } else { "file" },
            "key": trimmed,
            "store": handle.store,
            "size": size,
            "object_count": count,
            "expires_at": expires_at,
        }))
    }
}

/// Register `download_link` (SOUL §9). Called from `build_registry`'s storage block
/// whenever object storage is configured. Threads the signer + API base URL from
/// [`AppState`](crate::state::AppState) so the tool can mint links the public
/// `GET /download/{token}` route verifies.
pub(crate) fn register_download_link_tool(
    registry: &mut ToolRegistry,
    storage: StorageRegistry,
    store: Store,
    signer: DownloadSigner,
    base_url: String,
) {
    registry.register(Arc::new(DownloadLinkTool {
        storage,
        store,
        signer,
        base_url,
    }));
}

/// `delete_object` — remove a stored file, or a whole directory and everything under
/// it (SOUL §9). Deleting is a `storage:write` op (consistent with the API gating no
/// handler on `Delete`), reconciling the catalogue + §10 index + labels + firing the
/// `StorageObject` "deleted" trigger for each removed file through the shared
/// [`delete_object_at`](crate::routes::storage::delete_object_at) core the DELETE
/// route uses. Holds the [`StorageRegistry`] + [`Store`] to resolve the target store
/// (config or runtime, honouring the caller's default) exactly like `copy_object`.
pub(crate) struct DeleteObjectTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for DeleteObjectTool {
    fn name(&self) -> &str {
        "delete_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        // Deleting is a write op — the same scope the DELETE route + `copy_object` require.
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Delete a stored file — or an entire directory and everything under it — from \
         a files store. Give the object `key` (store-relative). By default exactly one \
         file is removed; to delete a whole folder, end the `key` in `/` or pass \
         `recursive: true`, and every file under that prefix is deleted. Omit `store` \
         to use your default files store. Idempotent: deleting a file that isn't there \
         still succeeds. Returns `kind` (file/directory) and `deleted` (how many files \
         were removed); a directory delete also returns `truncated: true` when it hit \
         the 1000-file cap and more remain — call again to finish."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Object key (store-relative) of the file to delete, or a directory prefix to delete recursively. A key ending in `/` is always treated as a directory." },
                "store": { "type": "string", "description": "Store name; omitted → your default files store." },
                "recursive": { "type": "boolean", "description": "Delete a whole directory: every file under `key`/. Also implied when `key` ends in `/`. Default false (delete a single file)." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let raw = required_str(&args, "key")?;
        let store_name = opt_str_some(&args, "store");
        let recursive = args
            .get("recursive")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        // A trailing `/` names a directory (like `download_link`), as does the explicit
        // flag; either way the bare prefix is what we scope the listing to.
        let looks_dir = raw.ends_with('/');
        let key = raw.trim_matches('/').to_string();
        if key.is_empty() {
            return Err(Error::invalid("`key` must not be empty"));
        }
        let handle = crate::routes::storage::resolve_store(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            store_name.as_deref(),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;

        if recursive || looks_dir {
            // Directory delete: list every object under `<prefix>/` (a trailing slash so
            // a sibling prefix like `reports2/` can't bleed in) and remove each. Bounded
            // at 1000 files per call; `truncated` tells the caller to repeat for the rest.
            let prefix_slash = format!("{key}/");
            let (keys, truncated) =
                crate::routes::storage::list_object_keys(&handle, ws, &prefix_slash)
                    .await
                    .map_err(|e| Error::other(e.to_string()))?;
            if keys.is_empty() {
                return Err(Error::invalid(format!(
                    "no directory at `{key}` (nothing to delete)"
                )));
            }
            let mut deleted = 0usize;
            let mut errors = Vec::new();
            for k in &keys {
                match crate::routes::storage::delete_object_at(&self.store, &handle, ws, k).await {
                    Ok(_) => deleted += 1,
                    Err(e) => errors.push(json!({ "key": k, "error": e.to_string() })),
                }
            }
            return Ok(json!({
                "kind": "directory",
                "key": key,
                "store": handle.store,
                "deleted": deleted,
                "truncated": truncated,
                "errors": errors,
            }));
        }

        // Single file (idempotent — an absent key still succeeds).
        crate::routes::storage::delete_object_at(&self.store, &handle, ws, &key)
            .await
            .map_err(|e| Error::other(e.to_string()))?;
        Ok(json!({ "kind": "file", "key": key, "store": handle.store, "deleted": 1 }))
    }
}

/// `create_directory` — make a new (empty) directory in a files store (SOUL §9).
/// Object stores have no real directories (a folder is only the shared prefix of the
/// keys inside it), so this writes a hidden `.keep` placeholder — **uncatalogued**,
/// via [`create_directory`](crate::routes::storage::create_directory) — to give an
/// otherwise-empty folder a presence in the Files tree. Holds the [`StorageRegistry`]
/// + [`Store`] to resolve the target store like `copy_object`/`delete_object`.
pub(crate) struct CreateDirectoryTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for CreateDirectoryTool {
    fn name(&self) -> &str {
        "create_directory"
    }
    fn required_capability(&self) -> Option<Capability> {
        // Writing the placeholder is a `storage:write` op, like `copy_object`.
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Create a new (empty) directory in a files store. Give the directory `path` \
         (store-relative, e.g. `reports/2026`); parent folders are implied. Omit \
         `store` to use your default files store. Object stores have no real \
         directories — a folder exists only as the shared prefix of the files inside \
         it — so this writes a hidden `.keep` placeholder to give the empty folder a \
         presence in the Files tree; the placeholder is uncatalogued, so it never \
         shows up in search. Idempotent. You do NOT need this just to write a file into \
         a folder — writing `dir/name` creates the folder implicitly; use it only to \
         make an empty directory. Returns the created `path` and its `marker` key."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to create (store-relative), e.g. `reports/2026`. Trailing slashes are ignored; parent folders are implied." },
                "store": { "type": "string", "description": "Store name; omitted → your default files store." }
            },
            "required": ["path"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let raw = required_str(&args, "path")?;
        let store_name = opt_str_some(&args, "store");
        let dir = raw.trim_matches('/').to_string();
        if dir.is_empty() {
            return Err(Error::invalid("`path` must not be empty"));
        }
        let (store, marker) = crate::routes::storage::create_directory(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (store_name.as_deref(), &dir),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        Ok(json!({ "path": dir, "store": store, "marker": marker }))
    }
}

/// Register the storage file tools — `delete_object`, `create_directory`,
/// `write_object`, `move_object` (SOUL §9/§11). Called from the storage block
/// (alongside `copy_object`) whenever object storage is configured; runtime stores
/// stay reachable by name through the resolver. All gate on `storage:write` and are
/// Boa-callable via the shared registry, so an automation code node can
/// `catalerum.callTool("write_object", …)`; `write_object`/`move_object` also back
/// the `WriteObject`/`MoveObject` automation actions.
pub(crate) fn register_storage_file_tools(
    registry: &mut ToolRegistry,
    storage: StorageRegistry,
    store: Store,
) {
    registry.register(Arc::new(DeleteObjectTool {
        storage: storage.clone(),
        store: store.clone(),
    }));
    registry.register(Arc::new(CreateDirectoryTool {
        storage: storage.clone(),
        store: store.clone(),
    }));
    registry.register(Arc::new(WriteObjectTool {
        storage: storage.clone(),
        store: store.clone(),
    }));
    registry.register(Arc::new(MoveObjectTool { storage, store }));
}

/// Cap on the bytes a single `write_object` call may create (post-base64-decode).
/// A tool arg is model- or automation-authored content, not a file upload — for
/// big blobs use the upload route / `copy_object` / a terminal.
pub(crate) const MAX_WRITE_OBJECT_BYTES: usize = 16 * 1024 * 1024;

/// `write_object` — write (create or overwrite) a stored file from content in
/// hand (SOUL §9/§11): the generic file-write the registry previously lacked
/// (§29 "Per-App durable state" recorded the gap). Text goes in `content`,
/// binary in `content_base64`; the write is catalogued + §10-ingested + fires
/// the `StorageObject` trigger exactly like an upload, via the shared
/// [`write_object_bytes`](crate::routes::storage::write_object_bytes) core (the
/// `text_to_speech` sink). Idempotent by `(store, key)` — a re-run overwrites
/// the same object — which is why the `WriteObject` automation action is safe
/// to re-run on a collect redelivery (SOUL §11/§29).
pub(crate) struct WriteObjectTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for WriteObjectTool {
    fn name(&self) -> &str {
        "write_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        // Same side-effecting scope as the upload route + `copy_object`.
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Write a stored file: create it (or overwrite an existing one) from content \
         you have in hand. Put text in `content`, or binary as `content_base64`. \
         Omit `store` to use your default files store; folders in `key` are implied \
         (`reports/2026/summary.md`). The file is catalogued and ingested like an \
         upload, so it becomes searchable and can trigger storage automations. \
         Overwrites are idempotent by key. To copy an EXISTING stored file use \
         copy_object; to relocate one use move_object."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "key": { "type": "string", "description": "Object key (store-relative) to write, e.g. `reports/2026/summary.md`. Parent folders are implied." },
                "content": { "type": "string", "description": "Text content to write (UTF-8). Mutually exclusive with `content_base64`." },
                "content_base64": { "type": "string", "description": "Binary content, base64-encoded. Mutually exclusive with `content`." },
                "store": { "type": "string", "description": "Store name; omitted → your default files store." },
                "content_type": { "type": "string", "description": "MIME type to record (e.g. `text/markdown`); omitted → guessed from the key's extension." }
            },
            "required": ["key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let key = required_str(&args, "key")?;
        let bytes = match (args.get("content"), args.get("content_base64")) {
            (Some(_), Some(_)) => {
                return Err(Error::invalid(
                    "`content` and `content_base64` are mutually exclusive",
                ))
            }
            (Some(text), None) => text
                .as_str()
                .ok_or_else(|| Error::invalid("`content` must be a string"))?
                .as_bytes()
                .to_vec(),
            (None, Some(b64)) => {
                let b64 = b64
                    .as_str()
                    .ok_or_else(|| Error::invalid("`content_base64` must be a string"))?;
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(b64.trim())
                    .map_err(|e| Error::invalid(format!("invalid `content_base64`: {e}")))?
            }
            (None, None) => {
                return Err(Error::invalid(
                    "one of `content` or `content_base64` is required",
                ))
            }
        };
        if bytes.len() > MAX_WRITE_OBJECT_BYTES {
            return Err(Error::invalid(format!(
                "content is {} bytes; write_object caps at {} — upload big files instead",
                bytes.len(),
                MAX_WRITE_OBJECT_BYTES
            )));
        }
        let object = crate::routes::storage::write_object_bytes(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (opt_str_some(&args, "store").as_deref(), &key),
            bytes,
            opt_str_some(&args, "content_type"),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        Ok(json!({
            "key": object.key,
            "size": object.size,
            "content_type": object.content_type,
            "etag": object.etag,
        }))
    }
}

/// `move_object` — relocate a stored file (SOUL §9/§11): a
/// [`copy_object_between`](crate::routes::storage::copy_object_between) to the
/// destination, then a [`delete_object_at`](crate::routes::storage::delete_object_at)
/// of the source (object stores have no rename primitive). Both halves reconcile the
/// catalogue/index like their standalone tools, so search follows the file. Gated
/// `storage:write` like `copy_object`/`delete_object`: a move never destroys content
/// (it lands at the destination first), while plain `storage:write` can already
/// overwrite any object — so it confers nothing new. NOT redelivery-idempotent (a
/// re-run finds the source gone), so the `MoveObject` automation action auto-skips
/// on a collect redelivery (SOUL §11/§29).
pub(crate) struct MoveObjectTool {
    pub(crate) storage: StorageRegistry,
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for MoveObjectTool {
    fn name(&self) -> &str {
        "move_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Write, "storage")
    }
    fn description(&self) -> &str {
        "Move (rename/relocate) a stored file: copy it to the destination, then \
         delete the source. Works across stores (S3 ↔ local ↔ WebDAV). Omit \
         `from_store`/`to_store` to use your default files store; `to_key` defaults \
         to `from_key` (for a pure store-to-store move). To keep the source, use \
         copy_object instead."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "from_key": { "type": "string", "description": "Source object key (store-relative)." },
                "from_store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                "to_key": { "type": "string", "description": "Destination object key; omitted → same as from_key (move between stores)." },
                "to_store": { "type": "string", "description": "Destination store name; omitted → your default files store." }
            },
            "required": ["from_key"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let from_key = required_str(&args, "from_key")?;
        let from_store = opt_str_some(&args, "from_store");
        let to_store = opt_str_some(&args, "to_store");
        let to_key = opt_str_some(&args, "to_key").unwrap_or_else(|| from_key.clone());
        // Land the copy first (`copy_object_between` refuses a same-object move,
        // so the delete below can never be deleting the freshly-written copy).
        let object = crate::routes::storage::copy_object_between(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            (from_store.as_deref(), &from_key),
            (to_store.as_deref(), &to_key),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        // Then remove the source. A failure here is surfaced as an error — but the
        // copy HAS landed, so the message says so and a retry (the delete is
        // idempotent, the copy overwrites in place) completes the move.
        let src = crate::routes::storage::resolve_store(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            from_store.as_deref(),
        )
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        crate::routes::storage::delete_object_at(&self.store, &src, ws, &from_key)
            .await
            .map_err(|e| {
                Error::other(format!(
                    "moved: the copy landed at `{to_key}`, but deleting the source \
                     `{from_key}` failed ({e}) — retry to finish the move"
                ))
            })?;
        Ok(json!({
            "from_key": from_key,
            "key": object.key,
            "size": object.size,
            "content_type": object.content_type,
            "etag": object.etag,
            "moved": true,
        }))
    }
}

/// Map every catalogued bucket in a workspace to `(bucket name, store name)` —
/// resolving `bucket → connection → store` so object results can carry both the
/// bucket label and the `?store=` selector the file lives on, which is also what
/// object labels key on (SOUL §9). Config stores map their connection back to
/// the store name; runtime stores use the connection name verbatim — so with no
/// registry in hand (`storage: None`) the connection-name fallback is exact for
/// runtime stores and all there is otherwise. The tool-side twin of
/// `routes::storage::bucket_labels` (which delegates here).
pub(crate) async fn bucket_store_map(
    store: &Store,
    storage: Option<&StorageRegistry>,
    workspace_id: WorkspaceId,
) -> Result<std::collections::HashMap<BucketId, (String, String)>> {
    let buckets = store
        .buckets()
        .list_by_workspace(workspace_id)
        .await
        .map_err(query_err)?;
    let connections = store
        .connections()
        .list_by_workspace(workspace_id)
        .await
        .map_err(query_err)?;
    let connection_names: std::collections::HashMap<_, _> =
        connections.into_iter().map(|c| (c.id, c.name)).collect();
    let store_for_connection = |conn_name: &str| -> String {
        storage
            .and_then(|reg| {
                reg.infos()
                    .into_iter()
                    .find(|(s, _)| reg.get(s).is_some_and(|cs| cs.connection == conn_name))
                    .map(|(s, _)| s)
            })
            .unwrap_or_else(|| conn_name.to_string())
    };
    Ok(buckets
        .into_iter()
        .map(|b| {
            let conn = connection_names
                .get(&b.connection_id)
                .map(String::as_str)
                .unwrap_or("");
            (b.id, (b.name, store_for_connection(conn)))
        })
        .collect())
}

/// A compact stored-object view for tool results: the object plus its bucket +
/// store names (resolved via `bucket_index`), so the model sees *where* a file
/// lives, not ids — `store` + `key` is the `(store, path)` pair the labels REST
/// targets — and its `labels` (from the page's batched fetch), so a Condition
/// can gate on has-any-tag / has-specific-tag.
pub(crate) fn object_summary(
    o: catalerum_core::model::StoredObject,
    bucket_index: &std::collections::HashMap<BucketId, (String, String)>,
    labels: &std::collections::HashMap<(String, String), Vec<String>>,
) -> Json {
    let (bucket, store) = bucket_index.get(&o.bucket_id).cloned().unwrap_or_default();
    let object_labels = labels
        .get(&(store.clone(), o.key.clone()))
        .cloned()
        .unwrap_or_default();
    json!({
        "id": o.id,
        "bucket": bucket,
        "store": store,
        "key": o.key,
        "size": o.size,
        "content_type": o.content_type,
        "last_modified": o.last_modified,
        "labels": object_labels,
    })
}

/// `read_object` — read a stored file's full **extracted text** by object id
/// (SOUL §9/§10). The id comes from `query_structured` (recent_objects /
/// objects_by_prefix) or `search_semantic`. Complements `search_semantic` (which
/// returns matched *snippets*): this returns the whole document, for summarizing
/// or analyzing one specific file — the object counterpart to `read_note`. An
/// object with no extracted text (binary, or not yet processed) returns an empty
/// `text` with `has_text:false`. Gated `storage:read` (mirrors `GET /storage/objects`).
pub(crate) struct ReadObjectTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for ReadObjectTool {
    fn name(&self) -> &str {
        "read_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "storage")
    }
    fn description(&self) -> &str {
        "Read a stored file's full extracted text by its object id (the `id` from \
         query_structured recent_objects/objects_by_prefix or a search hit). Use to \
         summarize or analyze one specific document; use search_semantic to find \
         relevant passages across many."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Object id (a UUID from query_structured or a search hit)." }
            },
            "required": ["id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id: ObjectId = parse_id(&args, "id")?;
        // Confirm the object exists in this workspace (NotFound otherwise — never
        // leaks another tenant's blob); then pull its §10 extracted-text document.
        let object = self.store.objects().get(ws, id).await?;
        let doc = self
            .store
            .documents()
            .get_by_source(ws, &SourceRef::Object { id })
            .await?;
        let (text, truncated) = cap_read_text(doc.as_ref().map(|d| d.text.as_str()).unwrap_or(""));
        Ok(json!({
            "id": id,
            "key": object.key,
            "content_type": object.content_type,
            "size": object.size,
            "has_text": doc.is_some(),
            "text": text,
            "truncated": truncated,
            "summary": doc.as_ref().and_then(|d| d.summary.clone()),
        }))
    }
}

/// `search_files` (SOUL §7/§9/§10) — literal full-text search over stored files'
/// §10 extracted text; the agent-tool counterpart of the Files panel's content
/// search, and the literal complement to `search_semantic`'s by-meaning search.
/// Thin store client (`ObjectRepo::search_text_in_workspace`), gated on
/// `storage:read` (deny-by-default §19), workspace-scoped.
pub(crate) struct SearchObjectsTool {
    pub(crate) store: Store,
}

#[async_trait]
impl Tool for SearchObjectsTool {
    fn name(&self) -> &str {
        "search_files"
    }
    fn required_capability(&self) -> Option<Capability> {
        cap(Action::Read, "storage")
    }
    fn description(&self) -> &str {
        "Find stored files by the exact text inside them — a literal, \
         case-insensitive substring search over each file's extracted text. Use \
         it for an exact string (an error code, an invoice/order number, a quoted \
         phrase) that semantic search would blur; use search_semantic to find \
         documents by meaning. Each hit gives the object id (pass to read_object \
         for the full text), the file key, content type, and a short excerpt \
         around the match. Only ingested files (those with extracted text) match. \
         This does NOT search filenames or object keys; to find a stored path such \
         as `finance/finance.db`, use query_structured with `objects_by_prefix`."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Exact text to find inside files (case-insensitive substring; %/_ are literal, not wildcards)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (1-50, default 10).",
                    "minimum": 1,
                    "maximum": 50
                }
            },
            "required": ["query"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let query = required_str(&args, "query")?;
        let limit = opt_clamped_u64(&args, "limit", 10, 50) as i64;
        let hits = self
            .store
            .objects()
            .search_text_in_workspace(ws, &query, limit)
            .await
            .map_err(|e| Error::provider(format!("file search failed: {e}")))?;
        let results: Vec<Json> = hits
            .into_iter()
            .map(|h| {
                json!({
                    "id": h.id,
                    "key": h.key,
                    "content_type": h.content_type,
                    "excerpt": h.excerpt,
                })
            })
            .collect();
        Ok(json!({ "results": results }))
    }
}
