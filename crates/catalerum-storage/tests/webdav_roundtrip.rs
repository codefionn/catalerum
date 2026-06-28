//! Integration test: [`WebDavBackend`] against a live WebDAV server.
//!
//! Gated on `CATALERUM_TEST_WEBDAV_URL` (e.g. `http://127.0.0.1:8788`) with optional
//! `CATALERUM_TEST_WEBDAV_USER`/`CATALERUM_TEST_WEBDAV_PASS`; unset → skip + pass, so
//! the suite stays green offline. Verified against `rclone serve webdav`.

use catalerum_core::error::Error;
use catalerum_core::provider::{ByteStream, PutMeta, StorageBackend};
use catalerum_storage::WebDavBackend;
use futures::stream::{self, StreamExt};

fn backend() -> Option<WebDavBackend> {
    let url = std::env::var("CATALERUM_TEST_WEBDAV_URL").ok()?;
    let user = std::env::var("CATALERUM_TEST_WEBDAV_USER").unwrap_or_default();
    let pass = std::env::var("CATALERUM_TEST_WEBDAV_PASS").unwrap_or_default();
    Some(WebDavBackend::new(&url, &user, &pass).expect("valid webdav url"))
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

async fn keys(b: &WebDavBackend, prefix: &str) -> Vec<String> {
    b.list(prefix)
        .await
        .unwrap()
        .map(|m| m.unwrap().key)
        .collect::<Vec<_>>()
        .await
}

#[tokio::test]
async fn webdav_put_get_stat_list_delete_roundtrip() {
    let Some(store) = backend() else {
        eprintln!("skipping webdav roundtrip: set CATALERUM_TEST_WEBDAV_URL (+ a WebDAV server)");
        return;
    };
    store.ensure_container().await.expect("ensure container");

    // Clean leftovers under our prefix so the listing assertion is exact.
    for k in keys(&store, "wdtest/").await {
        store.delete(&k).await.unwrap();
    }

    // put two objects, one nested (parent collections auto-created), one with a CT.
    store
        .put(
            "wdtest/notes/hello.md",
            bytes(b"# hi"),
            PutMeta {
                content_type: Some("text/markdown".into()),
                content_length: None,
            },
        )
        .await
        .expect("put nested");
    store
        .put("wdtest/data.json", bytes(b"{\"a\":1}"), PutMeta::default())
        .await
        .expect("put json");

    // get round-trips the bytes; stat reports size.
    assert_eq!(
        read_all(store.get("wdtest/notes/hello.md").await.unwrap()).await,
        b"# hi"
    );
    let meta = store.stat("wdtest/notes/hello.md").await.unwrap();
    assert_eq!(meta.key, "wdtest/notes/hello.md");
    assert_eq!(meta.size, 4);

    // list under the prefix → exactly our two keys, sorted; a narrower prefix filters.
    assert_eq!(
        keys(&store, "wdtest/").await,
        vec![
            "wdtest/data.json".to_string(),
            "wdtest/notes/hello.md".to_string()
        ]
    );
    assert_eq!(
        keys(&store, "wdtest/notes/").await,
        vec!["wdtest/notes/hello.md".to_string()]
    );

    // delete is idempotent; the object is then gone (NotFound on stat/get).
    store.delete("wdtest/data.json").await.unwrap();
    store.delete("wdtest/data.json").await.unwrap(); // no-op, no error
    assert!(matches!(
        store.stat("wdtest/data.json").await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        store.get("wdtest/missing").await,
        Err(Error::NotFound)
    ));

    // cleanup.
    store.delete("wdtest/notes/hello.md").await.unwrap();
}
