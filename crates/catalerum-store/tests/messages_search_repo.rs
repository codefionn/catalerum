//! Integration test: `MessageRepo::search_in_workspace` (SOUL §12/§6.1/§18) —
//! case-insensitive literal-substring content search, newest-first, carrying each
//! hit's conversation title, **workspace-scoped** (a match in another workspace is
//! never returned), and a blank query returning nothing.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageRole, Origin};
use catalerum_store::{NewMessage, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn message_search_is_substring_scoped_and_titled() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping message_search_is_substring_scoped_and_titled: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("msg", &format!("msg-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("msg-b", &format!("msg-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let conv = store
        .conversations()
        .create(ws.id, Some("Trip planning"), Origin::Web)
        .await
        .expect("conv");
    let other_conv = store
        .conversations()
        .create(other.id, Some("Other"), Origin::Web)
        .await
        .expect("other conv");

    for (role, content) in [
        (MessageRole::User, "Find me a flight to Berlin"),
        (MessageRole::Assistant, "Booked the BERLIN trip"),
        (MessageRole::User, "What about Paris?"),
    ] {
        store
            .messages()
            .insert(&NewMessage::text(conv.id, role, content))
            .await
            .expect("insert");
    }
    // A matching message in ANOTHER workspace — must never surface here.
    store
        .messages()
        .insert(&NewMessage::text(
            other_conv.id,
            MessageRole::User,
            "secret berlin plans",
        ))
        .await
        .expect("insert other");

    // Case-insensitive substring: "berlin" matches the two Berlin messages in `ws`
    // (not the Paris one, not the other workspace's), newest match first.
    let hits = store
        .messages()
        .search_in_workspace(ws.id, "berlin", 50)
        .await
        .expect("search");
    assert_eq!(hits.len(), 2, "two in-workspace Berlin matches");
    assert!(hits
        .iter()
        .all(|h| h.message.content.to_lowercase().contains("berlin")));
    // Each hit carries its conversation's title.
    assert!(hits
        .iter()
        .all(|h| h.conversation_title.as_deref() == Some("Trip planning")));
    // Newest-first ordering (the assistant "Booked…" was inserted after the user ask).
    assert_eq!(hits[0].message.content, "Booked the BERLIN trip");

    // The limit bounds the result set.
    let one = store
        .messages()
        .search_in_workspace(ws.id, "berlin", 1)
        .await
        .expect("search limit");
    assert_eq!(one.len(), 1);

    // A blank query returns nothing (no "match everything").
    assert!(store
        .messages()
        .search_in_workspace(ws.id, "   ", 50)
        .await
        .expect("blank")
        .is_empty());

    // A `%` in the query is matched literally (no LIKE wildcard), so it finds nothing.
    assert!(store
        .messages()
        .search_in_workspace(ws.id, "%", 50)
        .await
        .expect("literal pct")
        .is_empty());
}
