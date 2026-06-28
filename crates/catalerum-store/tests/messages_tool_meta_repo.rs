//! Integration test: tool-result metadata persistence on `messages` (SOUL §12).
//! A `tool` row round-trips its `tool_is_error` flag and `tool_duration_ms`, while
//! an ordinary text row keeps the defaults (`false` / `None`). This is what lets a
//! replayed transcript show the same success/error state and timing as the live
//! tool cards.
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
async fn tool_message_round_trips_error_flag_and_duration() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping tool_message_round_trips_error_flag_and_duration: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("tm", &format!("tm-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Tools"), Origin::Web)
        .await
        .expect("conv");

    // A plain text row keeps the defaults.
    store
        .messages()
        .insert(&NewMessage::text(conv.id, MessageRole::User, "run a tool"))
        .await
        .expect("insert user");

    // A failed tool row with a measured duration.
    store
        .messages()
        .insert(&NewMessage {
            conversation_id: conv.id,
            id: None,
            role: MessageRole::Tool,
            content: r#"{"error":"nope"}"#,
            attachments: &[],
            skill: None,
            tool_calls: &[],
            tool_call_id: Some("call-1"),
            tool_is_error: true,
            tool_duration_ms: Some(412),
            usage: None,
        })
        .await
        .expect("insert tool");

    let msgs = store
        .messages()
        .list_by_conversation(conv.id)
        .await
        .expect("list");
    assert_eq!(msgs.len(), 2);

    let user = &msgs[0];
    assert_eq!(user.role, MessageRole::User);
    assert!(!user.tool_is_error, "text row defaults to not-an-error");
    assert_eq!(user.tool_duration_ms, None, "text row has no duration");

    let tool = &msgs[1];
    assert_eq!(tool.role, MessageRole::Tool);
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        tool.tool_is_error,
        "persisted error flag survives the round trip"
    );
    assert_eq!(tool.tool_duration_ms, Some(412));

    // `get` by id sees the same metadata.
    let fetched = store.messages().get(tool.id).await.expect("get tool");
    assert!(fetched.tool_is_error);
    assert_eq!(fetched.tool_duration_ms, Some(412));
}
