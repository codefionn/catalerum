//! Integration test: reasoning persistence on `messages` (SOUL §7/§12).
//! An assistant row round-trips its reasoning blocks, signature included, with
//! the model that produced them. A conversation resumed from the store replays
//! these to the model, which rejects a block whose signature is gone.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageReasoning, MessageRole, Origin};
use catalerum_core::stream::ReasoningDetail;
use catalerum_store::{NewMessage, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn assistant_message_round_trips_signed_reasoning() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping assistant_message_round_trips_signed_reasoning: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mr", &format!("mr-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Reasoning"), Origin::Web)
        .await
        .expect("conv");

    // A row without reasoning stores NULL and reads back `None`.
    let user = store
        .messages()
        .insert(&NewMessage::text(ws.id, conv.id, MessageRole::User, "think"))
        .await
        .expect("insert user");
    assert_eq!(user.reasoning, None);

    let details = vec![
        ReasoningDetail {
            kind: "reasoning.text".into(),
            text: Some(String::new()),
            signature: Some("sig-1".into()),
            id: Some("rs_1_0".into()),
            index: Some(0),
            ..Default::default()
        },
        ReasoningDetail {
            kind: "reasoning.encrypted".into(),
            data: Some("opaque".into()),
            index: Some(1),
            ..Default::default()
        },
    ];
    let reasoning = MessageReasoning::new("anthropic/claude-opus-5-5", &details).expect("some");
    let mut new = NewMessage::text(ws.id, conv.id, MessageRole::Assistant, "done");
    new.reasoning = Some(&reasoning);
    let inserted = store.messages().insert(&new).await.expect("insert assistant");
    assert_eq!(inserted.reasoning.as_ref(), Some(&reasoning));

    // Every read path returns it: by id, the full list, and the recent window.
    let got = store.messages().get(ws.id, inserted.id).await.expect("get");
    assert_eq!(got.reasoning.as_ref(), Some(&reasoning));
    let all = store
        .messages()
        .list_by_conversation(ws.id, conv.id)
        .await
        .expect("list");
    let listed = all.iter().find(|m| m.id == inserted.id).expect("listed");
    assert_eq!(listed.reasoning.as_ref(), Some(&reasoning));
    let recent = store
        .messages()
        .list_recent(ws.id, conv.id, 10)
        .await
        .expect("recent");
    let listed = recent.iter().find(|m| m.id == inserted.id).expect("recent");
    assert_eq!(listed.reasoning.as_ref(), Some(&reasoning));

    // No details means nothing to store.
    assert_eq!(MessageReasoning::new("m", &[]), None);
}
