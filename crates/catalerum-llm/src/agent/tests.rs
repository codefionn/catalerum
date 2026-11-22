use super::dispatch::run_tool;
use super::*;
use async_trait::async_trait;
use catalerum_core::tool::{Tool, MODEL_MEDIA_RESULT_FIELD};
use std::sync::Arc;

/// A context holding full authority (`*:*`): these tests exercise the agent loop,
/// not the capability check, and dispatch refuses a context without capabilities.
fn full_authority() -> ToolContext {
    ToolContext {
        capabilities: Some(vec![catalerum_core::capability::Capability::new(
            catalerum_core::capability::Action::Any,
            catalerum_core::capability::Resource::any(),
        )]),
        ..ToolContext::default()
    }
}

#[test]
fn config_default() {
    let config = AgentConfig::default();
    assert_eq!(config.max_iterations, 256);
    assert_eq!(config.max_identical_tool_calls, 3);
    assert_eq!(config.max_consecutive_tool_errors, 8);
}

#[test]
fn assistant_and_tool_messages() {
    let turn = CollectedTurn {
        content: "hi".into(),
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "f".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    };
    let am = assistant_message(&turn);
    assert_eq!(am.role, MessageRole::Assistant);
    assert_eq!(am.tool_calls.len(), 1);

    let tm = tool_message(&turn.tool_calls[0], "{\"ok\":true}");
    assert_eq!(tm.role, MessageRole::Tool);
    assert_eq!(tm.tool_call_id.as_deref(), Some("c1"));
}

#[test]
fn value_to_string_passes_strings() {
    assert_eq!(value_to_string(&serde_json::json!("x")), "x");
    assert_eq!(value_to_string(&serde_json::json!({"a":1})), "{\"a\":1}");
}

#[test]
fn accumulates_usage() {
    let mut acc = None;
    accumulate_usage(
        &mut acc,
        Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            cost_usd: Some(0.5),
            cached_tokens: 1,
            cache_creation_tokens: 2,
        }),
    );
    accumulate_usage(
        &mut acc,
        Some(Usage {
            prompt_tokens: 4,
            completion_tokens: 5,
            total_tokens: 9,
            cost_usd: Some(1.5),
            cached_tokens: 3,
            cache_creation_tokens: 4,
        }),
    );
    assert_eq!(
        acc,
        Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 7,
            total_tokens: 12,
            cost_usd: Some(2.0),
            cached_tokens: 4,
            cache_creation_tokens: 6,
        })
    );
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        Ok(args)
    }
}

#[tokio::test]
async fn dispatch_one_success_and_error() {
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();

    let ok = dispatch_one(
        &reg,
        &ctx,
        &ToolCall {
            id: "1".into(),
            name: "echo".into(),
            arguments: "{\"a\":1}".into(),
        },
        &[],
    )
    .await;
    assert!(!ok.is_error);
    assert_eq!(ok.result, "{\"a\":1}");

    let missing = dispatch_one(
        &reg,
        &ctx,
        &ToolCall {
            id: "2".into(),
            name: "nope".into(),
            arguments: "{}".into(),
        },
        &[],
    )
    .await;
    assert!(missing.is_error);
    assert!(missing.result.contains("error"));

    let bad_args = dispatch_one(
        &reg,
        &ctx,
        &ToolCall {
            id: "3".into(),
            name: "echo".into(),
            arguments: "{not json".into(),
        },
        &[],
    )
    .await;
    assert!(bad_args.is_error);
}

struct NativeMediaTool;

#[async_trait]
impl Tool for NativeMediaTool {
    fn name(&self) -> &str {
        "native_media"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        panic!("media tool must receive trusted model capabilities")
    }
    async fn invoke_for_model(
        &self,
        _args: Json,
        _ctx: &ToolContext,
        input_modalities: &[String],
    ) -> Result<Json> {
        assert_eq!(input_modalities, ["image"]);
        Ok(serde_json::json!({
            "size": 4,
            MODEL_MEDIA_RESULT_FIELD: [{
                "type": "image",
                "url": "data:image/png;base64,AA==",
            }],
        }))
    }
}

#[tokio::test]
async fn native_media_is_removed_from_textual_tool_result() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NativeMediaTool));
    let call = ToolCall {
        id: "media-1".into(),
        name: "native_media".into(),
        arguments: r#"{"__catalerum_model_input_modalities":["spoofed"]}"#.into(),
    };
    let (result, is_error, media) =
        run_tool(&registry, &full_authority(), &call, &["image".into()]).await;
    assert!(!is_error);
    assert_eq!(result, r#"{"size":4}"#);
    assert_eq!(
        media,
        vec![MediaInput::Image {
            url: "data:image/png;base64,AA==".into()
        }]
    );
}

// ---- streaming loop -------------------------------------------------

use catalerum_core::stream::StreamEvent;
use futures::StreamExt;
use std::collections::VecDeque;
use std::sync::Mutex;

fn tool_turn(id: &str, name: &str, arguments: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some(id.into()),
            name: Some(name.into()),
            arguments: Some(arguments.into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ]
}

/// A scripted [`TurnStreamer`]: yields a pre-baked event stream per call and
/// records each request's message count + advertised tool names, so a test
/// can assert the loop fed the appended tool results back into the next turn
/// (and, for deferred advertising, that the tool set widened between rounds).
#[derive(Default)]
struct ScriptedStreamer {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen_message_counts: Mutex<Vec<usize>>,
    seen_tool_names: Mutex<Vec<Vec<String>>>,
}

