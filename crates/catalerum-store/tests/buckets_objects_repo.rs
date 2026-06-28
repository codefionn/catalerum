//! Integration test: the `BucketRepo` + `ObjectRepo` contract — the storage
//! catalogue (SOUL §9, §6.1/§18). Bucket get-or-create idempotency, object upsert
//! idempotency by `(bucket_id, key)`, listing order + per-bucket listing, key
//! lookup, idempotent delete, and cross-workspace isolation (§18).
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::ConnectionKind;
use catalerum_store::{Store, StoreError, UpsertObject};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn bucket_and_object_catalogue_idempotent_and_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping bucket_and_object_catalogue_idempotent_and_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("obj", &format!("obj-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("obj-b", &format!("obj-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // A storage connection backs the bucket (mirrors a calendar connection).
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "local-storage", None, None)
        .await
        .expect("connection");

    // ensure() is get-or-create: two calls for the same (connection, name) return
    // the SAME bucket id — idempotent, never duplicates (§3.4).
    let b1 = store
        .buckets()
        .ensure(ws.id, conn.id, "default", None)
        .await
        .expect("ensure bucket");
    let b2 = store
        .buckets()
        .ensure(ws.id, conn.id, "default", None)
        .await
        .expect("ensure bucket again");
    assert_eq!(b1.id, b2.id, "ensure is idempotent on (connection, name)");
    assert_eq!(
        store
            .buckets()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1,
        "no duplicate bucket row"
    );

    let now = chrono::Utc::now();
    // Catalogue an object, then re-catalogue the SAME key with a changed size:
    // the id is preserved and the size is refreshed — one row, not two (§3.4).
    let o1 = store
        .objects()
        .upsert(&UpsertObject {
            workspace_id: ws.id,
            bucket_id: b1.id,
            key: "docs/readme.md",
            size: 10,
            content_type: Some("text/markdown"),
            etag: Some("a-1"),
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("upsert object");
    assert_eq!(o1.size, 10);

    let o2 = store
        .objects()
        .upsert(&UpsertObject {
            workspace_id: ws.id,
            bucket_id: b1.id,
            key: "docs/readme.md",
            size: 99,
            content_type: Some("text/markdown"),
            etag: Some("a-2"),
            last_modified: now,
            sha256: Some("deadbeef"),
        })
        .await
        .expect("re-upsert object");
    assert_eq!(
        o2.id, o1.id,
        "upsert preserves id on (bucket, key) conflict"
    );
    assert_eq!(o2.size, 99, "size refreshed");
    assert_eq!(o2.sha256.as_deref(), Some("deadbeef"), "sha256 refreshed");

    // A second, distinct key catalogues as its own row.
    store
        .objects()
        .upsert(&UpsertObject {
            workspace_id: ws.id,
            bucket_id: b1.id,
            key: "images/logo.png",
            size: 2048,
            content_type: Some("image/png"),
            etag: None,
            last_modified: now,
            sha256: None,
        })
        .await
        .expect("upsert second object");

    let all = store
        .objects()
        .list_by_workspace(ws.id, "", catalerum_store::DEFAULT_OBJECT_LIMIT)
        .await
        .unwrap();
    assert_eq!(all.len(), 2, "two distinct objects catalogued");

    // A literal-prefix filter narrows to keys under that prefix (applied in SQL).
    let docs = store
        .objects()
        .list_by_workspace(ws.id, "docs/", catalerum_store::DEFAULT_OBJECT_LIMIT)
        .await
        .unwrap();
    assert_eq!(docs.len(), 1, "only the docs/ key");
    assert_eq!(docs[0].key, "docs/readme.md");
    // A non-matching prefix yields nothing; LIKE metacharacters are literal.
    assert!(store
        .objects()
        .list_by_workspace(ws.id, "img%", catalerum_store::DEFAULT_OBJECT_LIMIT)
        .await
        .unwrap()
        .is_empty());

    // The bound applies after the filter: limit 1 over 2 matches → 1 row.
    let capped = store
        .objects()
        .list_by_workspace(ws.id, "", 1)
        .await
        .unwrap();
    assert_eq!(capped.len(), 1, "bounded to one");

    let by_bucket = store.objects().list_by_bucket(ws.id, b1.id).await.unwrap();
    assert_eq!(by_bucket.len(), 2);

    // Key lookup returns the refreshed metadata.
    let got = store
        .objects()
        .get_by_key(ws.id, b1.id, "docs/readme.md")
        .await
        .expect("get_by_key");
    assert_eq!(got.size, 99);
    assert_eq!(got.content_type.as_deref(), Some("text/markdown"));

    // §18: another workspace sees none of it.
    assert!(store
        .buckets()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .objects()
        .list_by_workspace(other.id, "", catalerum_store::DEFAULT_OBJECT_LIMIT)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        store
            .objects()
            .get_by_key(other.id, b1.id, "docs/readme.md")
            .await,
        Err(StoreError::NotFound)
    ));

    // Delete is idempotent: removing the key once drops it; removing again is a
    // clean no-op (mirrors the backend's idempotent delete).
    store
        .objects()
        .delete_by_key(ws.id, b1.id, "docs/readme.md")
        .await
        .expect("delete");
    store
        .objects()
        .delete_by_key(ws.id, b1.id, "docs/readme.md")
        .await
        .expect("delete again (no-op)");
    let remaining = store
        .objects()
        .list_by_workspace(ws.id, "", catalerum_store::DEFAULT_OBJECT_LIMIT)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1, "only the un-deleted object remains");
    assert_eq!(remaining[0].key, "images/logo.png");
}

