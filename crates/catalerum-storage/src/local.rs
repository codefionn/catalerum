//! Local-filesystem [`StorageBackend`] (SOUL §9): a bucket is a directory, and an
//! object **key** is a slash-separated relative path under it. Blobs live on disk;
//! only catalogued metadata lands in Postgres (the ingest layer's job, §10).
//!
//! Keys are validated against path traversal — a key may not be absolute or
//! contain `..`/`.` components, so an object can never escape the bucket root.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::{self, BoxStream, StreamExt};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{ByteStream, ObjectMeta, PutMeta, StorageBackend};

/// A [`StorageBackend`] over a local directory (`root`). Each object key resolves
/// to `root/<key>`.
#[derive(Clone, Debug)]
pub struct LocalFsBackend {
    root: PathBuf,
}

impl LocalFsBackend {
    /// A backend rooted at `root` (the bucket directory; created on first `put`).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Resolve `key` to an absolute path under [`root`](Self::root), rejecting an
    /// empty key or any traversal (absolute, `..`, `.`).
    fn resolve(&self, key: &str) -> Result<PathBuf> {
        let rel = Path::new(key);
        let mut any = false;
        for component in rel.components() {
            match component {
                Component::Normal(_) => any = true,
                _ => return Err(Error::invalid(format!("invalid object key `{key}`"))),
            }
        }
        if !any {
            return Err(Error::invalid("object key must not be empty"));
        }
        Ok(self.root.join(rel))
    }
}

#[async_trait]
impl StorageBackend for LocalFsBackend {
    async fn list(&self, prefix: &str) -> Result<BoxStream<'static, Result<ObjectMeta>>> {
        let mut out: Vec<ObjectMeta> = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries = match tokio::fs::read_dir(&dir).await {
                Ok(e) => e,
                // The bucket dir (or a vanished subdir) doesn't exist → nothing here.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => return Err(map_io(e)),
            };
            while let Some(entry) = entries.next_entry().await.map_err(map_io)? {
                let file_type = entry.file_type().await.map_err(map_io)?;
                let path = entry.path();
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Some(key) = key_for(&self.root, &path) else {
                    continue;
                };
                if key.starts_with(prefix) {
                    let meta = entry.metadata().await.map_err(map_io)?;
                    out.push(object_meta(&key, &meta));
                }
            }
        }
        // Deterministic, key-sorted listing.
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(stream::iter(out.into_iter().map(Ok)).boxed())
    }

    async fn stat(&self, key: &str) -> Result<ObjectMeta> {
        let path = self.resolve(key)?;
        let meta = tokio::fs::metadata(&path).await.map_err(map_io)?;
        if !meta.is_file() {
            return Err(Error::NotFound);
        }
        Ok(object_meta(key, &meta))
    }

    async fn get(&self, key: &str) -> Result<ByteStream> {
        let path = self.resolve(key)?;
        // Open eagerly so a missing object fails with `NotFound` here (before the
        // stream is first polled), then stream the body in fixed-size chunks so a
        // large blob is never held in memory all at once.
        let file = tokio::fs::File::open(&path).await.map_err(map_io)?;
        Ok(read_chunks(file))
    }

    async fn put(&self, key: &str, mut data: ByteStream, _meta: PutMeta) -> Result<()> {
        let path = self.resolve(key)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(map_io)?;
        }
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(&path).await.map_err(map_io)?;
        while let Some(chunk) = data.next().await {
            file.write_all(&chunk?).await.map_err(map_io)?;
        }
        file.flush().await.map_err(map_io)?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            // Idempotent: deleting an absent object is a no-op success.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(map_io(e)),
        }
    }
}

/// Stream a file's bytes in fixed-size chunks, so reading a large object bounds
/// memory to one chunk rather than the whole file. An I/O error mid-stream surfaces
/// as a stream error (mapped like every other backend error).
fn read_chunks(file: tokio::fs::File) -> ByteStream {
    use tokio::io::AsyncReadExt;
    /// 64 KiB — the same working-set size the SSE/exec paths use; large enough to
    /// amortise syscalls, small enough to keep the per-read allocation cheap.
    const CHUNK: usize = 64 * 1024;
    stream::try_unfold(file, |mut file| async move {
        let mut buf = vec![0u8; CHUNK];
        let n = file.read(&mut buf).await.map_err(map_io)?;
        if n == 0 {
            Ok(None) // EOF
        } else {
            buf.truncate(n);
            Ok(Some((buf, file)))
        }
    })
    .boxed()
}

