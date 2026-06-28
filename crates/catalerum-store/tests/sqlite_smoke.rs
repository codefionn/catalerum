#![cfg(feature = "sqlite")]

use catalerum_core::model::Origin;
use catalerum_store::Store;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn sqlite_migrates_and_runs_core_repositories() {
    let path = std::env::temp_dir().join(format!("catalerum-store-{}.db", uuid::Uuid::new_v4()));
    let url = format!("sqlite://{}", path.display());

    let store = Store::connect(&url).await.expect("migrate SQLite store");
    let default_org = store
        .organisations()
        .get_by_slug("default")
        .await
        .expect("default organisation");
    let user = store
        .users()
        .create("admin@example.test", "Admin", None)
        .await
        .expect("create user");
    let workspace = store
        .workspaces()
        .create_in_org(default_org.id, "Home", "home")
        .await
        .expect("create workspace");
    store
        .memberships()
        .upsert(workspace.id, user.id, catalerum_core::Role::Owner)
        .await
        .expect("create membership");
    let conversation = store
        .conversations()
        .create(workspace.id, Some("hello"), Origin::Web)
        .await
        .expect("create conversation");

    assert_eq!(conversation.workspace_id, workspace.id);
    assert!(store.password_auth().setup_required().await.unwrap());
    let boot = store
        .password_auth()
        .bootstrap("owner@example.test", "Owner", "$argon2id$test")
        .await
        .expect("atomic first boot");
    assert!(!store.password_auth().setup_required().await.unwrap());
    let account = store
        .password_auth()
        .get_by_email("OWNER@example.test")
        .await
        .expect("password account");
    assert_eq!(account.user_id, boot.user_id.into_uuid());
    assert_eq!(account.workspace_id, boot.workspace_id.into_uuid());
    assert!(store
        .password_auth()
        .bootstrap("second@example.test", "Second", "$argon2id$test")
        .await
        .is_err());

    // The all-in-one worker uses the durable SQL queue without Valkey. Exercise
    // its SQLite-native atomic claim, retry, and completion path here.
    let queued = store
        .job_queue()
        .enqueue(
            Some(workspace.id),
            "sqlite-smoke",
            json!({"ok": true}),
            None,
        )
        .await
        .expect("enqueue job");
    let claimed = store
        .job_queue()
        .dequeue_one("single-node-worker")
        .await
        .expect("claim job")
        .expect("queued job is runnable");
    assert_eq!(claimed.id, queued.id);
    store
        .job_queue()
        .fail(claimed.id, "retry", 2, Duration::ZERO)
        .await
        .expect("requeue failed job");
    let retried = store
        .job_queue()
        .dequeue_one("single-node-worker")
        .await
        .expect("claim retry")
        .expect("retry is immediately runnable");
    let completed = store
        .job_queue()
        .complete(retried.id)
        .await
        .expect("complete retry");
    assert_eq!(completed.status.as_str(), "done");
    store.ping().await.expect("ping SQLite store");
    drop(store);
    let _ = std::fs::remove_file(path);
}
