//! The file/object half of [`TerminalManager`]: workdir sync, `persist`,
//! `stage_object`/`store_object`, and the workdir file reads/writes/edits.

use super::*;

impl TerminalManager {
    /// Mirror a directory to object storage under `prefix`, returning the
    /// user-facing keys written. Backs the `persist` (ephemeral flush).
    pub async fn sync_dir(
        &self,
        workspace_id: WorkspaceId,
        dir: &Path,
        prefix: &str,
    ) -> Result<Vec<String>> {
        let storage = self
            .storage
            .as_ref()
            .ok_or_else(|| Error::invalid("object storage is not configured"))?;
        let phys_prefix = workspace_object_key(workspace_id, prefix);
        let physical =
            catalerum_storage::sync_dir_to_backend(dir, storage.backend.as_ref(), &phys_prefix)
                .await?;
        Ok(physical
            .iter()
            .map(|k| strip_workspace_key(workspace_id, k))
            .collect())
    }

    /// Resolve where a session file op must run — against the live in-memory
    /// handle's host directory, or on the owning peer pod (whose filesystem
    /// holds the workdir) when forwarding is configured. Shared by
    /// [`persist`](Self::persist) and the workdir file tools.
    async fn file_route(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
    ) -> Result<FileRoute> {
        // Hot path (untouched): a live in-memory handle on this pod holds the real
        // working dir. A local/sandbox/container session on a host path carries
        // `Some(dir)`; a sandboxed container/k8s session keeps its files inside
        // the container and instead records its in-container workdir (`cwd`) —
        // reachable only through the sandbox copy channel ([`FileRoute::InSandbox`]).
        let live_handle = {
            let guard = self.live.read().await;
            guard
                .get(&id)
                .filter(|lr| lr.workspace_id == workspace_id)
                .map(|lr| {
                    (
                        lr.session.host_dir.clone(),
                        lr.sandboxed,
                        lr.session.cwd.clone(),
                    )
                })
        };
        if let Some((host_dir, sandboxed, cwd)) = live_handle {
            if let Some(dir) = host_dir {
                return Ok(FileRoute::Local(PathBuf::from(dir)));
            }
            if sandboxed {
                if let Some(cwd) = cwd {
                    return Ok(FileRoute::InSandbox { cwd });
                }
            }
            return Err(no_host_dir());
        }

        // No live handle on this pod. Do NOT fall through to the durable row's
        // `host_dir`: it names a path on the pod that opened the session, so a
        // foreign-pod row would aim these file ops at another pod's filesystem and
        // die with a confusing raw I/O error. Consult the row: a row owned by a
        // different pod routes there (forwarded when comms are up, else the precise
        // session-affinity error); otherwise its process is gone from this pod.
        let row = self
            .store
            .terminal_sessions()
            .get(workspace_id, id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| Error::invalid("unknown terminal session"))?;
        match row.pod_id {
            Some(pod) if pod != self.pod_id => {
                if self.forwarder().is_some() {
                    Ok(FileRoute::Remote(pod))
                } else {
                    Err(Error::invalid(format!(
                        "terminal session is owned by another pod (`{pod}`); its files live on \
                         that pod, so this request must be routed to it (enable session affinity)"
                    )))
                }
            }
            _ => Err(Error::invalid(
                "terminal session is no longer live on this pod (its process has ended)",
            )),
        }
    }