/// The slash-separated object key for `path` under `root`, if it is within it.
fn key_for(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let key = rel.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
    (!key.is_empty()).then_some(key)
}

/// Build an [`ObjectMeta`] from a key + filesystem metadata. The etag is a weak
/// `size-mtime` validator (cheap + stable for an unchanged file).
fn object_meta(key: &str, meta: &std::fs::Metadata) -> ObjectMeta {
    let size = meta.len();
    let last_modified: DateTime<Utc> = meta
        .modified()
        .ok()
        .map_or_else(Utc::now, DateTime::<Utc>::from);
    ObjectMeta {
        key: key.to_string(),
        size,
        etag: Some(format!("{size:x}-{:x}", last_modified.timestamp())),
        content_type: content_type_for(key),
        last_modified,
    }
}

/// A best-effort content type from the key's extension (`None` if unknown).
fn content_type_for(key: &str) -> Option<String> {
    let ext = key.rsplit('.').next()?.to_ascii_lowercase();
    // The text-like data/config/markup types here are chosen so the §10 object
    // indexer's `is_text_like` (catalerum-ingest) recognises them and extracts their
    // content — otherwise a locally-stored `.xml`/`.yaml`/`.toml`/`.ndjson` file would
    // carry no content type and be silently skipped despite being extractable.
    let ct = match ext.as_str() {
        "txt" | "text" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "ndjson" | "jsonl" => "application/x-ndjson",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "html" | "htm" => "text/html",
        "xhtml" => "application/xhtml+xml",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        // Source code + plain-text config/prose. These have no standard MIME, so
        // `text/plain` (which `is_text_like` accepts) — the point is the same as for
        // the data types above: an uploaded code/config file is §10-indexed rather
        // than silently skipped. Deliberately excludes ambiguous extensions that
        // collide with binaries (e.g. `.ts` = TypeScript *or* MPEG transport stream)
        // and secret-bearing `.env`.
        "rs" | "py" | "go" | "java" | "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "rb" | "php"
        | "sh" | "bash" | "zsh" | "sql" | "kt" | "kts" | "swift" | "scala" | "lua" | "pl"
        | "pm" | "r" | "jl" | "ex" | "exs" | "clj" | "hs" | "ini" | "cfg" | "conf"
        | "properties" | "rst" | "tex" | "adoc" => "text/plain",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(ct.to_string())
}

/// Map a filesystem error to a core [`Error`]: `NotFound` stays precise, the rest
/// surface as [`Error::Io`].
fn map_io(e: std::io::Error) -> Error {
    if e.kind() == ErrorKind::NotFound {
        Error::NotFound
    } else {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::provider::PutMeta;

    fn bytes(b: &[u8]) -> ByteStream {
        let owned = b.to_vec();
        stream::once(async move { Ok(owned) }).boxed()
    }

    async fn read_all(s: ByteStream) -> Vec<u8> {
        s.fold(Vec::new(), |mut acc, chunk| async move {
            acc.extend_from_slice(&chunk.unwrap());
            acc
        })
        .await
    }

    #[tokio::test]
    async fn put_get_stat_list_delete_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsBackend::new(dir.path());

        // put two objects, one nested.
        store
            .put("notes/hello.md", bytes(b"# hi"), PutMeta::default())
            .await
            .unwrap();
        store
            .put("data.json", bytes(b"{\"a\":1}"), PutMeta::default())
            .await
            .unwrap();

        // get round-trips the bytes; stat reports size + a guessed content type.
        assert_eq!(
            read_all(store.get("notes/hello.md").await.unwrap()).await,
            b"# hi"
        );
        let meta = store.stat("notes/hello.md").await.unwrap();
        assert_eq!(meta.key, "notes/hello.md");
        assert_eq!(meta.size, 4);
        assert_eq!(meta.content_type.as_deref(), Some("text/markdown"));
        assert!(meta.etag.is_some());

        // list: all keys (slash-separated, sorted), and a prefix filter.
        let keys: Vec<String> = read_keys(&store, "").await;
        assert_eq!(
            keys,
            vec!["data.json".to_string(), "notes/hello.md".to_string()]
        );
        assert_eq!(
            read_keys(&store, "notes/").await,
            vec!["notes/hello.md".to_string()]
        );

        // delete is idempotent; the object is then gone.
        store.delete("data.json").await.unwrap();
        store.delete("data.json").await.unwrap(); // no-op, no error
        assert!(matches!(
            store.stat("data.json").await,
            Err(Error::NotFound)
        ));
        assert!(matches!(store.get("missing").await, Err(Error::NotFound)));
    }

    #[tokio::test]
    async fn get_streams_large_objects_in_multiple_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsBackend::new(dir.path());
        // > 64 KiB so the body spans several chunks (not one read-whole-file blob).
        let big: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        store
            .put("big.bin", bytes(&big), PutMeta::default())
            .await
            .unwrap();

        let mut stream = store.get("big.bin").await.unwrap();
        let mut chunks = 0usize;
        let mut got = Vec::new();
        while let Some(chunk) = stream.next().await {
            let c = chunk.unwrap();
            assert!(!c.is_empty(), "a yielded chunk is never empty");
            chunks += 1;
            got.extend_from_slice(&c);
        }
        assert_eq!(got, big, "streamed bytes round-trip exactly");
        assert!(
            chunks >= 2,
            "a >64 KiB object should stream in multiple chunks, got {chunks}"
        );
    }

    async fn read_keys(store: &LocalFsBackend, prefix: &str) -> Vec<String> {
        store
            .list(prefix)
            .await
            .unwrap()
            .map(|m| m.unwrap().key)
            .collect::<Vec<_>>()
            .await
    }

    #[tokio::test]
    async fn rejects_path_traversal_and_empty_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalFsBackend::new(dir.path());
        for bad in ["../escape", "/abs", "a/../../b", "", "."] {
            assert!(
                matches!(store.stat(bad).await, Err(Error::Invalid(_))),
                "key `{bad}` must be rejected"
            );
            assert!(matches!(
                store.put(bad, bytes(b"x"), PutMeta::default()).await,
                Err(Error::Invalid(_))
            ));
        }
        // A normal nested key is fine and never escapes root.
        store
            .put("ok/nested/file.txt", bytes(b"x"), PutMeta::default())
            .await
            .unwrap();
        assert!(dir.path().join("ok/nested/file.txt").exists());
        assert!(!dir.path().parent().unwrap().join("escape").exists());
    }

    #[test]
    fn content_type_covers_text_like_and_web_extensions() {
        let ct = |k: &str| content_type_for(k);
        // Text-like data/config/markup → must be typed so the §10 indexer extracts
        // them (these strings satisfy catalerum-ingest's `is_text_like`).
        assert_eq!(ct("data.xml").as_deref(), Some("application/xml"));
        assert_eq!(ct("conf.yaml").as_deref(), Some("application/yaml"));
        assert_eq!(ct("conf.yml").as_deref(), Some("application/yaml"));
        assert_eq!(ct("Cargo.toml").as_deref(), Some("application/toml"));
        assert_eq!(ct("events.ndjson").as_deref(), Some("application/x-ndjson"));
        assert_eq!(ct("logo.svg").as_deref(), Some("image/svg+xml"));
        assert_eq!(ct("style.css").as_deref(), Some("text/css"));
        assert_eq!(ct("app.js").as_deref(), Some("text/javascript"));
        assert_eq!(ct("server.log").as_deref(), Some("text/plain"));
        // Source code + plain-text config → text/plain, so they're §10-indexed
        // instead of silently skipped for lack of a content type.
        for f in [
            "main.rs",
            "app.py",
            "query.sql",
            "run.sh",
            "notes.rst",
            "script.lua",
        ] {
            assert_eq!(
                ct(f).as_deref(),
                Some("text/plain"),
                "{f} should be text/plain"
            );
        }
        // Common images → correct download content type (not text-like).
        assert_eq!(ct("a.gif").as_deref(), Some("image/gif"));
        assert_eq!(ct("a.webp").as_deref(), Some("image/webp"));
        // Extension match is case-insensitive; unknown / extension-less stays None.
        assert_eq!(ct("DATA.XML").as_deref(), Some("application/xml"));
        assert_eq!(ct("mystery.bin"), None);
        assert_eq!(ct("noext"), None);
        // Deliberately excluded: `.ts` (binary-collision with MPEG-TS) and `.env`
        // (secrets) are NOT auto-typed as text.
        assert_eq!(ct("player.ts"), None, ".ts is ambiguous; not auto-typed");
        assert_eq!(ct(".env"), None, ".env is secret-bearing; not auto-typed");
    }
}
