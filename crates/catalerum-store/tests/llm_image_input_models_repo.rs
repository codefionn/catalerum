//! Integration test: per-user force-image-input model list on `llm_settings`
//! (SOUL §7/§9). An unset record reads back as an empty list; `set_image_input_models`
//! upserts it; and — the property that matters — the list writer and the model/voice
//! writer (`set`) are column-scoped, so neither clobbers the other's columns.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::id::UserId;
use catalerum_store::Store;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn image_input_models_upserts_and_coexists_with_model_selections() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping image_input_models_upserts_and_coexists_with_model_selections: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("li", &format!("li-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let user = UserId::new();

    // Unset → empty list (no NotFound branch; the gate falls back to the catalog).
    let initial = store
        .llm_settings()
        .get(ws.id, user)
        .await
        .expect("get unset");
    assert!(
        initial.image_input_models.is_empty(),
        "an unset force-image list reads back empty"
    );

    // Set the list → upserts and returns it in order.
    let saved = store
        .llm_settings()
        .set_image_input_models(ws.id, user, &["gpt-x".into(), "claude-y".into()])
        .await
        .expect("set image models");
    assert_eq!(
        saved.image_input_models,
        vec!["gpt-x".to_string(), "claude-y".to_string()]
    );

    // Setting a chat model via `set` must NOT disturb the image list (the two
    // writers touch disjoint columns).
    let with_model = store
        .llm_settings()
        .set(
            ws.id,
            user,
            Some("chatty"),
            None,
            None,
            None,
            1.5,
            Some("vision-x"),
        )
        .await
        .expect("set chat model");
    assert_eq!(with_model.chat_model.as_deref(), Some("chatty"));
    assert_eq!(with_model.ocr_model.as_deref(), Some("vision-x"));
    assert_eq!(
        with_model.image_input_models,
        vec!["gpt-x".to_string(), "claude-y".to_string()],
        "set() leaves the force-image list intact"
    );

    // …and replacing the image list must NOT clear the chat model.
    let relisted = store
        .llm_settings()
        .set_image_input_models(ws.id, user, &["only-this".into()])
        .await
        .expect("replace image models");
    assert_eq!(relisted.image_input_models, vec!["only-this".to_string()]);
    assert_eq!(
        relisted.chat_model.as_deref(),
        Some("chatty"),
        "set_image_input_models leaves the chat model intact"
    );
    assert_eq!(
        relisted.ocr_model.as_deref(),
        Some("vision-x"),
        "set_image_input_models leaves the OCR model intact"
    );

    // An empty list clears the override.
    let cleared = store
        .llm_settings()
        .set_image_input_models(ws.id, user, &[])
        .await
        .expect("clear image models");
    assert!(
        cleared.image_input_models.is_empty(),
        "clearing the force-image list reads back empty"
    );
}
