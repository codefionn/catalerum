//! Integration test: durable `ask_user` Q&A storage (SOUL §7/§12). A pending
//! question resolves **with** the user's structured answers (the form reply) and
//! reads them back; a supersede-style resolve (typed past the form / a fresh
//! `ask_user`) leaves the answers NULL; `list_for_conversation` returns the full
//! Q&A history oldest-first.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::ask::{Answer, Question};
use catalerum_core::model::Origin;
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn question(id: &str, text: &str, options: &[&str]) -> Question {
    Question {
        id: id.to_string(),
        text: text.to_string(),
        options: options.iter().map(|s| (*s).to_string()).collect(),
        multiple: false,
        allow_text: true,
    }
}

#[tokio::test]
async fn question_resolves_with_answers_and_lists_history() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping question_resolves_with_answers_and_lists_history: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("pq", &format!("pq-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conv = store
        .conversations()
        .create(ws.id, Some("Questions"), Origin::Web)
        .await
        .expect("conv");

    // 1. First form: superseded unanswered (the user typed past it) — the
    //    resolve carries no answers and the row must keep NULL.
    store
        .pending_questions()
        .create(
            ws.id,
            conv.id,
            &[question("q1", "Which tone?", &["a", "b"])],
        )
        .await
        .expect("create first");
    let closed = store
        .pending_questions()
        .resolve_for_conversation(ws.id, conv.id, None)
        .await
        .expect("supersede");
    assert_eq!(closed, 1);

    // 2. Second form: answered via the form — the structured answers are stamped
    //    onto the row the resolve closes and round-trip verbatim.
    let created = store
        .pending_questions()
        .create(
            ws.id,
            conv.id,
            &[
                question("tone", "Which tone?", &["formal", "casual"]),
                question("name", "Your name?", &[]),
            ],
        )
        .await
        .expect("create second");
    assert!(created.answers.is_none(), "a fresh question has no answers");
    let answers = vec![
        Answer {
            id: "tone".to_string(),
            selected: vec!["formal".to_string()],
            text: None,
        },
        Answer {
            id: "name".to_string(),
            selected: Vec::new(),
            text: Some("Ada".to_string()),
        },
    ];
    let closed = store
        .pending_questions()
        .resolve_for_conversation(ws.id, conv.id, Some(&answers))
        .await
        .expect("resolve with answers");
    assert_eq!(closed, 1);
    assert!(
        store
            .pending_questions()
            .get_unresolved(ws.id, conv.id)
            .await
            .expect("get_unresolved")
            .is_none(),
        "nothing stays pending after the resolve"
    );

    // 3. The history lists both forms oldest-first: the superseded one without
    //    answers, the answered one with them, verbatim.
    let history = store
        .pending_questions()
        .list_for_conversation(ws.id, conv.id)
        .await
        .expect("list history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].questions[0].id, "q1");
    assert!(history[0].resolved_at.is_some());
    assert!(
        history[0].answers.is_none(),
        "a superseded question was never answered — answers stay NULL"
    );
    assert_eq!(history[1].id, created.id);
    assert!(history[1].resolved_at.is_some());
    assert_eq!(
        history[1].answers.as_deref(),
        Some(answers.as_slice()),
        "the answered form round-trips its structured answers verbatim"
    );

    // 4. Re-resolving is a no-op (idempotent) and must not clobber the stored
    //    answers with NULL.
    let closed = store
        .pending_questions()
        .resolve_for_conversation(ws.id, conv.id, None)
        .await
        .expect("re-resolve");
    assert_eq!(closed, 0, "nothing unresolved is left to close");
    let history = store
        .pending_questions()
        .list_for_conversation(ws.id, conv.id)
        .await
        .expect("re-list");
    assert_eq!(
        history[1].answers.as_deref(),
        Some(answers.as_slice()),
        "an idempotent re-resolve leaves stored answers untouched"
    );
}
