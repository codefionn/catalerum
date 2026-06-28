//! Integration test: background memory auto-curation (SOUL §22).
//!
//! Proves the contract: an `extract_memories` job, run by a curate-capable
//! worker, mines a conversation via the (here faked) LLM, **dedups** the proposed
//! facts against the user's existing memories, and stores the new ones. A second
//! run creates nothing (idempotent against duplicates).
//!
//! Requires a Postgres. Set `CATALERUM_TEST_DATABASE_URL` (or `DATABASE_URL`); the
//! LLM is an in-test fake, so no llmleaf is needed and the assertions are stable.

mod common;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, BoxStream};
use futures::StreamExt;

use catalerum_core::error::Result as CoreResult;
use catalerum_core::llm::ChatRequest;
use catalerum_core::model::{MemoryScope, MessageRole, Origin};
use catalerum_core::provider::LlmClient;
use catalerum_core::stream::StreamEvent;
use catalerum_core::UserId;
use catalerum_ingest::{enqueue_extract_memories, CurateContext, SyncWorker};
use catalerum_store::{JobStatus, NewMessage, Store};

/// A fake [`LlmClient`] that streams a fixed reply (used as the extraction JSON).
struct FakeLlm {
    reply: String,
}

#[async_trait::async_trait]
impl LlmClient for FakeLlm {
    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> CoreResult<BoxStream<'static, CoreResult<StreamEvent>>> {
        let events = vec![
            Ok(StreamEvent::TextDelta {
                text: self.reply.clone(),
            }),
            Ok(StreamEvent::Done {
                finish_reason: None,
                usage: None,
            }),
        ];
        Ok(stream::iter(events).boxed())
    }
}

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

async fn drain(store: &Store, worker: &SyncWorker, job: uuid::Uuid) {
    for _ in 0..40 {
        let row = store.job_queue().get(job).await.expect("get job");
        if matches!(row.status().unwrap(), JobStatus::Done | JobStatus::Failed) {
            assert_eq!(
                row.status().unwrap(),
                JobStatus::Done,
                "last_error={:?}",
                row.last_error
            );
            return;
        }
        if !worker.poll_once().await.expect("poll") {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    panic!("extract_memories job {job} did not finish");
}

#[tokio::test]
async fn worker_extracts_and_dedups_memories_from_a_conversation() {
    let Some(db) = test_db_url() else {
        eprintln!("skipping curate test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = common::isolated_store(&db).await;
    let ws = store
        .workspaces()
        .create("curate", &format!("curate-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();
    let convo = store
        .conversations()
        .create(ws.id, Some("chat"), Origin::Web)
        .await
        .expect("conversation");

    // A short exchange to mine.
    for (role, text) in [
        (
            MessageRole::User,
            "I hike every weekend and I'm vegetarian.",
        ),
        (MessageRole::Assistant, "Got it — noted!"),
    ] {
        store
            .messages()
            .insert(&NewMessage::text(convo.id, role, text))
            .await
            .expect("message");
    }

    // Pre-seed an existing memory that one extracted fact will duplicate.
    store
        .memories()
        .create(ws.id, MemoryScope::User, Some(user), "is vegetarian", None)
        .await
        .expect("seed memory");

    // The fake model proposes two facts; one is already known.
    let reply = r#"["enjoys hiking every weekend", "is vegetarian"]"#.to_string();
    let worker = SyncWorker::new(store.clone())
        .with_curate_context(CurateContext::new(Arc::new(FakeLlm { reply }), "fake"));

    let job = enqueue_extract_memories(&store, ws.id, convo.id, user)
        .await
        .expect("enqueue");
    drain(&store, &worker, job).await;

    // The new fact was stored; the duplicate was not re-created.
    let mems: Vec<String> = store
        .memories()
        .list_visible(ws.id, Some(user), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(
        mems.len(),
        2,
        "1 pre-seeded + 1 new (the duplicate was deduped): {mems:?}"
    );
    assert!(mems.contains(&"enjoys hiking every weekend".to_string()));
    assert!(mems.contains(&"is vegetarian".to_string()));

    // Re-running extracts nothing new (both facts now exist).
    let job2 = enqueue_extract_memories(&store, ws.id, convo.id, user)
        .await
        .expect("enqueue 2");
    drain(&store, &worker, job2).await;
    assert_eq!(
        store
            .memories()
            .list_visible(ws.id, Some(user), 50)
            .await
            .unwrap()
            .len(),
        2,
        "re-running creates no duplicates"
    );
}