impl ScriptedStreamer {
    fn scripted(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Mutex::new(VecDeque::from(turns)),
            ..Self::default()
        }
    }
}

#[async_trait]
impl TurnStreamer for ScriptedStreamer {
    async fn open(
        &self,
        request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        self.seen_message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        self.seen_tool_names
            .lock()
            .unwrap()
            .push(request.tools.iter().map(|t| t.name.clone()).collect());
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

struct MediaCaptureStreamer {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    seen_messages: Mutex<Vec<Vec<ChatMessage>>>,
}

#[async_trait]
impl TurnStreamer for MediaCaptureStreamer {
    async fn open(
        &self,
        request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        self.seen_messages.lock().unwrap().push(request.messages);
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

#[tokio::test]
async fn native_image_is_attached_after_the_tool_result_on_the_next_turn() {
    let streamer = MediaCaptureStreamer {
        turns: Mutex::new(VecDeque::from([
            tool_turn("media-1", "native_media", "{}"),
            vec![
                StreamEvent::TextDelta {
                    text: "seen".into(),
                },
                StreamEvent::Done {
                    finish_reason: Some(FinishReason::Stop),
                    usage: None,
                },
            ],
        ])),
        seen_messages: Mutex::new(Vec::new()),
    };
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(NativeMediaTool));
    let config = AgentConfig {
        input_modalities: vec!["image".into()],
        ..AgentConfig::default()
    };
    let mut observer = RecordingObserver::default();
    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("inspect image.png")]),
        &registry,
        &full_authority(),
        &config,
        None,
        &mut observer,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "seen");
    let requests = streamer.seen_messages.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let second = &requests[1];
    assert_eq!(
        second
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::User,
        ]
    );
    assert_eq!(second[2].content, r#"{"size":4}"#);
    assert_eq!(
        second[3].media,
        vec![MediaInput::Image {
            url: "data:image/png;base64,AA==".into(),
        }]
    );
    assert_eq!(
        observer
            .messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Assistant,
        ]
    );
}

/// One [`TurnObserver::on_message`] call, captured for assertion: the
/// incremental-persistence contract (role, content, the answered call id, and
/// the dispatch error flag).
#[derive(Debug, PartialEq, Eq)]
struct RecordedMessage {
    role: MessageRole,
    content: String,
    tool_call_id: Option<String>,
    tool_is_error: bool,
}

/// An observer that records every forwarded event (the live-relay contract)
/// and every completed message (the incremental-persistence contract), and
/// hands out scripted user input batches (the mid-turn injection contract) —
/// one batch per `poll_user_input` boundary poll, then empties.
#[derive(Default)]
struct RecordingObserver {
    events: Vec<StreamEvent>,
    messages: Vec<RecordedMessage>,
    queued_input: VecDeque<Vec<ChatMessage>>,
}

#[async_trait]
impl TurnObserver for RecordingObserver {
    async fn on_event(&mut self, event: &StreamEvent) -> Result<()> {
        self.events.push(event.clone());
        Ok(())
    }

    async fn on_message(&mut self, message: &CompletedMessage<'_>) -> Result<()> {
        self.messages.push(RecordedMessage {
            role: message.role,
            content: message.content.to_string(),
            tool_call_id: message.tool_call_id.map(str::to_string),
            tool_is_error: message.tool_is_error,
        });
        Ok(())
    }

    async fn poll_user_input(&mut self) -> Result<Vec<ChatMessage>> {
        Ok(self.queued_input.pop_front().unwrap_or_default())
    }
}

#[tokio::test]
async fn streaming_loop_dispatches_tools_and_relays_events() {
    // Round 1: the model asks to call `echo`; round 2: it answers with text.
    let round1 = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{\"a\":1}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let round2 = vec![
        StreamEvent::TextDelta {
            text: "done".into(),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1, round2])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let request = ChatRequest::new("m", vec![ChatMessage::user("hi")]);

    let outcome = run_loop(
        &streamer,
        request,
        &reg,
        &ctx,
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(outcome.iterations, 2);
    // A clean finish (the model stopped on its own) is not a cap hit.
    assert!(!outcome.hit_iteration_cap);
    assert_eq!(outcome.tool_invocations.len(), 1);
    assert!(!outcome.tool_invocations[0].is_error);
    // Transcript: user, assistant(tool_calls), tool result, assistant(answer).
    assert_eq!(outcome.messages.len(), 4);
    assert_eq!(outcome.messages[1].role, MessageRole::Assistant);
    assert_eq!(outcome.messages[1].tool_calls.len(), 1);
    assert_eq!(outcome.messages[2].role, MessageRole::Tool);
    assert_eq!(outcome.messages[2].tool_call_id.as_deref(), Some("c1"));
    assert_eq!(outcome.messages[3].content, "done");
    // Round 2's request carried the appended assistant + tool messages (1→3).
    assert_eq!(
        *streamer.seen_message_counts.lock().unwrap(),
        vec![1usize, 3usize]
    );
    // Every event from both rounds was forwarded, plus the two synthesized
    // tool-lifecycle events bracketing the dispatch (started + result):
    // [ToolCallDelta, Done, ToolCallStarted, ToolResult, TextDelta, Done].
    assert_eq!(obs.events.len(), 6);
    assert!(matches!(
        &obs.events[2],
        StreamEvent::ToolCallStarted { id, name, .. } if id == "c1" && name == "echo"
    ));
    assert!(matches!(
        &obs.events[3],
        StreamEvent::ToolResult { id, name, is_error, .. }
            if id == "c1" && name == "echo" && !is_error
    ));
    // `on_message` fired once per appended transcript row, in order: the
    // assistant tool-call turn (no answered-call id), its tool result (keyed to
    // the call, not an error), then the final assistant answer. This is the
    // incremental-persistence contract — a caller persists each as it lands.
    assert_eq!(
        obs.messages,
        vec![
            RecordedMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_call_id: None,
                tool_is_error: false,
            },
            RecordedMessage {
                role: MessageRole::Tool,
                content: "{\"a\":1}".into(),
                tool_call_id: Some("c1".into()),
                tool_is_error: false,
            },
            RecordedMessage {
                role: MessageRole::Assistant,
                content: "done".into(),
                tool_call_id: None,
                tool_is_error: false,
            },
        ]
    );
}

