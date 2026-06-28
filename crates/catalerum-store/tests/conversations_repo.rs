//! Integration test: the `ConversationRepo` per-conversation **model override**
//! binding (SOUL §7/§12). Exercises migration 0032 — pin/clear round-trips through
//! the setter and a fresh `get`, and the setter is workspace-scoped.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageRole, Origin};
use catalerum_core::ConversationId;
use catalerum_store::{NewMessage, Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn client_chosen_conversation_id_is_idempotent_and_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping client_chosen_conversation_id_is_idempotent_and_workspace_scoped: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("outbox", &format!("outbox-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("outbox-b", &format!("outbox-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let id = ConversationId::new();
    let first = store
        .conversations()
        .create_with_id(ws.id, id, Some("first"), Origin::Web)
        .await
        .expect("first create");
    let retry = store
        .conversations()
        .create_with_id(ws.id, id, Some("ignored retry title"), Origin::Web)
        .await
        .expect("idempotent retry");
    assert_eq!(first.id, retry.id);
    assert_eq!(retry.title.as_deref(), Some("first"));
    assert!(matches!(
        store
            .conversations()
            .create_with_id(other.id, id, Some("foreign"), Origin::Web)
            .await,
        Err(StoreError::NotFound)
    ));
}

/// The chat "model" picker (SOUL §7): a conversation pins, re-pins, and clears a
/// free-form gateway model id; the binding is workspace-scoped (a foreign id is
/// `NotFound`).
#[tokio::test]
async fn conversation_pins_and_clears_a_model() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping conversation_pins_and_clears_a_model: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "model-bind",
            &format!("model-bind-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create(
            "model-bind-b",
            &format!("model-bind-b-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("other ws");

    let conv = store
        .conversations()
        .create(ws.id, Some("t"), Origin::Web)
        .await
        .expect("conv");
    assert!(
        conv.model.is_none(),
        "a fresh conversation has no model override"
    );

    // Pin → round-trips through both the setter and a fresh get.
    let pinned = store
        .conversations()
        .set_model(ws.id, conv.id, Some("openrouter/some-model"))
        .await
        .expect("pin model");
    assert_eq!(pinned.model.as_deref(), Some("openrouter/some-model"));
    let fetched = store
        .conversations()
        .get(ws.id, conv.id)
        .await
        .expect("get");
    assert_eq!(fetched.model.as_deref(), Some("openrouter/some-model"));

    // Re-pin replaces the value in place.
    let repinned = store
        .conversations()
        .set_model(ws.id, conv.id, Some("anthropic/claude-x"))
        .await
        .expect("re-pin");
    assert_eq!(repinned.model.as_deref(), Some("anthropic/claude-x"));

    // Clear with None.
    let cleared = store
        .conversations()
        .set_model(ws.id, conv.id, None)
        .await
        .expect("clear");
    assert!(cleared.model.is_none());

    // Workspace-scoped: another workspace can't touch this conversation's model.
    assert!(matches!(
        store
            .conversations()
            .set_model(other.id, conv.id, Some("x"))
            .await,
        Err(StoreError::NotFound)
    ));
}

/// Auto-compaction state (SOUL §7/§12, migration 0054): the rolling summary +
/// coverage anchor round-trip together, `list_recent_after` replays only what
/// the summary doesn't cover, deleting the anchor row nulls the pointer (the
/// regenerate-invalidation path), and the setter is workspace-scoped.
#[tokio::test]
async fn conversation_summary_round_trips_and_anchor_delete_nulls_it() {
    let Some(url) = test_db_url() else {
        eprintln!("skipping conversation_summary_round_trips_and_anchor_delete_nulls_it: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("compact", &format!("compact-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("compact-b", &format!("compact-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("t"), Origin::Web)
        .await
        .expect("conv");
    assert!(conv.summary.is_none() && conv.summary_upto.is_none());

    // Transcript: u1, a1 (folded) | u2, a2 (kept after the anchor).
    let mut ids = Vec::new();
    for (role, text) in [
        (MessageRole::User, "u1"),
        (MessageRole::Assistant, "a1"),
        (MessageRole::User, "u2"),
        (MessageRole::Assistant, "a2"),
    ] {
        ids.push(
            store
                .messages()
                .insert(&NewMessage::text(conv.id, role, text))
                .await
                .expect("insert")
                .id,
        );
    }
    let anchor = ids[1];

    // Set → round-trips through a fresh get.
    store
        .conversations()
        .set_summary(ws.id, conv.id, Some(("the summary", anchor)))
        .await
        .expect("set summary");
    let fetched = store
        .conversations()
        .get(ws.id, conv.id)
        .await
        .expect("get");
    assert_eq!(fetched.summary.as_deref(), Some("the summary"));
    assert_eq!(fetched.summary_upto, Some(anchor));

    // The seed window replays only messages strictly after the anchor.
    let after = store
        .messages()
        .list_recent_after(conv.id, anchor, 100)
        .await
        .expect("list after");
    assert_eq!(
        after.iter().map(|m| m.content.as_str()).collect::<Vec<_>>(),
        vec!["u2", "a2"]
    );
    // The bound keeps the most recent, still oldest-first.
    let bounded = store
        .messages()
        .list_recent_after(conv.id, anchor, 1)
        .await
        .expect("list after bounded");
    assert_eq!(
        bounded
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>(),
        vec!["a2"]
    );

    // Workspace-scoped setter; clearing with None empties both columns.
    assert!(matches!(
        store
            .conversations()
            .set_summary(other.id, conv.id, Some(("x", anchor)))
            .await,
        Err(StoreError::NotFound)
    ));

    // Deleting the anchor row (what a regenerate's tail-prune does) nulls the
    // pointer via the FK — the half-set pair means "no usable summary".
    store
        .messages()
        .delete(anchor)
        .await
        .expect("delete anchor");
    let stale = store
        .conversations()
        .get(ws.id, conv.id)
        .await
        .expect("get");
    assert_eq!(stale.summary.as_deref(), Some("the summary"));
    assert!(stale.summary_upto.is_none());

    // Explicit clear.
    store
        .conversations()
        .set_summary(ws.id, conv.id, None)
        .await
        .expect("clear");
    let cleared = store
        .conversations()
        .get(ws.id, conv.id)
        .await
        .expect("get");
    assert!(cleared.summary.is_none() && cleared.summary_upto.is_none());
}
