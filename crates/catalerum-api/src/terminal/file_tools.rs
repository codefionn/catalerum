//! Object and workdir file tools (`stage_object`, `store_object`,
//! `read_file`, `create_file`, `edit_file`).

use super::*;

/// `stage_object` — copy a stored file from a files store **into** a terminal
/// session's working directory (the inverse of `persist_terminal`, SOUL §9/§20).
/// Holds the [`StorageRegistry`] + [`Store`] so it can resolve *any* source store
/// the same way the `/storage` routes do — including the caller's per-user default
/// — so a file on a different backend than the terminal's folder (e.g. an S3 files
/// store vs. a local workdir) can be pulled in. Gated on `exec:run` like the rest
/// of the terminal surface; only registered when an object store exists.
pub(super) struct StageObjectTool {
    pub(super) manager: Arc<TerminalManager>,
    pub(super) storage: StorageRegistry,
    pub(super) store: Store,
}

impl StageObjectTool {
    /// Stage the optional `files` and recursive `directories` supplied directly
    /// to `open_terminal`. Per-item failures are returned alongside successes so
    /// a typo never hides/leaks the already-open live session. Directory expansion
    /// is bounded across the whole request to avoid accidentally copying a store.
    pub(super) async fn stage_open_requests(
        &self,
        args: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Json {
        let mut results = Vec::new();
        let mut staged_count = 0usize;

        if let Some(files) = args.get("files") {
            match files.as_array() {
                Some(files) => {
                    for spec in files {
                        if staged_count >= MAX_OPEN_STAGE_FILES {
                            results.push(json!({
                                "ok": false,
                                "kind": "limit",
                                "error": format!("open_terminal stages at most {MAX_OPEN_STAGE_FILES} files")
                            }));
                            break;
                        }
                        let mut result = match self.stage_one(spec, ws, id, ctx).await {
                            Ok(value) => value,
                            Err(error) => json!({ "ok": false, "error": error.to_string() }),
                        };
                        if result.get("ok").and_then(Json::as_bool) == Some(true) {
                            staged_count += 1;
                        }
                        if let Some(object) = result.as_object_mut() {
                            object.insert("kind".into(), json!("file"));
                        }
                        results.push(result);
                    }
                }
                None => results.push(json!({
                    "ok": false,
                    "kind": "files",
                    "error": "`files` must be an array"
                })),
            }
        }

        if let Some(directories) = args.get("directories") {
            match directories.as_array() {
                Some(directories) => {
                    for spec in directories {
                        if staged_count >= MAX_OPEN_STAGE_FILES {
                            results.push(json!({
                                "ok": false,
                                "kind": "limit",
                                "error": format!("open_terminal stages at most {MAX_OPEN_STAGE_FILES} files")
                            }));
                            break;
                        }
                        match self
                            .stage_open_directory(
                                spec,
                                ws,
                                id,
                                ctx,
                                MAX_OPEN_STAGE_FILES - staged_count,
                            )
                            .await
                        {
                            Ok(mut directory_results) => {
                                staged_count += directory_results
                                    .iter()
                                    .filter(|item| {
                                        item.get("ok").and_then(Json::as_bool) == Some(true)
                                    })
                                    .count();
                                results.append(&mut directory_results);
                            }
                            Err(error) => results.push(json!({
                                "ok": false,
                                "kind": "directory",
                                "prefix": spec.get("prefix").cloned().unwrap_or(Json::Null),
                                "error": error.to_string()
                            })),
                        }
                    }
                }
                None => results.push(json!({
                    "ok": false,
                    "kind": "directories",
                    "error": "`directories` must be an array"
                })),
            }
        }

        json!({
            "requested": args.get("files").is_some() || args.get("directories").is_some(),
            "staged_files": staged_count,
            "results": results,
        })
    }

    /// Expand one store prefix and stage every object while preserving its path
    /// below `dest_dir` (or below the source prefix's final directory name).
    async fn stage_open_directory(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
        remaining: usize,
    ) -> Result<Vec<Json>> {
        let prefix = spec
            .get("prefix")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.trim_matches('/').is_empty())
            .ok_or_else(|| Error::invalid("`directories[].prefix` must be non-empty"))?;
        let source_prefix = format!("{}/", prefix.trim_matches('/'));
        let store_name = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let default_dest = source_prefix
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default();
        let dest_dir = spec
            .get("dest_dir")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(default_dest)
            .trim_matches('/');
        if dest_dir.is_empty() {
            return Err(Error::invalid("`directories[].dest_dir` must be non-empty"));
        }

        let handle = crate::object_service::resolve_store(
            &self.storage,
            &self.store,
            ws,
            ctx.user_id,
            store_name,
        )
        .await
        .map_err(|error| Error::other(error.to_string()))?;
        let physical_prefix = handle.physical_key(ws, &source_prefix);
        let mut stream = handle.backend.list(&physical_prefix).await?;
        let mut keys = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item?;
            keys.push(handle.user_key(ws, &meta.key));
            if keys.len() > remaining {
                return Err(Error::invalid(format!(
                    "directory `{source_prefix}` exceeds the remaining open_terminal staging limit of {remaining} files"
                )));
            }
        }
        if keys.is_empty() {
            return Err(Error::invalid(format!(
                "no stored files under directory `{source_prefix}`"
            )));
        }
        keys.sort();

