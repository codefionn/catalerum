//! Live end-to-end checks of the chat / embeddings / TTS / STT clients against a
//! running llmleaf. `#[ignore]`d because they need the sibling proxy up; run them
//! with the bundled echo config, whose echo provider serves every modality
//! deterministically:
//!
//! ```sh
//! just llmleaf            # or: cargo run --manifest-path ../llmleaf/Cargo.toml \
//!                         #          -p llmleaf -- config/llmleaf.dev.toml
//! cargo test -p catalerum-llm --test llmleaf_modalities -- --ignored
//! ```
//!
//! Override the endpoint with `LLMLEAF_BASE_URL` / `LLMLEAF_API_KEY` / `LLMLEAF_MODEL`.

use catalerum_llm::{
    Embedder, EmbeddingRequest, OpenRouterClient, SpeechRequest, SpeechSynthesizer, Transcriber,
    TranscriptionRequest,
};

fn client() -> (OpenRouterClient, String) {
    // Origin only — the client appends `/v1/…`; a `…/v1` base double-prefixes.
    let base =
        std::env::var("LLMLEAF_BASE_URL").unwrap_or_else(|_| "http://localhost:8088".to_string());
    // Base64("catalerum-dev:dev-echo-key"): llmleaf's HTTP-Basic-shaped bearer for
    // the bundled echo config (config/llmleaf.dev.toml [[keys]]).
    let key = std::env::var("LLMLEAF_API_KEY")
        .unwrap_or_else(|_| "Y2F0YWxlcnVtLWRldjpkZXYtZWNoby1rZXk=".to_string());
    let model = std::env::var("LLMLEAF_MODEL").unwrap_or_else(|_| "echo".to_string());
    (OpenRouterClient::new(base, key), model)
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn responses_stream_round_trip() {
    use catalerum_core::llm::{ChatMessage, ChatRequest};
    use catalerum_core::stream::{FinishReason, StreamEvent};
    use futures::StreamExt;

    let (c, model) = client();
    let req = ChatRequest::new(&model, vec![ChatMessage::user("hi")]);

    // Streaming (`POST /v1/responses`, typed SSE): text deltas, then one Done
    // carrying the terminal snapshot's finish reason + usage.
    let mut stream = c.stream(req.clone()).await.expect("open stream");
    let mut text = String::new();
    let mut done = None;
    while let Some(ev) = stream.next().await {
        match ev.expect("stream event") {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::Error { message } => panic!("stream error: {message}"),
            StreamEvent::Done {
                finish_reason,
                usage,
            } => done = Some((finish_reason, usage)),
            _ => {}
        }
    }
    assert_eq!(text, "echo: hi");
    let (finish, usage) = done.expect("terminal Done");
    assert_eq!(finish, Some(FinishReason::Stop));
    assert!(usage.is_some(), "terminal snapshot carries usage");

    // Collected convenience folds the same stream.
    let turn = c.chat(req).await.expect("collected chat");
    assert_eq!(turn.content, "echo: hi");
    assert!(!turn.wants_tools());
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn embeddings_round_trip() {
    let (c, model) = client();
    let resp = c
        .embed(EmbeddingRequest::new(
            &model,
            vec!["hello world".into(), "x".into()],
        ))
        .await
        .expect("embed");

    // Echo returns one vector per input, in order: [byte_len, word_count].
    assert_eq!(resp.embeddings.len(), 2);
    assert_eq!(resp.embeddings[0].index, 0);
    assert_eq!(resp.embeddings[0].vector, vec![11.0, 2.0]); // "hello world"
    assert_eq!(resp.embeddings[1].vector, vec![1.0, 1.0]); // "x"
    assert_eq!(resp.dimensions(), Some(2));
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn speech_round_trip() {
    let (c, model) = client();
    let audio = c
        .synthesize(SpeechRequest::new(&model, "hello", "alloy"))
        .await
        .expect("synthesize");

    // Echo TTS returns the input text bytes verbatim, default mp3 MIME.
    assert_eq!(audio.data, b"hello");
    assert_eq!(audio.content_type, "audio/mpeg");
    assert!(!audio.is_empty());
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn transcription_round_trip() {
    let (c, model) = client();
    let resp = c
        .transcribe(TranscriptionRequest::new(
            &model,
            b"abc".to_vec(),
            "clip.wav",
        ))
        .await
        .expect("transcribe");

    // Echo STT echoes the upload's byte length + filename.
    assert_eq!(resp.text, "echo transcript of 3 bytes from clip.wav");
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn list_models_returns_catalog() {
    let (c, _model) = client();
    let models = c
        .list_models(catalerum_llm::ModelKind::All, None)
        .await
        .expect("list_models");
    // The bundled echo config exposes at least its echo model.
    assert!(!models.is_empty());
    assert!(models.iter().any(|m| !m.id.is_empty()));
}

#[tokio::test]
#[ignore = "requires a running llmleaf (just llmleaf)"]
async fn voices_lists_for_speech_model() {
    let (c, model) = client();
    // The call must succeed against a speech-capable model; the voice set itself
    // may be empty for the echo provider.
    let _voices = c.voices(&model).await.expect("voices");
}