#[tokio::test]
async fn streaming_loop_recovers_once_from_an_empty_final_response() {
    let tool_round = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let empty_finish = vec![StreamEvent::Done {
        finish_reason: Some(FinishReason::Stop),
        usage: None,
    }];
    let recovered = vec![
        StreamEvent::TextDelta {
            text: "finished".into(),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([tool_round, empty_finish, recovered])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("build it")]),
        &reg,
        &full_authority(),
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "finished");
    assert_eq!(outcome.iterations, 3);
    assert_eq!(
        *streamer.seen_message_counts.lock().unwrap(),
        vec![1usize, 3usize, 5usize]
    );
    assert!(outcome.messages.iter().any(|message| {
        message.role == MessageRole::System
            && message
                .content
                .contains("previous assistant response was empty")
    }));
}

fn text_turn_ending(text: &str, finish_reason: FinishReason) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    if !text.is_empty() {
        events.push(StreamEvent::TextDelta { text: text.into() });
    }
    events.push(StreamEvent::Done {
        finish_reason: Some(finish_reason),
        usage: None,
    });
    events
}

async fn run_text_turns(turns: Vec<Vec<StreamEvent>>) -> (AgentOutcome, Vec<usize>) {
    let streamer = ScriptedStreamer::scripted(turns);
    let mut obs = RecordingObserver::default();
    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("write it")]),
        &ToolRegistry::new(),
        &full_authority(),
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();
    let counts = streamer.seen_message_counts.lock().unwrap().clone();
    (outcome, counts)
}

#[tokio::test]
async fn streaming_loop_continues_an_answer_cut_off_by_the_output_limit() {
    let (outcome, counts) = run_text_turns(vec![
        text_turn_ending("first half, ", FinishReason::Length),
        text_turn_ending("second half.", FinishReason::Stop),
    ])
    .await;

    // The pieces join into one reply and the run is a clean finish.
    assert_eq!(outcome.content, "first half, second half.");
    assert!(!outcome.hit_output_limit);
    assert_eq!(outcome.iterations, 2);
    // Turn 2 saw the partial answer plus the continue nudge, sent as a user
    // message so the history never ends on assistant text.
    assert_eq!(counts, vec![1usize, 3usize]);
    let nudge = &outcome.messages[2];
    assert_eq!(nudge.role, MessageRole::User);
    assert!(nudge.content.contains("cut off by the output-token limit"));
}

#[tokio::test]
async fn streaming_loop_flags_an_answer_still_cut_off_after_its_continuations() {
    let turns = (0..=MAX_OUTPUT_CONTINUATIONS)
        .map(|_| text_turn_ending("more ", FinishReason::Length))
        .collect();
    let (outcome, _) = run_text_turns(turns).await;

    assert!(outcome.hit_output_limit);
    assert_eq!(outcome.iterations, MAX_OUTPUT_CONTINUATIONS + 1);
    assert_eq!(
        outcome.content,
        "more ".repeat(MAX_OUTPUT_CONTINUATIONS + 1)
    );
}

#[test]
fn incomplete_reason_names_every_way_of_ending_short() {
    let finished = |finish_reason| AgentOutcome {
        finish_reason,
        ..Default::default()
    };
    assert_eq!(finished(Some(FinishReason::Stop)).incomplete_reason(), None);
    assert_eq!(
        finished(Some(FinishReason::ContentFilter)).incomplete_reason(),
        Some("content_filter")
    );
    assert_eq!(finished(Some(FinishReason::Other)).incomplete_reason(), Some("other"));
    assert_eq!(finished(None).incomplete_reason(), Some("no_finish"));
    let cost = AgentOutcome {
        hit_cost_limit: true,
        finish_reason: Some(FinishReason::ToolCalls),
        ..Default::default()
    };
    assert_eq!(cost.incomplete_reason(), Some("cost_limit"));
    let errored = AgentOutcome {
        stream_error: Some("upstream 529".into()),
        ..Default::default()
    };
    assert_eq!(errored.incomplete_reason(), Some("stream_error"));
    // A deliberate Stop is not an incomplete run.
    let stopped = AgentOutcome {
        stopped: true,
        ..Default::default()
    };
    assert_eq!(stopped.incomplete_reason(), None);
}

