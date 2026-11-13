//! Filesystem and object helpers shared by the terminal manager and its tools.

use super::*;

/// A [`read_file`](TerminalManager::read_file) result: the (windowed, bounded)
/// file text plus metadata for the model.
pub struct ReadFile {
    /// The file's text — line-windowed if `offset`/`limit` were given, then
    /// capped at [`MAX_READ_FILE_BYTES`].
    pub content: String,
    /// Whether `content` was shortened (byte cap, or a file past the ceiling).
    pub truncated: bool,
    /// Total line count of the (bounded) file, regardless of the window.
    pub total_lines: usize,
    /// The file's size on disk, in bytes.
    pub size: u64,
}

/// Complete binary bytes returned for one native image input.
pub struct ReadMediaFile {
    pub bytes: Vec<u8>,
    pub size: u64,
}

/// Decode a forwarded `Read` op result (`{"output_b64": …}`) back to raw bytes.
pub(super) fn forwarded_read_bytes(result: &Json) -> Result<Vec<u8>> {
    result
        .get("output_b64")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::provider("malformed forwarded read result"))
        .and_then(from_b64)
}

/// Decode a forwarded `ReadFile` op result back into a [`ReadFile`].
pub(super) fn forwarded_read_file(result: &Json) -> Result<ReadFile> {
    let content = result
        .get("content")
        .and_then(Json::as_str)
        .ok_or_else(|| Error::provider("malformed forwarded read_file result"))?;
    Ok(ReadFile {
        content: content.to_string(),
        truncated: result
            .get("truncated")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        total_lines: result
            .get("total_lines")
            .and_then(Json::as_u64)
            .unwrap_or(0) as usize,
        size: result.get("size").and_then(Json::as_u64).unwrap_or(0),
    })
}

/// The largest `read_file` content returned inline (256 KiB) — bounded so a big
/// file can't blow up a tool result / the model's context.
const MAX_READ_FILE_BYTES: usize = 256 * 1024;

/// Hard ceiling on bytes the file tools read from disk (8 MiB): a multi-GB file
/// can't OOM the worker. `read_file` reports such a file `truncated`; `edit_file`
/// refuses it (editing a partial read would corrupt the file on write-back).
const HARD_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Resolve a workdir-relative `path` against a session's host `dir`, rejecting
/// absolute paths and `.`/`..` traversal so a file tool can never escape the
/// session's working directory (the relative-key contract storage enforces, §18).
pub(super) fn resolve_in_dir(dir: &Path, rel: &str) -> Result<PathBuf> {
    use std::path::Component;
    let rel = rel.trim();
    if rel.is_empty() {
        return Err(Error::invalid("`path` must not be empty"));
    }
    let candidate = Path::new(rel);
    if !candidate
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::invalid(
            "`path` must be relative to the terminal's working directory \
             (no '.', '..', or absolute segments)",
        ));
    }
    Ok(dir.join(candidate))
}

/// Join a workdir-relative `path` onto a container-backed session's in-container
/// working dir — [`resolve_in_dir`]'s counterpart for paths that live inside the
/// sandbox. Same containment contract (no absolute / `.` / `..` segments); the
/// result is handed to the sandbox copy channel as a shell *positional*, never
/// spliced into a script, so containment is the only concern (not quoting).
pub(super) fn in_container_path(cwd: &str, path: &str) -> Result<String> {
    use std::path::Component;
    let rel = path.trim();
    if rel.is_empty() {
        return Err(Error::invalid("`path` must not be empty"));
    }
    if !Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(Error::invalid(
            "`path` must be relative to the terminal's working directory \
             (no '.', '..', or absolute segments)",
        ));
    }
    if rel.contains('\n') {
        return Err(Error::invalid("`path` must not contain newlines"));
    }
    Ok(format!("{}/{rel}", cwd.trim_end_matches('/')))
}

/// Stream a stored object's bytes into a local file, returning the byte count —
/// the shared download half of `stage_object` (direct into a host workdir, or
/// into the staging temp file for container-backed sessions).
pub(super) async fn download_object_to(
    backend: &dyn StorageBackend,
    physical_key: &str,
    file: &Path,
) -> Result<u64> {
    let mut stream = backend.get(physical_key).await?;
    let mut out = tokio::fs::File::create(file)
        .await
        .map_err(|e| io_err("create", file, e))?;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        out.write_all(&chunk)
            .await
            .map_err(|e| io_err("write", file, e))?;
    }
    out.flush().await.map_err(|e| io_err("flush", file, e))?;
    Ok(total)
}

/// Stream an open file's bytes in fixed-size chunks as a [`ByteStream`], so pushing
/// a large workdir file into a storage backend (`store_object`) bounds memory to one
/// chunk rather than the whole file — the write-side mirror of the backends' own
/// chunked `get`. An I/O error mid-read surfaces as a stream error.
pub(super) fn file_chunk_stream(file: tokio::fs::File) -> ByteStream {
    use tokio::io::AsyncReadExt;
    /// 64 KiB — the same chunk size the storage backends' `get` uses: large enough
    /// to amortise syscalls, small enough to keep the per-read allocation cheap.
    const CHUNK: usize = 64 * 1024;
    futures::stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0u8; CHUNK];
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| Error::other(format!("reading file for upload: {e}")))?;
        if n == 0 {
            Ok(None) // EOF
        } else {
            buf.truncate(n);
            Ok(Some((buf, file)))
        }
    })
    .boxed()
}

