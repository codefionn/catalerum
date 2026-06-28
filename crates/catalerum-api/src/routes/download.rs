//! Public signed-download redeem route (SOUL §9/§18).
//!
//! `GET /download/{token}` is the **one unauthenticated** storage surface: it takes
//! no `Auth` extractor because the token itself is the authorization (an
//! HMAC-signed [`DownloadClaims`](crate::download_link::DownloadClaims), minted by
//! the `download_link` tool). The route re-verifies the signature + expiry, then
//! streams exactly the one file — or, for a directory link, a `.tar.gz` of every
//! object under the prefix — that the claims name, scoped to the claims' workspace
//! (§18). Every failure (bad signature, expired, unknown key) collapses to a flat
//! `404` so a probe learns nothing.
//!
//! The link is deliberately weaker than a bearer token (§19): it reads one
//! object/prefix for a short window, never the workspace at large.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::StreamExt;

use catalerum_core::WorkspaceId;
use catalerum_store::DEFAULT_OBJECT_LIMIT;

use crate::error::{ApiError, ApiResult};
use crate::routes::storage::{resolve_store, validate_object_key};
use crate::state::{AppState, StorageHandle};

/// The largest total (uncompressed) payload a **directory** link will archive
/// (256 MiB). Bounds the in-memory `.tar.gz` build so a link over a huge prefix
/// fails fast at mint time rather than exhausting memory at redeem (SOUL §18, the
/// no-unbounded-read principle). A single-file link is unbounded — one object is
/// already a bounded thing the user chose.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 256 << 20;

/// Mount the public download route.
pub fn router() -> Router<AppState> {
    Router::new().route("/download/{token}", get(redeem))
}

/// `GET /download/{token}` — verify the signed token and stream the file (or the
/// directory as a `.tar.gz`). Unauthenticated: the token is the capability.
async fn redeem(State(state): State<AppState>, Path(token): Path<String>) -> ApiResult<Response> {
    // Verify signature + expiry. Any problem → an opaque 404 (never reveal whether a
    // token was forged, expired, or simply names a missing file).
    let now = chrono::Utc::now().timestamp();
    let claims = state
        .download_signer()
        .verify(&token, now)
        .map_err(|_| ApiError::NotFound)?;
    let ws = claims.workspace_id;
    // Fail closed on an archived workspace (SOUL §18, "fail closed everywhere"):
    // a link minted before archival must not keep leaking the now-inert
    // workspace's files. Indistinguishable from a bad token — the same opaque
    // 404 every other failure here collapses to (`get` returns archived rows by
    // design, so the flag is tested explicitly). Mirrors the trigger/webhook
    // public-redeem guard.
    if let Ok(w) = state.store().workspaces().get(ws).await {
        if w.archived_at.is_some() {
            return Err(ApiError::NotFound);
        }
    }
    // Resolve the store named in the claims (no acting user — the claims are
    // self-contained, so default resolution never consults a per-user override).
    let handle = resolve_store(
        state.storage(),
        state.store(),
        ws,
        None,
        claims.store.as_deref(),
    )
    .await
    .map_err(|_| ApiError::NotFound)?;
    if claims.dir {
        serve_archive(ws, &handle, &claims.key).await
    } else {
        serve_file(ws, &handle, &claims.key).await
    }
}

/// Stream one stored object's bytes as an attachment. `404` when absent (so a link
/// whose file was deleted after minting fails cleanly).
async fn serve_file(ws: WorkspaceId, handle: &StorageHandle, key: &str) -> ApiResult<Response> {
    validate_object_key(key).map_err(|_| ApiError::NotFound)?;
    let physical = handle.physical_key(ws, key);
    let meta = handle
        .backend
        .stat(&physical)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let mut stream = handle
        .backend
        .get(&physical)
        .await
        .map_err(|_| ApiError::NotFound)?;
    let mut bytes = Vec::with_capacity(meta.size as usize);
    while let Some(chunk) = stream.next().await {
        bytes.extend(chunk.map_err(|e| ApiError::internal(format!("reading object: {e}")))?);
    }
    let content_type = meta
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, attachment(&basename(key))),
        ],
        bytes,
    )
        .into_response())
}

/// Stream every object under `prefix` as a single `.tar.gz` attachment. The prefix
/// listing is bounded by [`DEFAULT_OBJECT_LIMIT`] and the total uncompressed size by
/// [`MAX_ARCHIVE_BYTES`]; over either, `404` (a mint-time check already refuses these,
/// so this is the belt-and-braces guard). Each entry keeps its user-facing key as its
/// archive path.
async fn serve_archive(
    ws: WorkspaceId,
    handle: &StorageHandle,
    prefix: &str,
) -> ApiResult<Response> {
    let entries = collect_prefix(ws, handle, prefix).await?;
    if entries.is_empty() {
        return Err(ApiError::NotFound);
    }
    // Build the gzip'd tar off the async runtime (CPU-bound compression), from the
    // already-fetched in-memory entries.
    let name = format!("{}.tar.gz", basename(prefix.trim_end_matches('/')));
    let archive = tokio::task::spawn_blocking(move || build_tar_gz(&entries))
        .await
        .map_err(|e| ApiError::internal(format!("archiving directory: {e}")))?
        .map_err(|e| ApiError::internal(format!("archiving directory: {e}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/gzip".to_string()),
            (header::CONTENT_DISPOSITION, attachment(&name)),
        ],
        Body::from(archive),
    )
        .into_response())
}