    /// Flush a session's working dir (or, with `source_subdir`, just that subdir of
    /// it) to object storage under `prefix` (the on-demand ephemeral persist,
    /// SOUL §20). Returns the user-facing keys.
    ///
    /// Each written file is then **catalogued + notified** exactly like an upload
    /// (`catalogue_and_notify`): it becomes a queryable object (with a durable
    /// ObjectId) and fires a per-file `StorageObject` trigger (created / updated),
    /// so a `git clone` in the terminal → persist here can head a downstream
    /// index/de-index automation without waiting on the periodic storage watch.
    pub async fn persist(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        prefix: &str,
        source_subdir: Option<&str>,
    ) -> Result<Vec<String>> {
        let host_dir = match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => dir,
            FileRoute::InSandbox { .. } => return Err(no_host_dir()),
            // The owner holds the files and shares the object store — it uploads,
            // catalogues + notifies exactly as if the call had landed there.
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::Persist {
                            prefix: prefix.to_string(),
                            source_subdir: source_subdir.map(str::to_string),
                        },
                    )
                    .await?;
                return result
                    .get("keys")
                    .and_then(Json::as_array)
                    .map(|keys| {
                        keys.iter()
                            .filter_map(Json::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .ok_or_else(|| Error::provider("malformed forwarded persist result"));
            }
        };
        // A subdir narrows the copy to one folder of the workdir (e.g. a cloned
        // repo's `docs/`), guarded against `..`/absolute escapes like the file tools.
        let dir = match source_subdir.map(str::trim).filter(|s| !s.is_empty()) {
            Some(sub) => resolve_in_dir(&host_dir, sub)?,
            None => host_dir,
        };
        let keys = self.sync_dir(workspace_id, &dir, prefix).await?;

        // Catalogue + fire a per-file trigger for each written key. Best-effort per
        // file: the bytes are already durable, so a stat/catalogue failure is logged,
        // not surfaced (mirrors `StoreObjectTool::store_one`).
        if let Some(storage) = self.storage.as_ref() {
            for key in &keys {
                let physical = workspace_object_key(workspace_id, key);
                match storage.backend.stat(&physical).await {
                    Ok(mut object) => {
                        object.key = key.clone();
                        crate::object_service::catalogue_and_notify(
                            &self.store,
                            workspace_id,
                            &storage.connection,
                            &storage.bucket,
                            &object,
                        )
                        .await;
                    }
                    Err(e) => tracing::warn!(error = %e, key = %key,
                        "failed to stat persisted file for catalogue (bytes stored)"),
                }
            }
        }

        let _ = self
            .store
            .terminal_sessions()
            .set_sync_prefix(workspace_id, id, prefix)
            .await;
        Ok(keys)
    }

    /// A fresh host temp path under the ephemeral root for staging copies
    /// to/from container-backed sessions (the caller removes it when done).
    async fn staging_file(&self) -> Result<PathBuf> {
        let dir = self.ephemeral_root.join(".staging");
        ensure_dir(&dir).await?;
        Ok(dir.join(TerminalSessionId::new().to_string()))
    }

