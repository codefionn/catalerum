//! The object-blob half of a backup (SOUL §30): copy every live blob into the
//! backup, and on restore copy them back out.
//!
//! Blobs are the bucket bytes that deliberately never land in Postgres (SOUL
//! §9/§14) — uploaded files, archived raw emails (`mail/<id>.eml`, §28) — keyed
//! by the workspace-namespaced object key (SOUL §18). A backup copies each
//! verbatim under `<prefix>/<id>/objects/<key>`; the catalogue rows that point at
//! them ride along in the Postgres dump, so a restore reunites both halves.
//!
//! A workspace can hold **many** named storage stores (the legacy `[storage]`
//! default plus each `[storage.backends.<name>]`, SOUL §9). A backup mirrors
//! every attached store, keeping each one's blobs under its own
//! `objects/<segment>/<key>` sub-tree (format v2) so a restore can route each
//! store's blobs back to the live backend registered under the same name. A
//! legacy v1 artifact wrote blobs flat at `objects/<key>`; restore still reads
//! that layout (into the default store).
//!
//! This is a full copy every time (no dedup/incremental yet) — simple and
//! correct; content-addressed dedup is a later refinement.

use futures::StreamExt;

use catalerum_core::error::Result;
use catalerum_core::provider::StorageBackend;

use crate::{get_all, put_bytes};

/// The `objects/` sub-prefix inside a backup directory (the v1 flat root).
fn objects_root(prefix: &str, id: &str) -> String {
    format!("{prefix}/{id}/objects/")
}

/// One store's blob sub-prefix: `<prefix>/<id>/objects/<segment>/` (v2 layout).
fn store_root(prefix: &str, id: &str, segment: &str) -> String {
    format!("{prefix}/{id}/objects/{segment}/")
}

/// Copy every blob from one `source` store into the backup under
/// `objects/<segment>/<key>` (its slice of the inventory). Returns
/// `(count, total_bytes)`.
pub(crate) async fn copy_store_objects(
    source: &dyn StorageBackend,
    dest: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    segment: &str,
) -> Result<(u64, u64)> {
    let root = store_root(prefix, id, segment);
    // Materialize the listing first: streaming a `get`+`put` while still holding
    // the source's `list` stream is fine for these buffered backends, but the
    // small up-front collect keeps the copy loop independent of list paging.
    let mut listing = source.list("").await?;
    let mut keys = Vec::new();
    while let Some(meta) = listing.next().await {
        let meta = meta?;
        keys.push((meta.key, meta.content_type));
    }
    drop(listing);

    let mut count = 0u64;
    let mut bytes = 0u64;
    for (key, content_type) in keys {
        let data = get_all(source, &key).await?;
        bytes += data.len() as u64;
        let ct = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
        put_bytes(dest, &format!("{root}{key}"), data, &ct).await?;
        count += 1;
    }
    Ok((count, bytes))
}

/// Copy one store's blobs (under `objects/<segment>/`) back into its live
/// `source` backend (the reverse of [`copy_store_objects`]). Returns the count.
pub(crate) async fn restore_store_objects(
    dest: &dyn StorageBackend,
    source: &dyn StorageBackend,
    prefix: &str,
    id: &str,
    segment: &str,
) -> Result<u64> {
    restore_under_root(dest, source, &store_root(prefix, id, segment)).await
}

/// Restore the **legacy (v1)** flat layout — every blob at `objects/<key>`, no
/// per-store segment — into `source`. Returns the count restored.
pub(crate) async fn restore_legacy_objects(
    dest: &dyn StorageBackend,
    source: &dyn StorageBackend,
    prefix: &str,
    id: &str,
) -> Result<u64> {
    restore_under_root(dest, source, &objects_root(prefix, id)).await
}

/// Copy every blob under `root` in the backup back into `source`, stripping
/// `root` to recover each blob's original (live) key.
async fn restore_under_root(
    dest: &dyn StorageBackend,
    source: &dyn StorageBackend,
    root: &str,
) -> Result<u64> {
    let mut listing = dest.list(root).await?;
    let mut keys = Vec::new();
    while let Some(meta) = listing.next().await {
        let meta = meta?;
        if let Some(orig) = meta.key.strip_prefix(root) {
            if !orig.is_empty() {
                keys.push((meta.key.clone(), orig.to_string(), meta.content_type));
            }
        }
    }
    drop(listing);

    let mut count = 0u64;
    for (backup_key, orig_key, content_type) in keys {
        let data = get_all(dest, &backup_key).await?;
        let ct = content_type.unwrap_or_else(|| "application/octet-stream".to_string());
        put_bytes(source, &orig_key, data, &ct).await?;
        count += 1;
    }
    Ok(count)
}