/// Fetch every object under `prefix` into memory as `(archive_path, bytes)`,
/// enforcing the count + size bounds. `archive_path` is the object's user-facing key
/// (so `reports/q3.pdf` lands at that path inside the tar).
async fn collect_prefix(
    ws: WorkspaceId,
    handle: &StorageHandle,
    prefix: &str,
) -> ApiResult<Vec<(String, Vec<u8>)>> {
    let scoped = handle.physical_key(ws, prefix);
    let stream = handle
        .backend
        .list(&scoped)
        .await
        .map_err(|e| ApiError::internal(format!("listing directory: {e}")))?;
    let metas: Vec<_> = stream
        .filter_map(|r| async move { r.ok() })
        .take(DEFAULT_OBJECT_LIMIT as usize)
        .collect()
        .await;
    // Refuse an over-large archive up front (also guards against a prefix whose total
    // grew past the cap since the link was minted).
    let total: u64 = metas.iter().map(|m| m.size).sum();
    if total > MAX_ARCHIVE_BYTES {
        return Err(ApiError::bad_request(format!(
            "directory is too large to archive ({total} bytes > {MAX_ARCHIVE_BYTES} limit)"
        )));
    }
    let mut out = Vec::with_capacity(metas.len());
    for m in metas {
        let path = handle.user_key(ws, &m.key);
        let mut stream = handle
            .backend
            .get(&m.key)
            .await
            .map_err(|e| ApiError::internal(format!("reading {path}: {e}")))?;
        let mut bytes = Vec::with_capacity(m.size as usize);
        while let Some(chunk) = stream.next().await {
            bytes.extend(chunk.map_err(|e| ApiError::internal(format!("reading {path}: {e}")))?);
        }
        out.push((path, bytes));
    }
    Ok(out)
}

/// Build a gzip-compressed tar from in-memory entries. Uses GNU headers so keys
/// longer than the 100-byte ustar limit are stored losslessly.
fn build_tar_gz(entries: &[(String, Vec<u8>)]) -> std::io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    let gz = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(gz);
    for (path, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        // `append_data` sets the (possibly long GNU) path and recomputes the checksum.
        builder.append_data(&mut header, path, bytes.as_slice())?;
    }
    let gz = builder.into_inner()?;
    gz.finish()
}

/// The last path segment of a key (the download's suggested filename). Falls back to
/// `"download"` for an empty/slash-only key so the header is always well-formed.
fn basename(key: &str) -> String {
    let name = key
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        "download".to_string()
    } else {
        name.to_string()
    }
}

/// A `Content-Disposition: attachment; filename="…"` value, with characters that
/// would break the header (quotes, backslashes, control chars, path separators)
/// stripped from the name.
fn attachment(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '"' | '\\' | '/'))
        .collect();
    let safe = if safe.trim().is_empty() {
        "download".to_string()
    } else {
        safe
    };
    format!("attachment; filename=\"{safe}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn basename_takes_last_segment() {
        assert_eq!(basename("reports/q3.pdf"), "q3.pdf");
        assert_eq!(basename("reports/"), "reports");
        assert_eq!(basename("file.txt"), "file.txt");
        assert_eq!(basename(""), "download");
        assert_eq!(basename("/"), "download");
    }

    #[test]
    fn attachment_strips_dangerous_chars() {
        assert_eq!(attachment("q3.pdf"), "attachment; filename=\"q3.pdf\"");
        // Quotes / backslashes / slashes that would break the header are removed.
        assert_eq!(
            attachment("a\"b\\c/d.pdf"),
            "attachment; filename=\"abcd.pdf\""
        );
        assert_eq!(attachment("   "), "attachment; filename=\"download\"");
    }

    #[test]
    fn build_tar_gz_is_a_readable_gzip_tar() {
        let entries = vec![
            ("reports/a.txt".to_string(), b"hello".to_vec()),
            ("reports/nested/b.bin".to_string(), vec![0u8, 1, 2, 3]),
        ];
        let gz = build_tar_gz(&entries).unwrap();
        // gzip magic bytes.
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        // Decompress + read the tar back; every entry round-trips with its path/bytes.
        let mut decoder = flate2::read::GzDecoder::new(&gz[..]);
        let mut tar_bytes = Vec::new();
        decoder.read_to_end(&mut tar_bytes).unwrap();
        let mut archive = tar::Archive::new(&tar_bytes[..]);
        let mut seen = std::collections::HashMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            seen.insert(path, data);
        }
        assert_eq!(
            seen.get("reports/a.txt").map(Vec::as_slice),
            Some(&b"hello"[..])
        );
        assert_eq!(
            seen.get("reports/nested/b.bin").map(Vec::as_slice),
            Some(&[0u8, 1, 2, 3][..])
        );
    }
}
