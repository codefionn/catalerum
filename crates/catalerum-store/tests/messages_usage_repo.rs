//! Integration test: per-turn token + cost usage persistence on `messages`
//! (SOUL §7/§12). The final assistant message of an exchange round-trips its
//! summed [`Usage`](catalerum_core::stream::Usage); user/tool rows keep `None`.
//! This is what lets a replayed transcript show the same token info-icon / cost
//! readout the live `message_done` frame did, instead of losing it on reload.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageRole, Origin};
use catalerum_core::stream::Usage;
use catalerum_store::{NewMessage, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn assistant_message_round_trips_token_and_cost_usage() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping assistant_message_round_trips_token_and_cost_usage: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("tu", &format!("tu-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Usage"), Origin::Web)
        .await
        .expect("conv");

    // A user turn carries no usage.
    store
        .messages()
        .insert(&NewMessage::text(conv.id, MessageRole::User, "hello"))
        .await
        .expect("insert user");

    // The answering assistant turn carries the exchange's summed usage.
    let usage = Usage {
        prompt_tokens: 1200,
        completion_tokens: 340,
        total_tokens: 1540,
        cost_usd: Some(0.0123),
        cached_tokens: 800,
        cache_creation_tokens: 64,
    };
    store
        .messages()
        .insert(&NewMessage {
            conversation_id: conv.id,
            id: None,
            role: MessageRole::Assistant,
            content: "hi there",
            attachments: &[],
            skill: None,
            tool_calls: &[],
            tool_call_id: None,
            tool_is_error: false,
            tool_duration_ms: None,
            usage: Some(usage),
        })
        .await
        .expect("insert assistant");

    let msgs = store
        .messages()
        .list_by_conversation(conv.id)
        .await
        .expect("list");

    let user = msgs
        .iter()
        .find(|m| m.role == MessageRole::User)
        .expect("user row");
    assert!(user.usage.is_none(), "user rows carry no usage");

    let assistant = msgs
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("assistant row");
    assert_eq!(
        assistant.usage,
        Some(usage),
        "assistant row round-trips the full token + cost accounting"
    );
}
