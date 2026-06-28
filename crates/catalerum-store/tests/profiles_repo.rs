//! Integration test: the `ProfileRepo` contract (SOUL §22, §6.1/§18). An empty
//! default when none exists, JSONB top-level merge (preserve + override), and
//! per-(workspace, user) keying / cross-workspace isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::Map;
use catalerum_core::UserId;
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn fields(pairs: &[(&str, serde_json::Value)]) -> Map {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[tokio::test]
async fn profile_merge_preserves_and_overrides_and_is_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping profile_merge_preserves_and_overrides_and_is_workspace_scoped: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("prof", &format!("prof-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("prof-b", &format!("prof-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let user = UserId::new();

    // No row yet → an empty default profile (no NotFound).
    let empty = store.profiles().get(ws.id, user).await.unwrap();
    assert!(empty.fields.is_empty());
    assert_eq!(empty.user_id, user);

    // First merge creates the row.
    let p = store
        .profiles()
        .merge(
            ws.id,
            user,
            &fields(&[("timezone", serde_json::json!("Europe/Berlin"))]),
        )
        .await
        .unwrap();
    assert_eq!(
        p.fields.get("timezone"),
        Some(&serde_json::json!("Europe/Berlin"))
    );

    // Second merge preserves the existing key and adds a new one.
    let p2 = store
        .profiles()
        .merge(
            ws.id,
            user,
            &fields(&[("focus_hours", serde_json::json!(4))]),
        )
        .await
        .unwrap();
    assert_eq!(
        p2.fields.get("timezone"),
        Some(&serde_json::json!("Europe/Berlin"))
    );
    assert_eq!(p2.fields.get("focus_hours"), Some(&serde_json::json!(4)));

    // An overlapping key overrides (right wins).
    let p3 = store
        .profiles()
        .merge(
            ws.id,
            user,
            &fields(&[("timezone", serde_json::json!("UTC"))]),
        )
        .await
        .unwrap();
    assert_eq!(p3.fields.get("timezone"), Some(&serde_json::json!("UTC")));
    assert_eq!(p3.fields.get("focus_hours"), Some(&serde_json::json!(4)));

    // get reflects the merged state.
    let got = store.profiles().get(ws.id, user).await.unwrap();
    assert_eq!(got.fields, p3.fields);

    // Cross-workspace isolation: the same user in another workspace is empty,
    // and a merge there does not leak back.
    assert!(store
        .profiles()
        .get(other.id, user)
        .await
        .unwrap()
        .fields
        .is_empty());
    store
        .profiles()
        .merge(
            other.id,
            user,
            &fields(&[("locale", serde_json::json!("de"))]),
        )
        .await
        .unwrap();
    let ws_profile = store.profiles().get(ws.id, user).await.unwrap();
    assert!(
        !ws_profile.fields.contains_key("locale"),
        "other workspace's field must not leak"
    );
}