#[tokio::test]
async fn streaming_loop_flags_a_turn_whose_whole_allowance_went_to_reasoning() {
    // Empty and cut off twice: the one empty-response retry runs, then the run
    // ends flagged instead of passing for a clean, blank answer.
    let (outcome, _) = run_text_turns(vec![
        text_turn_ending("", FinishReason::Length),
        text_turn_ending("", FinishReason::Length),
    ])
    .await;

    assert!(outcome.hit_output_limit);
    assert_eq!(outcome.content, "");
    assert_eq!(outcome.iterations, 2);
}

#[tokio::test]
async fn streaming_loop_carries_reasoning_into_assistant_turn() {
    // Round 1 emits reasoning + a tool call; the assistant message the loop
    // appends (and re-sends in round 2) must carry that reasoning so a
    // reasoning model's signed chain survives the tool round-trip.
    let round1 = vec![
        StreamEvent::ReasoningDelta {
            text: "weighing options".into(),
            details: vec![catalerum_core::stream::ReasoningDetail {
                kind: "reasoning.encrypted".into(),
                data: Some("blob".into()),
                signature: Some("sig".into()),
                index: Some(0),
                ..Default::default()
            }],
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let round2 = vec![
        StreamEvent::TextDelta { text: "ok".into() },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1, round2])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    // The assistant tool-call turn (messages[1]) carries the reasoning.
    let assistant = &outcome.messages[1];
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(assistant.reasoning.as_deref(), Some("weighing options"));
    assert_eq!(assistant.reasoning_details.len(), 1);
    assert_eq!(
        assistant.reasoning_details[0].signature.as_deref(),
        Some("sig")
    );
    // The reasoning event was relayed live, too.
    assert!(obs
        .events
        .iter()
        .any(|e| matches!(e, StreamEvent::ReasoningDelta { .. })));
}

#[tokio::test]
async fn streaming_loop_stops_on_stream_error() {
    let round1 = vec![
        StreamEvent::TextDelta {
            text: "partial".into(),
        },
        StreamEvent::Error {
            message: "boom".into(),
        },
        StreamEvent::Done {
            finish_reason: None,
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let request = ChatRequest::new("m", vec![ChatMessage::user("hi")]);

    let outcome = run_loop(
        &streamer,
        request,
        &reg,
        &ctx,
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    // A stream error is terminal: one round, best-effort partial text, and the
    // error event still reached the client.
    assert_eq!(outcome.iterations, 1);
    assert_eq!(outcome.content, "partial");
    assert!(obs
        .events
        .iter()
        .any(|e| matches!(e, StreamEvent::Error { .. })));
}

#[tokio::test]
async fn iteration_cap_surfaces_last_assistant_text_not_empty() {
    // A model that keeps requesting tools: with max_iterations = 1 the loop
    // dispatches the tool then hits the cap. `content` must fall back to the
    // last assistant turn's text rather than being left empty.
    let round1 = vec![
        StreamEvent::TextDelta {
            text: "thinking".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        max_iterations: 1,
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.iterations, 1);
    assert_eq!(outcome.content, "thinking");
    // The run was truncated by the cap, not a clean finish — callers must be
    // able to tell the answer is incomplete.
    assert!(outcome.hit_iteration_cap);
    // The transcript ends on the dangling tool result (cap hit mid-loop).
    assert_eq!(outcome.messages.last().unwrap().role, MessageRole::Tool);
    assert_eq!(outcome.tool_invocations.len(), 1);
}

#[test]
fn tool_call_signature_ignores_json_formatting_and_object_key_order() {
    let first = ToolCall {
        id: "first".into(),
        name: "echo".into(),
        arguments: r#"{"b":[{"y":2,"x":1}],"a":true}"#.into(),
    };
    let second = ToolCall {
        id: "second".into(),
        name: "echo".into(),
        arguments: r#"{ "a": true, "b": [ { "x": 1, "y": 2 } ] }"#.into(),
    };
    assert_eq!(tool_call_signature(&first), tool_call_signature(&second));
}

#[tokio::test]
async fn identical_tool_call_streak_stops_before_the_iteration_cap() {
    // The model changes call ids and JSON formatting, but the semantic call
    // is identical. Three completed calls are persisted, then the loop stops
    // without opening the fourth scripted turn.
    let streamer = ScriptedStreamer::scripted(vec![
        tool_turn("c1", "echo", r#"{"a":1,"b":2}"#),
        tool_turn("c2", "echo", r#"{ "b": 2, "a": 1 }"#),
        tool_turn("c3", "echo", r#"{"b":2,"a":1}"#),
        text_turn("should not run"),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &full_authority(),
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.iterations, 3);
    assert_eq!(outcome.tool_invocations.len(), 3);
    assert!(outcome.hit_tool_loop_cap);
    assert!(!outcome.hit_iteration_cap);
    assert_eq!(streamer.seen_message_counts.lock().unwrap().len(), 3);
    assert_eq!(outcome.messages.last().unwrap().role, MessageRole::Tool);
}

#[tokio::test]
async fn consecutive_tool_errors_stop_even_when_calls_keep_changing() {
    let streamer = ScriptedStreamer::scripted(vec![
        tool_turn("c1", "missing", r#"{"attempt":1}"#),
        tool_turn("c2", "missing", r#"{"attempt":2}"#),
        tool_turn("c3", "missing", r#"{"attempt":3}"#),
        text_turn("should not run"),
    ]);
    let cfg = AgentConfig {
        max_identical_tool_calls: 0,
        max_consecutive_tool_errors: 3,
        ..AgentConfig::default()
    };
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &ToolRegistry::new(),
        &full_authority(),
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.iterations, 3);
    assert_eq!(outcome.tool_invocations.len(), 3);
    assert!(outcome.tool_invocations.iter().all(|call| call.is_error));
    assert!(outcome.hit_tool_loop_cap);
    assert!(!outcome.hit_iteration_cap);
    assert_eq!(outcome.messages.last().unwrap().role, MessageRole::Tool);
}

#[tokio::test]
async fn a_success_resets_the_consecutive_tool_error_streak() {
    let streamer = ScriptedStreamer::scripted(vec![
        tool_turn("c1", "missing", r#"{"attempt":1}"#),
        tool_turn("c2", "missing", r#"{"attempt":2}"#),
        tool_turn("c3", "echo", r#"{"ok":true}"#),
        tool_turn("c4", "missing", r#"{"attempt":3}"#),
        tool_turn("c5", "missing", r#"{"attempt":4}"#),
        text_turn("recovered"),
    ]);
    let cfg = AgentConfig {
        max_identical_tool_calls: 0,
        max_consecutive_tool_errors: 3,
        ..AgentConfig::default()
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &full_authority(),
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "recovered");
    assert_eq!(outcome.iterations, 6);
    assert!(!outcome.hit_tool_loop_cap);
    assert!(!outcome.hit_iteration_cap);
}

#[tokio::test]
async fn cost_limit_halts_before_another_paid_turn() {
    // A turn that wants another tool round and reports a cost over the cap: the
    // loop must stop BEFORE dispatching the tools / opening the next (paid) turn.
    let round1 = vec![
        StreamEvent::TextDelta {
            text: "spending".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 10,
                total_tokens: 20,
                cost_usd: Some(0.60),
                cached_tokens: 0,
                cache_creation_tokens: 0,
            }),
        },
    ];
    // Only one turn is scripted: if the cap is ignored the loop would open a
    // second (empty) turn and the assertions below would fail.
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        max_iterations: 5,
        cost_limit: Some(0.50),
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    // Stopped after one turn, flagged as cost-capped (not an iteration cap).
    assert_eq!(outcome.iterations, 1);
    assert!(outcome.hit_cost_limit);
    assert!(!outcome.hit_iteration_cap);
    // We halted BEFORE dispatching the pending tool — no spend beyond the turn
    // already paid for: no tool invocations, transcript ends on the assistant turn.
    assert!(outcome.tool_invocations.is_empty());
    assert_eq!(
        outcome.messages.last().unwrap().role,
        MessageRole::Assistant
    );
    assert_eq!(outcome.content, "spending");
    assert_eq!(outcome.usage.and_then(|u| u.cost_usd), Some(0.60));
}

#[tokio::test]
async fn cost_limit_does_not_trip_when_under_budget() {
    // Same shape but the turn's cost stays under the cap → the loop proceeds
    // normally: dispatches the tool, the next turn finishes clean (no cap flags).
    let round1 = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cost_usd: Some(0.10),
                cached_tokens: 0,
                cache_creation_tokens: 0,
            }),
        },
    ];
    let round2 = vec![
        StreamEvent::TextDelta {
            text: "done".into(),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::Stop),
            usage: Some(Usage {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cost_usd: Some(0.10),
                cached_tokens: 0,
                cache_creation_tokens: 0,
            }),
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1, round2])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        max_iterations: 5,
        cost_limit: Some(0.50),
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    // Two turns ran (cumulative 0.20 < 0.50), the tool was dispatched, clean finish.
    assert_eq!(outcome.iterations, 2);
    assert!(!outcome.hit_cost_limit);
    assert_eq!(outcome.tool_invocations.len(), 1);
    assert_eq!(outcome.content, "done");
    assert_eq!(outcome.usage.and_then(|u| u.cost_usd), Some(0.20));
}

// ---- mid-turn user input (SOUL §12) ----------------------------------

fn text_turn(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta { text: text.into() },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::Stop),
            usage: None,
        },
    ]
}

#[tokio::test]
async fn queued_user_input_extends_the_turn_past_a_clean_finish() {
    // The model finishes cleanly after round 1, but the user typed a follow-up
    // while it streamed: the loop folds it in and runs another round instead of
    // ending the turn. Only the (empty) second poll ends it.
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([text_turn("first"), text_turn("second")])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver {
        queued_input: VecDeque::from([vec![ChatMessage::user("follow-up")]]),
        ..Default::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.iterations, 2);
    assert_eq!(outcome.content, "second");
    assert!(!outcome.stopped);
    // Transcript: user, assistant("first"), injected user, assistant("second").
    let roles: Vec<_> = outcome.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::User,
            MessageRole::Assistant
        ]
    );
    assert_eq!(outcome.messages[2].content, "follow-up");
    // Round 2's request carried the injected message (1 → 3 messages).
    assert_eq!(
        *streamer.seen_message_counts.lock().unwrap(),
        vec![1usize, 3usize]
    );
}

#[tokio::test]
async fn queued_user_input_lands_after_the_round_s_tool_results() {
    // Round 1 dispatches a tool; the user's mid-round message must land right
    // AFTER that round's tool results (the "next available slot"), before the
    // model's next turn.
    let round1 = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1, text_turn("done")])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver {
        queued_input: VecDeque::from([vec![ChatMessage::user("note")]]),
        ..Default::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &AgentConfig::default(),
        None,
        &mut obs,
    )
    .await
    .unwrap();

    // Transcript: user, assistant(tool_calls), tool result, injected user,
    // assistant("done").
    let roles: Vec<_> = outcome.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::User,
            MessageRole::Assistant
        ]
    );
    assert_eq!(outcome.messages[3].content, "note");
    assert_eq!(outcome.content, "done");
}

