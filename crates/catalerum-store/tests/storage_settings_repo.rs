//! Integration test: per-user default files store preference on `storage_settings`
//! (SOUL §7/§9/§13). An unset record reads back empty (the effective default then
//! falls back to the `[storage]` config); a `set` upserts the choice; a `None`
//! clears it again. Mirrors the search-settings preference contract.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::id::UserId;
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn default_store_upserts_and_clears() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping default_store_upserts_and_clears: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ss", &format!("ss-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();

    // Unset → empty record (no NotFound branch; falls back to config at use).
    let initial = store
        .storage_settings()
        .get(ws.id, user)
        .await
        .expect("get unset");
    assert!(
        initial.default_store.is_none(),
        "an unset default store reads back as None"
    );

    // Set → upserts and returns the stored choice.
    let saved = store
        .storage_settings()
        .set(ws.id, user, Some("archive"))
        .await
        .expect("set");
    assert_eq!(saved.default_store.as_deref(), Some("archive"));
    let reread = store
        .storage_settings()
        .get(ws.id, user)
        .await
        .expect("get after set");
    assert_eq!(reread.default_store.as_deref(), Some("archive"));

    // A second set overwrites (full upsert).
    let updated = store
        .storage_settings()
        .set(ws.id, user, Some("uploads"))
        .await
        .expect("update");
    assert_eq!(updated.default_store.as_deref(), Some("uploads"));

    // None clears the override.
    let cleared = store
        .storage_settings()
        .set(ws.id, user, None)
        .await
        .expect("clear");
    assert!(
        cleared.default_store.is_none(),
        "clearing the default store reads back as None"
    );
}
