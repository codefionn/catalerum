//! Integration test: [`S3Backend`] against a live S3-compatible service (MinIO).
//!
//! Gated on `CATALERUM_TEST_S3_ENDPOINT` (e.g. `http://127.0.0.1:9000`) with
//! optional `CATALERUM_TEST_S3_KEY`/`CATALERUM_TEST_S3_SECRET` (default
//! `minioadmin`/`minioadmin`); unset → the test prints a skip note and passes, so
//! the suite stays green offline.

use catalerum_core::error::Error;
use catalerum_core::provider::{ByteStream, PutMeta, StorageBackend};
use catalerum_storage::S3Backend;
use futures::stream::{self, StreamExt};

fn backend() -> Option<S3Backend> {
    let endpoint = std::env::var("CATALERUM_TEST_S3_ENDPOINT").ok()?;
    let key = std::env::var("CATALERUM_TEST_S3_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("CATALERUM_TEST_S3_SECRET").unwrap_or_else(|_| "minioadmin".into());
    // Path-style for MinIO; a fixed bucket (idempotently ensured below).
    Some(S3Backend::new(
        &endpoint,
        "us-east-1",
        &key,
        &secret,
        "cat-s3-test",
        true,
    ))
}

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

async fn keys(b: &S3Backend, prefix: &str) -> Vec<String> {
    b.list(prefix)
        .await
        .unwrap()
        .map(|m| m.unwrap().key)
        .collect::<Vec<_>>()
        .await
}

#[tokio::test]
async fn s3_put_get_stat_list_delete_roundtrip() {
    let Some(store) = backend() else {
        eprintln!("skipping s3 roundtrip: set CATALERUM_TEST_S3_ENDPOINT (+ MinIO running)");
        return;
    };
    store.ensure_container().await.expect("ensure bucket");

    // Clean any leftovers under our test prefix so the listing assertion is exact.
    for k in keys(&store, "s3test/").await {
        store.delete(&k).await.unwrap();
    }

    // put two objects, one nested, one with an explicit content type.
    store
        .put(
            "s3test/notes/hello.md",
            bytes(b"# hi"),
            PutMeta {
                content_type: Some("text/markdown".into()),
                content_length: None,
            },
        )
        .await
        .expect("put nested");
    store
        .put("s3test/data.json", bytes(b"{\"a\":1}"), PutMeta::default())
        .await
        .expect("put json");

    // get round-trips the bytes; stat reports size + the stored content type + etag.
    assert_eq!(
        read_all(store.get("s3test/notes/hello.md").await.unwrap()).await,
        b"# hi"
    );
    let meta = store.stat("s3test/notes/hello.md").await.unwrap();
    assert_eq!(meta.key, "s3test/notes/hello.md");
    assert_eq!(meta.size, 4);
    assert_eq!(meta.content_type.as_deref(), Some("text/markdown"));
    assert!(meta.etag.is_some() && !meta.etag.as_deref().unwrap().contains('"'));

    // list under the prefix → exactly our two keys, sorted.
    assert_eq!(
        keys(&store, "s3test/").await,
        vec![
            "s3test/data.json".to_string(),
            "s3test/notes/hello.md".to_string()
        ]
    );
    // A narrower prefix filters.
    assert_eq!(
        keys(&store, "s3test/notes/").await,
        vec!["s3test/notes/hello.md".to_string()]
    );

    // delete is idempotent; the object is then gone (NotFound on stat/get).
    store.delete("s3test/data.json").await.unwrap();
    store.delete("s3test/data.json").await.unwrap(); // no-op, no error
    assert!(matches!(
        store.stat("s3test/data.json").await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        store.get("s3test/missing").await,
        Err(Error::NotFound)
    ));

    // cleanup.
    store.delete("s3test/notes/hello.md").await.unwrap();
}

/// A `get`/`stat` against a **missing bucket** must map to `NotFound` (a 404), not a
/// generic provider error (which the REST layer turns into a 500). `GetObjectError`
/// has no `NoSuchBucket` variant, so this exercises the HTTP-status fallback.
#[tokio::test]
async fn s3_missing_bucket_maps_to_not_found() {
    let Some(endpoint) = std::env::var("CATALERUM_TEST_S3_ENDPOINT").ok() else {
        eprintln!("skipping s3 missing-bucket test: set CATALERUM_TEST_S3_ENDPOINT");
        return;
    };
    let key = std::env::var("CATALERUM_TEST_S3_KEY").unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("CATALERUM_TEST_S3_SECRET").unwrap_or_else(|_| "minioadmin".into());
    let absent = S3Backend::new(
        &endpoint,
        "us-east-1",
        &key,
        &secret,
        "cat-no-such-bucket-zzz",
        true,
    );
    assert!(
        matches!(absent.get("k").await, Err(Error::NotFound)),
        "get on a missing bucket → NotFound"
    );
    assert!(
        matches!(absent.stat("k").await, Err(Error::NotFound)),
        "stat on a missing bucket → NotFound"
    );

    // `ensure_container` with a non-`us-east-1` region exercises the
    // `LocationConstraint` branch; MinIO accepts (ignores) it, so it succeeds.
    let regional = S3Backend::new(
        &endpoint,
        "eu-central-1",
        &key,
        &secret,
        "cat-s3-test-eu",
        true,
    );
    regional
        .ensure_container()
        .await
        .expect("ensure with a regional LocationConstraint");
}