// ---- stop (user cancel, SOUL §12) ------------------------------------

/// A streamer whose turn streams a text delta + a half tool call, then stalls
/// forever — the shape of a live turn a user stops mid-stream.
struct StallingStreamer;

#[async_trait]
impl TurnStreamer for StallingStreamer {
    async fn open(
        &self,
        _request: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamEvent>>> {
        let events = vec![
            StreamEvent::TextDelta {
                text: "partial".into(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("c1".into()),
                name: Some("echo".into()),
                arguments: Some("{}".into()),
            },
        ];
        Ok(futures::stream::iter(events.into_iter().map(Ok))
            .chain(futures::stream::pending())
            .boxed())
    }
}

#[tokio::test]
async fn stop_mid_stream_keeps_partial_text_and_drops_half_assembled_calls() {
    let cancel = CancellationToken::new();
    let cfg = AgentConfig {
        cancel: cancel.clone(),
        ..AgentConfig::default()
    };
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &StallingStreamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert!(outcome.stopped);
    assert_eq!(outcome.content, "partial");
    // The partial text is persisted, but the half-assembled tool call is
    // dropped — the transcript must never dangle an undispatched call.
    let assistant = outcome.messages.last().unwrap();
    assert_eq!(assistant.role, MessageRole::Assistant);
    assert_eq!(assistant.content, "partial");
    assert!(assistant.tool_calls.is_empty());
    assert!(outcome.tool_invocations.is_empty());
    assert_eq!(
        obs.messages,
        vec![RecordedMessage {
            role: MessageRole::Assistant,
            content: "partial".into(),
            tool_call_id: None,
            tool_is_error: false,
        }]
    );
}

/// A tool that never finishes on its own — for racing a dispatch against the
/// user's stop.
struct StuckTool;

#[async_trait]
impl Tool for StuckTool {
    fn name(&self) -> &str {
        "stuck"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        Ok(Json::Null)
    }
}

#[tokio::test]
async fn stop_mid_dispatch_synthesizes_cancelled_results_for_every_call() {
    // Round 1 requests two calls: `stuck` (which hangs) then `echo`. Stopping
    // mid-`stuck` must abandon it AND answer both calls with a synthesized
    // cancelled error — persisted like real results, so no call dangles.
    let round1 = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("stuck".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::ToolCallDelta {
            index: 1,
            id: Some("c2".into()),
            name: Some("echo".into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::from([round1])),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(StuckTool));
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let cancel = CancellationToken::new();
    let cfg = AgentConfig {
        cancel: cancel.clone(),
        ..AgentConfig::default()
    };
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    });
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert!(outcome.stopped);
    // Both calls answered with a cancelled error — the abandoned in-flight one
    // and the never-started one alike.
    assert_eq!(outcome.tool_invocations.len(), 2);
    for inv in &outcome.tool_invocations {
        assert!(inv.is_error);
        assert!(inv.result.contains("cancelled"), "{}", inv.result);
    }
    // Transcript stays well-formed: assistant(2 calls) then a tool result per
    // call, each persisted via on_message with the error flag set.
    let roles: Vec<_> = outcome.messages.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool
        ]
    );
    let tool_rows: Vec<_> = obs
        .messages
        .iter()
        .filter(|m| m.role == MessageRole::Tool)
        .collect();
    assert_eq!(tool_rows.len(), 2);
    assert!(tool_rows.iter().all(|m| m.tool_is_error));
    // The client saw both cards resolve (a ToolResult per call).
    let results: Vec<_> = obs
        .events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolResult { id, is_error, .. } => Some((id.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![("c1".to_string(), true), ("c2".to_string(), true)]
    );
}