#[tokio::test]
async fn list_unlabeled_anti_joins_labels_per_store_and_scopes() {
    // The backlog feed for a scheduled "label the unlabelled files" sweep (SOUL
    // §9/§11): labels key on (store, path) while objects key on (bucket, key),
    // so the caller passes the bucket→store mapping and the anti-join runs per
    // bucket under its own store name; `prefix` narrows to a subdirectory; only
    // exact-path labels count; §18 scoping holds. Also covers the batched
    // `list_for_paths` a summaries page uses.
    use catalerum_core::model::Author;
    use catalerum_core::UserId;

    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping list_unlabeled_anti_joins_labels_per_store_and_scopes: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("unlbl", &format!("unlbl-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Storage, "files", None, None)
        .await
        .expect("connection");
    let bucket = store
        .buckets()
        .ensure(ws.id, conn.id, "default", None)
        .await
        .expect("bucket");

    let now = chrono::Utc::now();
    let put = |key: &'static str| {
        let store = store.clone();
        let ws = ws.id;
        let bucket = bucket.id;
        async move {
            store
                .objects()
                .upsert(&UpsertObject {
                    workspace_id: ws,
                    bucket_id: bucket,
                    key,
                    size: 1,
                    content_type: Some("text/plain"),
                    etag: None,
                    last_modified: now,
                    sha256: None,
                })
                .await
                .expect("upsert object");
        }
    };
    put("docs/a.md").await;
    put("docs/b.md").await;
    put("other/c.md").await;

    // Label docs/a.md under the bucket's store name ("files" — the connection
    // name, the runtime-store naming) + a decoy under a DIFFERENT store name on
    // the same path: the anti-join must match per store, so the decoy hides
    // nothing. A directory label on "docs" must not mark the files under it.
    let author = Author::User { id: UserId::new() };
    let labels = store.object_labels();
    labels
        .add(ws.id, author, "files", "docs/a.md", false, "work")
        .await
        .expect("label a.md");
    labels
        .add(ws.id, author, "elsewhere", "docs/b.md", false, "decoy")
        .await
        .expect("decoy label");
    labels
        .add(ws.id, author, "files", "docs", true, "dir-label")
        .await
        .expect("dir label");

    let mapping = vec![(bucket.id, "files".to_string())];
    let unlabeled = store
        .objects()
        .list_unlabeled_by_workspace(ws.id, &mapping, "", 50)
        .await
        .expect("list unlabeled");
    let mut keys: Vec<&str> = unlabeled.iter().map(|o| o.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["docs/b.md", "other/c.md"],
        "a.md is labelled under its own store; the decoy + dir labels don't count"
    );

    // Subdirectory bound: only the docs/ stragglers.
    let docs_only = store
        .objects()
        .list_unlabeled_by_workspace(ws.id, &mapping, "docs/", 50)
        .await
        .expect("prefix-bounded");
    assert_eq!(docs_only.len(), 1);
    assert_eq!(docs_only[0].key, "docs/b.md");

    // Empty mapping → empty (no query); the limit bounds the page.
    assert!(store
        .objects()
        .list_unlabeled_by_workspace(ws.id, &[], "", 50)
        .await
        .expect("empty mapping")
        .is_empty());
    assert_eq!(
        store
            .objects()
            .list_unlabeled_by_workspace(ws.id, &mapping, "", 1)
            .await
            .expect("capped")
            .len(),
        1
    );

    // Batched label fetch for a summaries page: one query, per-pair grouping.
    let page = store
        .object_labels()
        .list_for_paths(
            ws.id,
            &[
                ("files".to_string(), "docs/a.md".to_string()),
                ("files".to_string(), "docs/b.md".to_string()),
            ],
        )
        .await
        .expect("list_for_paths");
    assert_eq!(page.len(), 1, "only a.md carries a files-store label");
    assert_eq!(page[0].path, "docs/a.md");
    assert_eq!(page[0].label, "work");
    assert!(store
        .object_labels()
        .list_for_paths(ws.id, &[])
        .await
        .expect("empty pairs")
        .is_empty());

    // §18: a foreign workspace sees nothing through the same mapping.
    let other = store
        .workspaces()
        .create("unlbl2", &format!("unlbl2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    assert!(store
        .objects()
        .list_unlabeled_by_workspace(other.id, &mapping, "", 50)
        .await
        .expect("foreign")
        .is_empty());
}