        let mut results = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(relative) = key.strip_prefix(&source_prefix) else {
                continue;
            };
            if relative.is_empty() {
                continue;
            }
            let dest_path = format!("{dest_dir}/{relative}");
            let request = json!({
                "key": key,
                "store": handle.store,
                "dest_path": dest_path,
            });
            let mut result = match self.stage_one(&request, ws, id, ctx).await {
                Ok(value) => value,
                Err(error) => json!({
                    "ok": false,
                    "key": request.get("key"),
                    "path": request.get("dest_path"),
                    "error": error.to_string()
                }),
            };
            if let Some(object) = result.as_object_mut() {
                object.insert("kind".into(), json!("directory_file"));
                object.insert("source_prefix".into(), json!(source_prefix));
            }
            results.push(result);
        }
        Ok(results)
    }

    /// Resolve one source store and stream a single object (`{key, store?,
    /// dest_path?}`) into the already-opened session at `id`. Shared by the
    /// single-object form and each entry of the batch (`items`) form so both take
    /// the same fields and defaulting (unnamed store → per-user default, `dest_path`
    /// → the key's filename).
    async fn stage_one(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Result<Json> {
        let key = spec
            .get("key")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`key` is required"))?;
        let store = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let dest_path = spec
            .get("dest_path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // A session owned by a peer pod stages there (SOUL §16 M7): the workdir
        // lives on the owner's filesystem, and the object store is shared, so the
        // owner resolves the store + streams the bytes itself.
        if let Some(pod) = self.manager.remote_owner(ws, id).await {
            return self
                .manager
                .forward_remote(
                    &pod,
                    ws,
                    id,
                    PodOp::StageObject {
                        store: store.map(str::to_string),
                        key: key.to_string(),
                        dest_path: dest_path.map(str::to_string),
                        user_id: ctx.user_id,
                    },
                )
                .await;
        }
        stage_object_via(
            &self.manager,
            &self.storage,
            &self.store,
            ws,
            id,
            ctx.user_id,
            store,
            key,
            dest_path,
        )
        .await
    }
}

/// Resolve one source store and stream a single object into the session at
/// `id` — the body shared by the `stage_object` tool and the owner side of a
/// forwarded [`PodOp::StageObject`], so both paths default identically (unnamed
/// store → the caller's per-user default; `dest_path` → the key's filename).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_object_via(
    manager: &TerminalManager,
    storage: &StorageRegistry,
    store_db: &Store,
    ws: WorkspaceId,
    id: TerminalSessionId,
    user_id: Option<catalerum_core::UserId>,
    store: Option<&str>,
    key: &str,
    dest_path: Option<&str>,
) -> Result<Json> {
    let dest_path = dest_path
        .map(str::to_string)
        .unwrap_or_else(|| key.rsplit('/').next().unwrap_or(key).to_string());
    // Resolve the source store the same way an HTTP handler does (honouring the
    // caller's per-user default when `store` is omitted), then stream its bytes
    // into the session workdir.
    let handle = crate::object_service::resolve_store(storage, store_db, ws, user_id, store)
        .await
        .map_err(|e| Error::other(e.to_string()))?;
    let physical = handle.physical_key(ws, key);
    let bytes = manager
        .stage_object(ws, id, handle.backend.as_ref(), &physical, &dest_path)
        .await?;
    Ok(json!({
        "ok": true,
        "path": dest_path,
        "bytes": bytes,
        "store": handle.store,
        "key": key,
        "session_id": id,
        "next_tools": ["terminal_write", "terminal_read"],
        "usage_note": "Run commands against this path with terminal_write in this same session, then collect output with terminal_read. run_command cannot see this terminal workdir.",
    }))
}

