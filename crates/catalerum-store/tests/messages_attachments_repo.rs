//! Integration test: chat-message file/image attachment references on `messages`
//! (SOUL §9/§12). A user turn round-trips its `attachments` (the bytes live in a
//! files store; only the reference rides on the row); assistant/tool rows keep an
//! empty list, and a plain `NewMessage::text` user turn reads back with none.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{Attachment, MessageRole, Origin};
use catalerum_store::{NewMessage, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn user_message_round_trips_attachment_references() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping user_message_round_trips_attachment_references: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ta", &format!("ta-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Attach"), Origin::Web)
        .await
        .expect("conv");

    let attachments = vec![
        Attachment {
            url: "/storage/objects/chat/report.pdf".to_string(),
            filename: Some("report.pdf".to_string()),
            content_type: Some("application/pdf".to_string()),
            size: Some(2048),
        },
        Attachment {
            url: "https://example.com/diagram.png".to_string(),
            filename: Some("diagram.png".to_string()),
            content_type: Some("image/png".to_string()),
            size: None,
        },
    ];

    // A user turn carrying attachment references.
    store
        .messages()
        .insert(&NewMessage {
            attachments: &attachments,
            ..NewMessage::text(conv.id, MessageRole::User, "here are the files")
        })
        .await
        .expect("insert user with attachments");

    // A plain assistant turn carries none.
    store
        .messages()
        .insert(&NewMessage::text(conv.id, MessageRole::Assistant, "thanks"))
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
    assert_eq!(
        user.attachments, attachments,
        "user row round-trips its attachment references verbatim"
    );

    let assistant = msgs
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("assistant row");
    assert!(
        assistant.attachments.is_empty(),
        "assistant rows carry no attachments"
    );
}