#[tokio::test]
async fn pre_cancelled_run_stops_before_any_model_turn() {
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cfg = AgentConfig {
        cancel,
        ..AgentConfig::default()
    };
    let streamer = ScriptedStreamer {
        turns: Mutex::new(VecDeque::new()),
        seen_message_counts: Mutex::new(Vec::new()),
        seen_tool_names: Mutex::new(Vec::new()),
    };
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert!(outcome.stopped);
    // No (paid) model turn was opened.
    assert!(streamer.seen_message_counts.lock().unwrap().is_empty());
    assert!(obs.messages.is_empty());
}

// ---- deferred tool advertising (SOUL §7) -------------------------------

/// A stand-in discovery tool: its result names `echo` (plus an unknown tool)
/// in the `"tools": [{"name": …}]` shape the real `search_tools`/`list_tools`
/// return.
struct FinderTool;

#[async_trait]
impl Tool for FinderTool {
    fn name(&self) -> &str {
        "finder"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        Ok(serde_json::json!({
            "tools": [{ "name": "echo" }, { "name": "not_registered" }]
        }))
    }
}

/// A discovery tool that loads only `explicit_loader`; that ordinary tool's
/// result then explicitly advertises `echo` for the following round.
struct ExplicitLoaderFinder;

#[async_trait]
impl Tool for ExplicitLoaderFinder {
    fn name(&self) -> &str {
        "explicit_loader_finder"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        Ok(serde_json::json!({ "tools": [{ "name": "explicit_loader" }] }))
    }
}