#[async_trait]
impl Tool for StageObjectTool {
    fn name(&self) -> &str {
        "stage_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Copy a stored file from a files store into a terminal session's working \
         directory so the shell can operate on it — the inverse of persist_terminal. \
         Use when a file lives in object storage (e.g. a chat upload) but the \
         terminal works on a local folder. `key` is the store-relative object key \
         (from an attachment reference or query_structured); omit `store` to use \
         your default files store. `dest_path` defaults to the key's filename. \
         Returns the bytes written and the workdir path. Stage several files into the \
         same session at once by passing an `items` array instead of the top-level \
         key/store/dest_path fields. After staging, operate on the path with \
         terminal_write and terminal_read using this same `session_id`; run_command \
         has a different working-directory context and cannot see session files."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "key": { "type": "string", "description": "Object key within the store (store-relative). Single-stage form; omit when using `items`." },
                "store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                "dest_path": { "type": "string", "description": "Destination path relative to the session workdir; omitted → the key's filename." },
                "items": {
                    "type": "array",
                    "description": "Batch form: stage several objects into this same session in one call. Each entry takes the same key/store/dest_path fields as a single stage. When present, the top-level key/store/dest_path fields are ignored and the result is a `results` array (one entry per item, in order, each carrying an `ok` flag — failures don't abort the rest).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "key": { "type": "string", "description": "Object key within the store (store-relative)." },
                            "store": { "type": "string", "description": "Source store name; omitted → your default files store." },
                            "dest_path": { "type": "string", "description": "Destination path relative to the session workdir; omitted → the key's filename." }
                        },
                        "required": ["key"]
                    }
                }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        // Batch form: `items` stages each spec into this session in order, reporting
        // per-item success so a bad entry surfaces without aborting (or hiding) the
        // others — staging is not transactional across items.
        if let Some(items) = args.get("items").and_then(Json::as_array) {
            if items.is_empty() {
                return Err(Error::invalid("`items` must not be empty"));
            }
            let mut results = Vec::with_capacity(items.len());
            for (index, spec) in items.iter().enumerate() {
                let entry = match self.stage_one(spec, ws, id, ctx).await {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
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
        self.stage_one(&args, ws, id, ctx).await
    }
}

/// `store_object` — copy a file **out of** a terminal session's working directory
/// into a files store (the inverse of `stage_object`, SOUL §9/§20). Like
/// [`StageObjectTool`] it holds the [`StorageRegistry`] + [`Store`] so it can resolve
/// *any* destination store the same way the `/storage` routes do (including the
/// caller's per-user default), then catalogues the written object + fires the
/// `StorageObject` trigger exactly like an upload so the file is queryable and can
/// head a downstream automation. Gated on `exec:run`; only registered with a store.
pub(super) struct StoreObjectTool {
    pub(super) manager: Arc<TerminalManager>,
    pub(super) storage: StorageRegistry,
    pub(super) store: Store,
}

impl StoreObjectTool {
    /// Resolve one destination store and stream a single workdir file (`{path,
    /// store?, key?}`) out of the session at `id` into it. Shared by the single-file
    /// and batch (`items`) forms so both take the same fields and defaulting (unnamed
    /// store → per-user default, `key` → the source path's filename).
    async fn store_one(
        &self,
        spec: &Json,
        ws: WorkspaceId,
        id: TerminalSessionId,
        ctx: &ToolContext,
    ) -> Result<Json> {
        let path = spec
            .get("path")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`path` is required"))?;
        let store = spec
            .get("store")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let key = spec
            .get("key")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // A session owned by a peer pod stores from there (SOUL §16 M7): the
        // workdir file lives on the owner's filesystem, and the object store is
        // shared, so the owner resolves the store + streams the bytes itself.
        if let Some(pod) = self.manager.remote_owner(ws, id).await {
            return self
                .manager
                .forward_remote(
                    &pod,
                    ws,
                    id,
                    PodOp::StoreObject {
                        store: store.map(str::to_string),
                        key: key.map(str::to_string),
                        path: path.to_string(),
                        user_id: ctx.user_id,
                    },
                )
                .await;
        }
        store_object_via(
            &self.manager,
            &self.storage,
            &self.store,
            ws,
            id,
            ctx.user_id,
            store,
            key,
            path,
        )
        .await
    }
}

/// Resolve one destination store and stream a single workdir file out of the
/// session at `id` into it — the body shared by the `store_object` tool and the
/// owner side of a forwarded [`PodOp::StoreObject`], so both paths default
/// identically (unnamed store → the caller's per-user default; `key` → the
/// source path's filename).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store_object_via(
    manager: &TerminalManager,
    storage: &StorageRegistry,
    store_db: &Store,
    ws: WorkspaceId,
    id: TerminalSessionId,
    user_id: Option<catalerum_core::UserId>,
    store: Option<&str>,
    key: Option<&str>,
    path: &str,
) -> Result<Json> {
    let key = key
        .map(str::to_string)
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(path).to_string());
    // Resolve the destination store the same way an HTTP handler does (honouring
    // the caller's per-user default when `store` is omitted), stream the workdir
    // file into it, then catalogue + notify exactly like an upload.
    let handle = crate::object_service::resolve_store(storage, store_db, ws, user_id, store)
        .await
        .map_err(|e| Error::other(e.to_string()))?;
    let physical = handle.physical_key(ws, &key);
    let bytes = manager
        .store_object(ws, id, handle.backend.as_ref(), &physical, path)
        .await?;
    // Re-stat for the authoritative size/etag/content-type and catalogue the
    // user-facing key (never the physical namespaced one). Best-effort: the bytes
    // are already stored, so a stat/catalogue failure is logged, not surfaced.
    match handle.backend.stat(&physical).await {
        Ok(mut object) => {
            object.key = key.clone();
            crate::object_service::catalogue_and_notify(
                store_db,
                ws,
                &handle.connection,
                &handle.bucket,
                &object,
            )
            .await;
        }
        Err(e) => {
            tracing::warn!(error = %e, key = %key,
                "failed to stat stored object for catalogue (bytes stored)");
        }
    }
    Ok(json!({
        "ok": true,
        "path": path,
        "bytes": bytes,
        "store": handle.store,
        "key": key,
    }))
}

