//! Integration test: the `MemoryRepo` contract (SOUL §22, §6.1/§18). CRUD plus
//! the two scopes (`User`/`Workspace`) and the **user-visibility filter** — a
//! member sees workspace memories and their own private ones, never another
//! member's — and cross-workspace isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::MemoryScope;
use catalerum_core::{MemoryId, UserId};
use catalerum_store::{Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn memory_crud_scopes_and_visibility_are_workspace_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping memory_crud_scopes_and_visibility_are_workspace_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mem", &format!("mem-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("mem-b", &format!("mem-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let alice = UserId::new();
    let bob = UserId::new();

    // A workspace memory, Alice's private memory, Bob's private memory.
    let shared = store
        .memories()
        .create(
            ws.id,
            MemoryScope::Workspace,
            Some(alice),
            "ships on Fridays",
            None,
        )
        .await
        .expect("shared");
    // Workspace scope ignores the user_id.
    assert_eq!(shared.user_id, None);
    let a_mem = store
        .memories()
        .create(
            ws.id,
            MemoryScope::User,
            Some(alice),
            "prefers morning meetings",
            None,
        )
        .await
        .expect("alice mem");
    assert_eq!(a_mem.user_id, Some(alice));
    store
        .memories()
        .create(ws.id, MemoryScope::User, Some(bob), "is in CET", None)
        .await
        .expect("bob mem");

    // Alice sees the shared + her own, not Bob's.
    let visible_a: Vec<_> = store
        .memories()
        .list_visible(ws.id, Some(alice), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(visible_a.len(), 2);
    assert!(visible_a.contains(&"ships on Fridays".to_string()));
    assert!(visible_a.contains(&"prefers morning meetings".to_string()));
    assert!(!visible_a.contains(&"is in CET".to_string()));

    // An agent run (no user) sees only the workspace memory.
    let visible_none = store
        .memories()
        .list_visible(ws.id, None, 50)
        .await
        .unwrap();
    assert_eq!(visible_none.len(), 1);
    assert_eq!(visible_none[0].text, "ships on Fridays");

    // Batched get_many: returns the matching rows (any order), ignores an absent
    // id, stays workspace-scoped, and treats an empty input as empty.
    let many = store
        .memories()
        .get_many(ws.id, &[shared.id, a_mem.id, MemoryId::new()])
        .await
        .unwrap();
    assert_eq!(
        many.len(),
        2,
        "two real ids resolve, the random one is absent"
    );
    let texts: Vec<&str> = many.iter().map(|m| m.text.as_str()).collect();
    assert!(texts.contains(&"ships on Fridays"));
    assert!(texts.contains(&"prefers morning meetings"));
    // §18: another workspace can't reach these ids.
    assert!(store
        .memories()
        .get_many(other.id, &[shared.id, a_mem.id])
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .memories()
        .get_many(ws.id, &[])
        .await
        .unwrap()
        .is_empty());

    // get + update + delete round-trip.
    assert_eq!(
        store.memories().get(ws.id, a_mem.id).await.unwrap().text,
        "prefers morning meetings"
    );
    let updated = store
        .memories()
        .update_text(ws.id, a_mem.id, "prefers afternoon meetings")
        .await
        .unwrap();
    assert_eq!(updated.text, "prefers afternoon meetings");
    store.memories().delete(ws.id, a_mem.id).await.unwrap();
    assert!(matches!(
        store.memories().get(ws.id, a_mem.id).await,
        Err(StoreError::NotFound)
    ));

    // Cross-workspace isolation: `other` sees none of `ws`'s memories, and can't
    // get/delete one of them.
    assert!(store
        .memories()
        .list_visible(other.id, Some(alice), 50)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        store.memories().get(other.id, shared.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.memories().delete(other.id, shared.id).await,
        Err(StoreError::NotFound)
    ));
}
