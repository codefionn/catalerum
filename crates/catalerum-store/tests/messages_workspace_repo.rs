//! Integration test: `messages.workspace_id` tenancy (SOUL §3.14/§6.1/§18).
//! A message stored in workspace A carries A's id and is unreachable through
//! every `MessageRepo` method when called with workspace B's id: reads come
//! back NotFound or empty, writes and deletes touch nothing, and an insert
//! naming A's conversation under B's workspace is refused.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageRole, Origin};
use catalerum_core::stream::Usage;
use catalerum_store::{NewMessage, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn messages_are_unreachable_from_another_workspace() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping messages_are_unreachable_from_another_workspace: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let a = store
        .workspaces()
        .create("mw-a", &format!("mw-a-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws a");
    let b = store
        .workspaces()
        .create("mw-b", &format!("mw-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws b");
    let conv = store
        .conversations()
        .create(a.id, Some("A"), Origin::Web)
        .await
        .expect("conv a");
    let msgs = store.messages();

    // The insert stamps the conversation's workspace onto the row.
    let first = msgs
        .insert(&NewMessage::text(
            a.id,
            conv.id,
            MessageRole::User,
            "alpha secret",
        ))
        .await
        .expect("insert u1");
    assert_eq!(first.workspace_id, a.id);
    let second = msgs
        .insert(&NewMessage::text(
            a.id,
            conv.id,
            MessageRole::Assistant,
            "reply",
        ))
        .await
        .expect("insert a1");

    // Writing into A's conversation while claiming workspace B is refused, and
    // leaves no row behind.
    assert!(matches!(
        msgs.insert(&NewMessage::text(
            b.id,
            conv.id,
            MessageRole::User,
            "intruder"
        ))
        .await,
        Err(StoreError::NotFound)
    ));

    // Reads through B see nothing.
    assert!(matches!(
        msgs.get(b.id, first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(msgs
        .list_by_conversation(b.id, conv.id)
        .await
        .expect("list b")
        .is_empty());
    assert!(msgs
        .list_recent(b.id, conv.id, 50)
        .await
        .expect("recent b")
        .is_empty());
    assert!(msgs
        .list_recent_after(b.id, conv.id, first.id, 50)
        .await
        .expect("recent after b")
        .is_empty());
    assert!(msgs
        .search_in_workspace(b.id, "alpha", 50)
        .await
        .expect("search b")
        .is_empty());

    // Writes and deletes through B touch nothing.
    let usage = Usage {
        prompt_tokens: 1,
        completion_tokens: 1,
        total_tokens: 2,
        ..Default::default()
    };
    assert!(matches!(
        msgs.set_usage(b.id, second.id, Some(usage)).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        msgs.delete(b.id, first.id).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        msgs.delete_after(b.id, conv.id, first.id)
            .await
            .expect("delete_after b"),
        0
    );

    // A still sees both rows, unchanged.
    let in_a = msgs
        .list_by_conversation(a.id, conv.id)
        .await
        .expect("list a");
    assert_eq!(
        in_a.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec!["alpha secret", "reply"]
    );
    assert!(in_a.iter().all(|m| m.workspace_id == a.id));
    assert!(in_a[1].usage.is_none(), "B's set_usage must not land");
    assert_eq!(
        msgs.get(a.id, first.id).await.expect("get a").content,
        "alpha secret"
    );
    assert_eq!(
        msgs.search_in_workspace(a.id, "alpha", 50)
            .await
            .expect("search a")
            .len(),
        1
    );

    // And A's own scoped mutations work.
    msgs.set_usage(a.id, second.id, Some(usage))
        .await
        .expect("set_usage a");
    assert_eq!(
        msgs.delete_after(a.id, conv.id, first.id)
            .await
            .expect("delete_after a"),
        1
    );
    msgs.delete(a.id, first.id).await.expect("delete a");
    assert!(msgs
        .list_by_conversation(a.id, conv.id)
        .await
        .expect("list a after")
        .is_empty());
}