struct ExplicitLoaderTool;

#[async_trait]
impl Tool for ExplicitLoaderTool {
    fn name(&self) -> &str {
        "explicit_loader"
    }
    /// Test tool: open.
    fn required_capability(&self) -> Option<catalerum_core::capability::Capability> {
        None
    }
    fn parameters_schema(&self) -> Json {
        serde_json::json!({ "type": "object" })
    }
    async fn invoke(&self, _args: Json, _ctx: &ToolContext) -> Result<Json> {
        Ok(serde_json::json!({
            "advertise_tools": ["echo", "not_registered", "echo"]
        }))
    }
}

fn tool_call_turn(id: &str, name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some(id.into()),
            name: Some(name.into()),
            arguments: Some("{}".into()),
        },
        StreamEvent::Done {
            finish_reason: Some(FinishReason::ToolCalls),
            usage: None,
        },
    ]
}

#[tokio::test]
async fn deferred_advertising_seeds_pinned_set_and_widens_from_discovery() {
    // Rounds: the model calls `finder`, then the freshly advertised `echo`,
    // then answers.
    let streamer = ScriptedStreamer::scripted(vec![
        tool_call_turn("c1", "finder"),
        tool_call_turn("c2", "echo"),
        text_turn("done"),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(FinderTool));
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        discovery_tools: vec!["finder".to_string()],
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(outcome.tool_invocations.len(), 2);
    // Round 1 advertised only the pinned subset; the finder's result named
    // `echo`, so rounds 2+ carry its full spec too. The unknown name in the
    // result is skipped, never fabricated.
    assert_eq!(
        *streamer.seen_tool_names.lock().unwrap(),
        vec![
            vec!["finder".to_string()],
            vec!["finder".to_string(), "echo".to_string()],
            vec!["finder".to_string(), "echo".to_string()],
        ]
    );
    // The discovery note leads the seed (no caller system prefix here) and
    // maps the catalog: both tools are listed, with the ungated ones under
    // "general".
    let note = &outcome.messages[0];
    assert_eq!(note.role, MessageRole::System);
    assert!(note.content.contains("Tool discovery"), "{}", note.content);
    assert!(
        note.content.contains("general: echo, finder"),
        "{}",
        note.content
    );
}

#[tokio::test]
async fn deferred_advertising_accepts_explicit_companions_from_any_loaded_tool() {
    let streamer = ScriptedStreamer::scripted(vec![
        tool_call_turn("c1", "explicit_loader_finder"),
        tool_call_turn("c2", "explicit_loader"),
        tool_call_turn("c3", "echo"),
        text_turn("done"),
    ]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(ExplicitLoaderFinder));
    reg.register(Arc::new(ExplicitLoaderTool));
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        discovery_tools: vec!["explicit_loader_finder".to_string()],
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(
        *streamer.seen_tool_names.lock().unwrap(),
        vec![
            vec!["explicit_loader_finder".to_string()],
            vec![
                "explicit_loader_finder".to_string(),
                "explicit_loader".to_string()
            ],
            vec![
                "explicit_loader_finder".to_string(),
                "explicit_loader".to_string(),
                "echo".to_string()
            ],
            vec![
                "explicit_loader_finder".to_string(),
                "explicit_loader".to_string(),
                "echo".to_string()
            ],
        ]
    );
}

#[tokio::test]
async fn deferred_advertising_rewidens_from_replayed_history() {
    // The seed history already contains an earlier turn's `finder` call +
    // result naming `echo`: the new run must advertise `echo` from round 1
    // so the model calls it directly instead of re-running discovery.
    let streamer =
        ScriptedStreamer::scripted(vec![tool_call_turn("c2", "echo"), text_turn("done")]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(FinderTool));
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        discovery_tools: vec!["finder".to_string()],
        ..AgentConfig::default()
    };
    let call = ToolCall {
        id: "c1".to_string(),
        name: "finder".to_string(),
        arguments: "{}".to_string(),
    };
    let mut prior_turn = ChatMessage::assistant("");
    prior_turn.tool_calls = vec![call.clone()];
    let history = vec![
        ChatMessage::user("find me a tool"),
        prior_turn,
        tool_message(
            &call,
            r#"{"tools":[{"name":"echo"},{"name":"not_registered"}]}"#,
        ),
        ChatMessage::user("now use it"),
    ];

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", history),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    // Every round of the new run — including the first — carries the
    // previously discovered `echo` alongside the pinned subset; the unknown
    // name is still skipped.
    assert_eq!(
        *streamer.seen_tool_names.lock().unwrap(),
        vec![
            vec!["finder".to_string(), "echo".to_string()],
            vec!["finder".to_string(), "echo".to_string()],
        ]
    );
}

