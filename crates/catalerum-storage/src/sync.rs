//! Mirror a local directory into a [`StorageBackend`] under a key prefix
//! (SOUL §20): the ephemeral-terminal flush (`persist_terminal`) reuses this. It
//! walks the directory via a
//! throwaway [`LocalFsBackend`] and re-streams each file through the uniform
//! `StorageBackend::put`, so it works against any target (local FS / S3 / WebDAV).
//!
//! Whole-file buffered (no true streaming) — intended for terminal working
//! directories, not arbitrarily large blobs.

use std::path::Path;

use catalerum_core::error::Result;
use catalerum_core::provider::{ByteStream, PutMeta, StorageBackend};
use futures::StreamExt;

use crate::local::LocalFsBackend;

/// Mirror every regular file under `dir` into `target` at
/// `<key_prefix>/<relative-path>`. Returns the destination keys written, in the
/// order uploaded. An empty `key_prefix` writes the relative paths at the root.
///
/// The caller is responsible for any workspace namespacing of `key_prefix`
/// (e.g. [`workspace_object_key`](catalerum_core::provider::workspace_object_key)).
pub async fn sync_dir_to_backend(
    dir: &Path,
    target: &dyn StorageBackend,
    key_prefix: &str,
) -> Result<Vec<String>> {
    let src = LocalFsBackend::new(dir.to_path_buf());
    let prefix = key_prefix.trim_matches('/');

    let mut listing = src.list("").await?;
    let mut written = Vec::new();
    while let Some(meta) = listing.next().await {
        let meta = meta?;
        let rel = meta.key;
        let dest = if prefix.is_empty() {
            rel.clone()
        } else {
            format!("{prefix}/{rel}")
        };
        let bytes = read_all(src.get(&rel).await?).await?;
        let put = PutMeta {
            content_type: meta.content_type.clone(),
            content_length: Some(bytes.len() as u64),
        };
        let stream = futures::stream::once(async move { Ok(bytes) }).boxed();
        target.put(&dest, stream, put).await?;
        written.push(dest);
    }
    Ok(written)
}

/// Drain a [`ByteStream`] into a single buffer.
async fn read_all(mut stream: ByteStream) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk?);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mirrors_a_dir_tree_under_a_prefix() {
        let src = tempfile::tempdir().unwrap();
        tokio::fs::write(src.path().join("a.txt"), b"alpha")
            .await
            .unwrap();
        tokio::fs::create_dir(src.path().join("sub")).await.unwrap();
        tokio::fs::write(src.path().join("sub/b.txt"), b"beta")
            .await
            .unwrap();

        let dest_dir = tempfile::tempdir().unwrap();
        let target = LocalFsBackend::new(dest_dir.path().to_path_buf());

        let mut keys = sync_dir_to_backend(src.path(), &target, "snap/v1")
            .await
            .unwrap();
        keys.sort();
        assert_eq!(
            keys,
            vec!["snap/v1/a.txt".to_string(), "snap/v1/sub/b.txt".to_string()]
        );

        // Bytes landed under the prefixed keys.
        let got = read_all(target.get("snap/v1/sub/b.txt").await.unwrap())
            .await
            .unwrap();
        assert_eq!(got, b"beta");
    }
}