/// The shared on-disk body of [`TerminalManager::read_file`]: stat + bounded
/// read + line-window + byte-cap one file. `file` is either the resolved host
/// workdir path or a staging temp file a sandbox copy was staged out to;
/// `path` is the caller's workdir-relative label (for error messages).
pub(super) async fn read_file_windowed(
    file: &Path,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<ReadFile> {
    let meta = tokio::fs::metadata(file)
        .await
        .map_err(|e| io_err("stat", file, e))?;
    if meta.is_dir() {
        return Err(Error::invalid(format!(
            "`{path}` is a directory, not a file"
        )));
    }
    // Bounded read so a multi-GB file can't OOM the worker; a file past the
    // ceiling is reported truncated (and the on-disk size still reflects it).
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    let full = text_file_content(bytes, path, over_cap)?;
    let total_lines = full.lines().count();
    // Window by line if asked (1-based offset); else the whole (bounded) file.
    let windowed: String = match (offset, limit) {
        (None, None) => full,
        (off, lim) => {
            let start = off.unwrap_or(1).saturating_sub(1);
            let mut it = full.lines().skip(start);
            let selected: Vec<&str> = match lim {
                Some(n) => it.by_ref().take(n).collect(),
                None => it.by_ref().collect(),
            };
            selected.join("\n")
        }
    };
    let (content, byte_capped) = cap_text(&windowed, MAX_READ_FILE_BYTES);
    Ok(ReadFile {
        content,
        truncated: over_cap || byte_capped,
        total_lines,
        size: meta.len(),
    })
}

pub(super) async fn read_media_file_bounded(file: &Path, path: &str) -> Result<ReadMediaFile> {
    let meta = tokio::fs::metadata(file)
        .await
        .map_err(|e| io_err("stat", file, e))?;
    if meta.is_dir() {
        return Err(Error::invalid(format!(
            "`{path}` is a directory, not a file"
        )));
    }
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    if over_cap {
        return Err(Error::invalid(format!(
            "`{path}` is too large for native model ingestion (maximum {HARD_READ_BYTES} bytes)"
        )));
    }
    Ok(ReadMediaFile {
        bytes,
        size: meta.len(),
    })
}

pub(super) fn text_file_content(bytes: Vec<u8>, path: &str, truncated: bool) -> Result<String> {
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes).expect("prefix ending at valid_up_to is valid UTF-8")
        }
        Err(_) => {
            return Err(Error::invalid(format!(
                "`{path}` is binary or is not valid UTF-8; read_file reads text files only"
            )))
        }
    };
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(Error::invalid(format!(
            "`{path}` contains binary control bytes; read_file reads text files only"
        )));
    }
    Ok(text)
}

/// Load a file's text for [`TerminalManager::edit_file`], refusing an
/// over-the-ceiling or non-UTF-8 file (a partial or lossy read would corrupt
/// it on write-back).
pub(super) async fn read_text_for_edit(file: &Path) -> Result<String> {
    let (bytes, over_cap) = read_file_bounded(file, HARD_READ_BYTES).await?;
    if over_cap {
        return Err(Error::invalid(
            "file is too large to edit safely; edit it from the terminal instead",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| Error::invalid("file is not valid UTF-8; edit_file only edits text files"))
}

/// Apply the `edit_file` replacement contract to `content`: `old` must occur
/// exactly once unless `replace_all`. Returns the updated text and how many
/// replacements were made.
pub(super) fn apply_edit(content: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, usize)> {
    let count = content.matches(old).count();
    if count == 0 {
        return Err(Error::invalid("`old_string` was not found in the file"));
    }
    if count > 1 && !replace_all {
        return Err(Error::invalid(format!(
            "`old_string` is not unique ({count} matches); add surrounding context to \
             target one occurrence, or pass replace_all=true"
        )));
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    Ok((updated, if replace_all { count } else { 1 }))
}

/// Read up to `cap` bytes of `file`; the bool is whether the file exceeded `cap`
/// (so the returned bytes are a prefix). Bounds memory on a huge file.
async fn read_file_bounded(file: &Path, cap: u64) -> Result<(Vec<u8>, bool)> {
    use tokio::io::AsyncReadExt;
    let f = tokio::fs::File::open(file)
        .await
        .map_err(|e| io_err("open", file, e))?;
    // Read one byte past the cap so we can detect (and flag) an oversized file.
    let mut limited = f.take(cap + 1);
    let mut buf = Vec::new();
    limited
        .read_to_end(&mut buf)
        .await
        .map_err(|e| io_err("read", file, e))?;
    let over = buf.len() as u64 > cap;
    if over {
        buf.truncate(cap as usize);
    }
    Ok((buf, over))
}

/// Cap `text` to `max` bytes on a UTF-8 char boundary; returns the (possibly
/// shortened) text and whether it was truncated.
pub(super) fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Map a file io error to a tool [`Error`], keeping "not found" precise (the
/// host path is already exposed to the agent via `open_terminal`'s `host_dir`).
pub(super) fn io_err(op: &str, file: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        return Error::invalid(format!("no such file: {}", file.display()));
    }
    Error::provider(format!("failed to {op} {}: {e}", file.display()))
}