#[async_trait]
impl Tool for StoreObjectTool {
    fn name(&self) -> &str {
        "store_object"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Copy a file out of a terminal session's working directory into a files store \
         — the inverse of stage_object. Use to hand a file the shell produced (a \
         build artifact, a generated report) back to object storage so it's \
         catalogued, searchable, and can head a downstream automation. `path` is the \
         file's path relative to the session workdir; omit `store` to use your \
         default files store; `key` defaults to the source path's filename. Returns \
         the bytes written and the destination key. Store several files out of the \
         same session at once by passing an `items` array instead of the top-level \
         path/store/key fields."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "Source file path relative to the session workdir. Single-store form; omit when using `items`." },
                "store": { "type": "string", "description": "Destination store name; omitted → your default files store." },
                "key": { "type": "string", "description": "Destination object key within the store (store-relative); omitted → the source path's filename." },
                "items": {
                    "type": "array",
                    "description": "Batch form: store several workdir files out of this same session in one call. Each entry takes the same path/store/key fields as a single store. When present, the top-level path/store/key fields are ignored and the result is a `results` array (one entry per item, in order, each carrying an `ok` flag — failures don't abort the rest).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": { "type": "string", "description": "Source file path relative to the session workdir." },
                            "store": { "type": "string", "description": "Destination store name; omitted → your default files store." },
                            "key": { "type": "string", "description": "Destination object key within the store (store-relative); omitted → the source path's filename." }
                        },
                        "required": ["path"]
                    }
                }
            },
            "required": ["session_id"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        // Batch form: `items` stores each spec out of this session in order, reporting
        // per-item success so a bad entry surfaces without aborting (or hiding) the
        // others — storing is not transactional across items.
        if let Some(items) = args.get("items").and_then(Json::as_array) {
            if items.is_empty() {
                return Err(Error::invalid("`items` must not be empty"));
            }
            let mut results = Vec::with_capacity(items.len());
            for (index, spec) in items.iter().enumerate() {
                let entry = match self.store_one(spec, ws, id, ctx).await {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
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
        // Single form.
        self.store_one(&args, ws, id, ctx).await
    }
}

/// `read_file` — read a text file from a terminal session's working directory.
pub(super) struct ReadFileTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Read a text file from a terminal session's working directory. `path` is \
         relative to that directory (the `host_dir` from open_terminal). Window a \
         large file with `offset` (1-based start line) and `limit` (max lines). \
         Returns the file `content` plus `total_lines`, on-disk `size`, and a \
         `truncated` flag. Prefer this over `cat` in terminal_write — it returns \
         the text directly instead of racing the PTY output."
    }
    fn description_for(&self, input_modalities: &[String]) -> String {
        read_file_description(self.description(), input_modalities)
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory." },
                "offset": { "type": "integer", "description": "1-based line to start from (optional)." },
                "limit": { "type": "integer", "description": "Max lines to return (optional)." }
            },
            "required": ["session_id", "path"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        self.invoke_for_model(args, ctx, &[]).await
    }
    async fn invoke_for_model(
        &self,
        args: Json,
        ctx: &ToolContext,
        input_modalities: &[String],
    ) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let offset = args
            .get("offset")
            .and_then(Json::as_u64)
            .map(|n| n as usize)
            .filter(|n| *n > 0);
        let limit = args.get("limit").and_then(Json::as_u64).map(|n| n as usize);
        if let Some(content_type) = supported_media_path(path, input_modalities) {
            if offset.is_some() || limit.is_some() {
                return Err(Error::invalid(
                    "`offset`/`limit` cannot be used when ingesting binary media",
                ));
            }
            let media = self.manager.read_media_file(ws, id, path).await?;
            let encoded = b64(&media.bytes);
            let input = MediaInput::Image {
                url: format!("data:{content_type};base64,{encoded}"),
            };
            return Ok(json!({
                "path": path,
                "content_type": content_type,
                "size": media.size,
                "ingested": true,
                MODEL_MEDIA_RESULT_FIELD: [input],
            }));
        }
        let r = self.manager.read_file(ws, id, path, offset, limit).await?;
        Ok(json!({
            "content": r.content,
            "total_lines": r.total_lines,
            "size": r.size,
            "truncated": r.truncated,
        }))
    }
}