    /// Stream a stored object's bytes from `backend` (at the already-namespaced
    /// `physical_key`) into a session's working directory at `dest_path` — the
    /// inverse of [`persist`](Self::persist) (SOUL §9/§20). The caller resolves the
    /// source store, so a file living on a *different* backend than the terminal's
    /// folder (e.g. an S3 files store vs. a local workdir) lands where the shell can
    /// work on it. `dest_path` is workdir-relative (no absolute / `..`); parent dirs
    /// are created. Works for **every** backend: a session with a host workdir is
    /// written directly (streamed chunk-by-chunk); a container-backed session is
    /// staged through a host temp file and copied in over the sandbox's exec
    /// channel. Returns the byte count written.
    pub async fn stage_object(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        backend: &dyn StorageBackend,
        physical_key: &str,
        dest_path: &str,
    ) -> Result<u64> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, dest_path)?;
                if let Some(parent) = file.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_err("create parent dir for", &file, e))?;
                }
                download_object_to(backend, physical_key, &file).await
            }
            // The default for container-backed sessions: no host dir, so stage
            // via a temp file + the sandbox copy channel.
            FileRoute::InSandbox { cwd } => {
                let dest = in_container_path(&cwd, dest_path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    let total = download_object_to(backend, physical_key, &tmp).await?;
                    sandbox.copy_in(workspace_id, &tmp, &dest).await?;
                    Ok(total)
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => Err(Error::invalid(format!(
                "terminal session is owned by another pod (`{pod}`); its files live on that \
                 pod, so this request must be routed to it"
            ))),
        }
    }

    /// Stream a file **out of** a session's working directory into `backend` at the
    /// already-namespaced `physical_key` — the inverse of
    /// [`stage_object`](Self::stage_object) (SOUL §9/§20). Where [`persist`](Self::persist)
    /// flushes the whole workdir under a prefix, this hands a shell a way to push a
    /// *single* file it produced back to a files store (which the caller resolves, so
    /// it can land on a different backend than the terminal's folder). `src_path` is
    /// workdir-relative (no absolute / `..`). Streamed chunk-by-chunk so a large file
    /// doesn't buffer whole. Returns the byte count written; errors if `src_path` is
    /// missing or a directory.
    pub async fn store_object(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        backend: &dyn StorageBackend,
        physical_key: &str,
        src_path: &str,
    ) -> Result<u64> {
        let file = match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, src_path)?;
                let meta = tokio::fs::metadata(&file)
                    .await
                    .map_err(|e| io_err("stat", &file, e))?;
                if meta.is_dir() {
                    return Err(Error::invalid(format!(
                        "`{src_path}` is a directory, not a file"
                    )));
                }
                file
            }
            // Container-backed session: copy the file out over the sandbox exec
            // channel into a temp file, then upload that (removed below).
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, src_path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                if let Err(e) = sandbox.copy_out(workspace_id, &src, &tmp).await {
                    let _ = tokio::fs::remove_file(&tmp).await;
                    return Err(e);
                }
                tmp
            }
            FileRoute::Remote(pod) => {
                return Err(Error::invalid(format!(
                    "terminal session is owned by another pod (`{pod}`); its files live on \
                     that pod, so this request must be routed to it"
                )))
            }
        };
        let staged = file.starts_with(self.ephemeral_root.join(".staging"));
        let res = async {
            let size = tokio::fs::metadata(&file)
                .await
                .map_err(|e| io_err("stat", &file, e))?
                .len();
            let handle = tokio::fs::File::open(&file)
                .await
                .map_err(|e| io_err("open", &file, e))?;
            // Content type is left unset here; the backend re-guesses it from the key's
            // extension on the re-stat the caller does (local/WebDAV), mirroring an upload.
            let put_meta = PutMeta {
                content_type: None,
                content_length: Some(size),
            };
            backend
                .put(physical_key, file_chunk_stream(handle), put_meta)
                .await?;
            Ok(size)
        }
        .await;
        if staged {
            let _ = tokio::fs::remove_file(&file).await;
        }
        res
    }

    /// Read a text file from a session's working directory (`read_file` tool).
    /// `path` is relative to the workdir (no absolute / `..` segments). An
    /// optional 1-based `offset` line + `limit` line count window a large file.
    /// The returned content is bounded ([`MAX_READ_FILE_BYTES`]); `truncated`
    /// flags either that byte cap or a file past the hard read ceiling. Works
    /// for **every** backend: a host workdir is read directly; a
    /// container-backed session's file is staged out through the sandbox copy
    /// channel first (mirroring `store_object`'s transport).
    pub async fn read_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<ReadFile> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                read_file_windowed(&file, path, offset, limit).await
            }
            // Files live inside the sandbox: copy the file out to a staging temp
            // file over the exec channel, then window it exactly like a host file
            // (the whole file is staged — the same disk-not-memory bound
            // `store_object` already accepts on this route).
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    read_file_windowed(&tmp, path, offset, limit).await
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::ReadFile {
                            path: path.to_string(),
                            offset: offset.map(|v| v as u64),
                            limit: limit.map(|v| v as u64),
                        },
                    )
                    .await?;
                forwarded_read_file(&result)
            }
        }
    }

    /// Read one complete image file for native model input. Unlike
    /// [`read_file`](Self::read_file), this preserves bytes and refuses files
    /// over the hard read ceiling rather than sending a corrupt prefix.
    pub async fn read_media_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
    ) -> Result<ReadMediaFile> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                read_media_file_bounded(&file, path).await
            }
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    read_media_file_bounded(&tmp, path).await
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::ReadMediaFile {
                            path: path.to_string(),
                        },
                    )
                    .await?;
                let bytes = result
                    .get("content_b64")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::provider("malformed forwarded read_media_file result"))
                    .and_then(from_b64)?;
                Ok(ReadMediaFile {
                    bytes,
                    size: result.get("size").and_then(Json::as_u64).unwrap_or(0),
                })
            }
        }
    }

    /// Create or overwrite a text file in a session's working directory
    /// (`create_file` tool), creating any intermediate directories. Returns the
    /// byte count written and whether an existing file was overwritten. Works
    /// for **every** backend: a host workdir is written directly; a
    /// container-backed session's file rides the sandbox copy channel
    /// (`stage_object`'s transport) via a staging temp file.
    pub async fn write_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        content: &str,
    ) -> Result<(u64, bool)> {
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                if let Some(parent) = file.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| io_err("create parent dir for", &file, e))?;
                }
                let overwrote = tokio::fs::metadata(&file).await.is_ok();
                tokio::fs::write(&file, content.as_bytes())
                    .await
                    .map_err(|e| io_err("write", &file, e))?;
                Ok((content.len() as u64, overwrote))
            }
            FileRoute::InSandbox { cwd } => {
                let dest = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                // Probe existence first for the created-vs-overwrote flag (the
                // copy-in script always truncates). The path is a positional,
                // never spliced into the script; best-effort — a probe hiccup
                // reports `created`, it doesn't block the write.
                let overwrote = sandbox
                    .run(
                        workspace_id,
                        CommandSpec {
                            argv: vec![
                                "sh".into(),
                                "-c".into(),
                                r#"test -e "$1""#.into(),
                                "sh".into(),
                                dest.clone(),
                            ],
                            timeout_secs: Some(10),
                            ..Default::default()
                        },
                    )
                    .await
                    .map(|r| r.exit_code == 0)
                    .unwrap_or(false);
                let tmp = self.staging_file().await?;
                let res = async {
                    tokio::fs::write(&tmp, content.as_bytes())
                        .await
                        .map_err(|e| io_err("write", &tmp, e))?;
                    // copy_in creates the in-container parent dirs itself.
                    sandbox.copy_in(workspace_id, &tmp, &dest).await?;
                    Ok((content.len() as u64, overwrote))
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::WriteFile {
                            path: path.to_string(),
                            content: content.to_string(),
                        },
                    )
                    .await?;
                let bytes = result
                    .get("bytes")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| Error::provider("malformed forwarded write_file result"))?;
                let overwrote = result
                    .get("overwrote")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                Ok((bytes, overwrote))
            }
        }
    }

    /// Replace `old` with `new` in a text file in a session's working directory
    /// (`edit_file` tool). `old` must occur exactly once unless `replace_all`.
    /// Returns the number of replacements made. Refuses a non-UTF-8 or
    /// over-the-ceiling file (a partial read would corrupt it on write-back).
    /// Works for **every** backend: a host workdir is edited in place; a
    /// container-backed session's file is staged out over the sandbox copy
    /// channel, edited on the host, and copied back.
    pub async fn edit_file(
        &self,
        workspace_id: WorkspaceId,
        id: TerminalSessionId,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<usize> {
        if old.is_empty() {
            return Err(Error::invalid("`old_string` must not be empty"));
        }
        if old == new {
            return Err(Error::invalid(
                "`old_string` and `new_string` are identical",
            ));
        }
        match self.file_route(workspace_id, id).await? {
            FileRoute::Local(dir) => {
                let file = resolve_in_dir(&dir, path)?;
                let content = read_text_for_edit(&file).await?;
                let (updated, count) = apply_edit(&content, old, new, replace_all)?;
                tokio::fs::write(&file, updated.as_bytes())
                    .await
                    .map_err(|e| io_err("write", &file, e))?;
                Ok(count)
            }
            // Files live inside the sandbox: stage the file out, apply the same
            // read/replace contract to the host copy, and copy the result back.
            // Not atomic against a concurrent in-container writer — neither is
            // the local path against a concurrent shell.
            FileRoute::InSandbox { cwd } => {
                let src = in_container_path(&cwd, path)?;
                let sandbox = self
                    .sandbox
                    .clone()
                    .ok_or_else(|| Error::invalid("per-workspace sandbox is not configured"))?;
                let tmp = self.staging_file().await?;
                let res = async {
                    sandbox.copy_out(workspace_id, &src, &tmp).await?;
                    let content = read_text_for_edit(&tmp).await?;
                    let (updated, count) = apply_edit(&content, old, new, replace_all)?;
                    tokio::fs::write(&tmp, updated.as_bytes())
                        .await
                        .map_err(|e| io_err("write", &tmp, e))?;
                    sandbox.copy_in(workspace_id, &tmp, &src).await?;
                    Ok(count)
                }
                .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                res
            }
            FileRoute::Remote(pod) => {
                let result = self
                    .forward_remote(
                        &pod,
                        workspace_id,
                        id,
                        PodOp::EditFile {
                            path: path.to_string(),
                            old: old.to_string(),
                            new: new.to_string(),
                            replace_all,
                        },
                    )
                    .await?;
                result
                    .get("replacements")
                    .and_then(Json::as_u64)
                    .map(|n| n as usize)
                    .ok_or_else(|| Error::provider("malformed forwarded edit_file result"))
            }
        }
    }
}
