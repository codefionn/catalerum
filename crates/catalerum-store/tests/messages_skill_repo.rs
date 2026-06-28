//! Integration test: `/<skill>` invocation snapshots on `messages` (SOUL
//! §12/§23). A user turn round-trips its `skill` snapshot ({name, instructions,
//! tools} JSONB — the runbook frozen at invocation time); a plain turn reads
//! back with none.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{MessageRole, Origin, SkillInvocation};
use catalerum_store::{NewMessage, Store};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn user_message_round_trips_skill_snapshot() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping user_message_round_trips_skill_snapshot: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ts", &format!("ts-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Skill"), Origin::Web)
        .await
        .expect("conv");

    let snapshot = SkillInvocation {
        name: "summarize".to_string(),
        instructions: "1. Read the notes.\n2. Write a summary.".to_string(),
        tools: vec!["search_notes".to_string(), "create_note".to_string()],
    };

    // A user turn invoking the skill: the visible content stays the typed
    // command; the snapshot rides the row.
    store
        .messages()
        .insert(&NewMessage {
            skill: Some(&snapshot),
            ..NewMessage::text(conv.id, MessageRole::User, "/summarize the meeting")
        })
        .await
        .expect("insert user with skill");

    // A plain assistant turn carries none.
    store
        .messages()
        .insert(&NewMessage::text(conv.id, MessageRole::Assistant, "done"))
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
    assert_eq!(user.content, "/summarize the meeting");
    assert_eq!(
        user.skill.as_ref(),
        Some(&snapshot),
        "user row round-trips its skill snapshot verbatim"
    );

    let assistant = msgs
        .iter()
        .find(|m| m.role == MessageRole::Assistant)
        .expect("assistant row");
    assert!(assistant.skill.is_none(), "plain rows carry no skill");
}