pub(super) fn supported_media_path(path: &str, input_modalities: &[String]) -> Option<&'static str> {
    let content_type = mime_guess::from_path(path).first_raw()?;
    (content_type.starts_with("image/")
        && input_modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("image")))
    .then_some(content_type)
}

pub(super) fn read_file_description(base: &str, input_modalities: &[String]) -> String {
    let supports_images = input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"));
    if !supports_images {
        return format!(
            "{base} Binary files are rejected; native binary ingestion is unavailable for the \
             active model through llmleaf."
        );
    }
    format!(
        "{base} Binary files are rejected by default. Because the active model accepts image input, \
         a recognized image file is instead ingested natively through llmleaf and attached to the \
         next model turn; do not use `offset`/`limit` for images.",
    )
}

/// `create_file` — write a text file into a terminal session's working directory.
pub(super) struct CreateFileTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for CreateFileTool {
    fn name(&self) -> &str {
        "create_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Create (or overwrite) a text file in a terminal session's working \
         directory, creating any parent directories. `path` is relative to that \
         directory. Use this to author code/config the terminal will run, instead \
         of heredocs in terminal_write. Returns the bytes written and whether an \
         existing file was overwritten. Use edit_file for targeted changes."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory (parent dirs are created)." },
                "content": { "type": "string", "description": "The file's full contents." }
            },
            "required": ["session_id", "path", "content"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let content = args
            .get("content")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`content` is required"))?;
        let (bytes_written, overwrote) = self.manager.write_file(ws, id, path, content).await?;
        Ok(json!({
            "ok": true,
            "path": path,
            "bytes_written": bytes_written,
            "created": !overwrote,
        }))
    }
}

/// `edit_file` — replace text in an existing file in a session's working directory.
pub(super) struct EditFileTool {
    pub(super) manager: Arc<TerminalManager>,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn required_capability(&self) -> Option<Capability> {
        exec_cap()
    }
    fn description(&self) -> &str {
        "Replace text in an existing file in a terminal session's working \
         directory. `old_string` must appear exactly once unless `replace_all` is \
         true (then every occurrence is replaced). `path` is relative to the \
         working directory. Returns the number of replacements. Use create_file to \
         write a whole file."
    }
    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "From open_terminal." },
                "path": { "type": "string", "description": "File path relative to the session's working directory." },
                "old_string": { "type": "string", "description": "Exact text to replace (must be unique unless replace_all)." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
            },
            "required": ["session_id", "path", "old_string", "new_string"]
        })
    }
    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let ws = workspace(ctx)?;
        let id = session_id_arg(&args)?;
        let path = path_arg(&args)?;
        let old = args
            .get("old_string")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`old_string` is required"))?;
        let new = args
            .get("new_string")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`new_string` is required"))?;
        let replace_all = args
            .get("replace_all")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let replacements = self
            .manager
            .edit_file(ws, id, path, old, new, replace_all)
            .await?;
        Ok(json!({ "ok": true, "path": path, "replacements": replacements }))
    }
}