#[tokio::test]
async fn deferred_advertising_respects_the_allow_list_when_widening() {
    // `echo` is outside the allow-list: the finder's result must NOT
    // advertise it, and the catalog note must not map it either.
    let streamer =
        ScriptedStreamer::scripted(vec![tool_call_turn("c1", "finder"), text_turn("done")]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(FinderTool));
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        discovery_tools: vec!["finder".to_string()],
        ..AgentConfig::default()
    };
    let allowed = vec!["finder".to_string()];

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        Some(&allowed),
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(
        *streamer.seen_tool_names.lock().unwrap(),
        vec![vec!["finder".to_string()], vec!["finder".to_string()]]
    );
    assert!(!outcome.messages[0].content.contains("echo"));
}

#[tokio::test]
async fn deferred_advertising_falls_back_to_everything_on_an_empty_seed() {
    // None of the configured discovery tools exist in this registry: the loop
    // must degrade to the verbose-but-correct path (advertise everything, no
    // discovery note).
    let streamer = ScriptedStreamer::scripted(vec![text_turn("done")]);
    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(EchoTool));
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        discovery_tools: vec!["nope".to_string()],
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", vec![ChatMessage::user("hi")]),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(
        *streamer.seen_tool_names.lock().unwrap(),
        vec![vec!["echo".to_string()]]
    );
    // No note was injected — the seed still opens with the user message.
    assert_eq!(outcome.messages[0].role, MessageRole::User);
}

// ---- auto-compaction (SOUL §7) ---------------------------------------

use crate::compact::COMPACTION_SUMMARY_PREFIX;

/// A seed history big enough to trip a tiny compaction budget: a system
/// prefix, two large replayed turns (the foldable head), and the fresh user
/// message (the tail that must survive verbatim).
fn oversized_seed() -> Vec<ChatMessage> {
    vec![
        ChatMessage::system("sys"),
        ChatMessage::user("old question ".repeat(50)),
        ChatMessage::assistant("old answer ".repeat(50)),
        ChatMessage::user("answer me now"),
    ]
}

/// A compaction config with a window small enough that [`oversized_seed`]
/// (~300 estimated tokens) trips it, keeping only the last message verbatim.
fn tiny_compaction() -> CompactionConfig {
    CompactionConfig {
        context_window: Some(200),
        keep_recent: 1,
        ..CompactionConfig::default()
    }
}

#[tokio::test]
async fn oversized_history_is_compacted_before_the_next_turn() {
    // Turn 1 is consumed by the summarize call; turn 2 answers on the
    // compacted history.
    let streamer =
        ScriptedStreamer::scripted(vec![text_turn("THE-SUMMARY"), text_turn("done")]);
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        compaction: tiny_compaction(),
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", oversized_seed()),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(outcome.compactions, 1);
    // The summarize request carried its own 2-message prompt; the real turn
    // then ran on the compacted history: system prefix + summary + tail
    // (down from the 4-message seed).
    assert_eq!(
        *streamer.seen_message_counts.lock().unwrap(),
        vec![2usize, 3usize]
    );
    assert_eq!(outcome.messages[0].role, MessageRole::System);
    assert!(outcome.messages[1]
        .content
        .starts_with(COMPACTION_SUMMARY_PREFIX));
    assert!(outcome.messages[1].content.contains("THE-SUMMARY"));
    assert_eq!(outcome.messages[2].content, "answer me now");
    // The synthesized Compacted event rode the observer relay (folded = the
    // 2 head messages), but the summarize turn's own deltas did NOT — the
    // summary is loop machinery, never streamed answer text.
    assert!(obs.events.iter().any(|e| matches!(
        e,
        StreamEvent::Compacted { folded: 2, summary } if summary == "THE-SUMMARY"
    )));
    assert!(!obs
        .events
        .iter()
        .any(|e| matches!(e, StreamEvent::TextDelta { text } if text == "THE-SUMMARY")));
}

#[tokio::test]
async fn failed_summarize_fails_open_and_the_turn_proceeds() {
    // The summarize turn errors → compaction is skipped, the real turn runs
    // on the full (oversized) history, and the run still finishes cleanly.
    let summarize_error = vec![
        StreamEvent::Error {
            message: "boom".into(),
        },
        StreamEvent::Done {
            finish_reason: None,
            usage: None,
        },
    ];
    let streamer = ScriptedStreamer::scripted(vec![summarize_error, text_turn("done")]);
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        compaction: tiny_compaction(),
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", oversized_seed()),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.content, "done");
    assert_eq!(outcome.compactions, 0);
    // Summarize prompt (2) then the real turn on the UNcompacted seed (4).
    assert_eq!(
        *streamer.seen_message_counts.lock().unwrap(),
        vec![2usize, 4usize]
    );
    assert!(!obs
        .events
        .iter()
        .any(|e| matches!(e, StreamEvent::Compacted { .. })));
}

#[tokio::test]
async fn compaction_disabled_never_summarizes() {
    let streamer = ScriptedStreamer::scripted(vec![text_turn("done")]);
    let reg = ToolRegistry::new();
    let ctx = full_authority();
    let mut obs = RecordingObserver::default();
    let cfg = AgentConfig {
        compaction: CompactionConfig {
            enabled: false,
            ..tiny_compaction()
        },
        ..AgentConfig::default()
    };

    let outcome = run_loop(
        &streamer,
        ChatRequest::new("m", oversized_seed()),
        &reg,
        &ctx,
        &cfg,
        None,
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome.compactions, 0);
    // One turn, straight on the full seed.
    assert_eq!(*streamer.seen_message_counts.lock().unwrap(), vec![4usize]);
}
