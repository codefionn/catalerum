//! The llmleaf client adapter (SOUL §7), speaking the Responses dialect.
//!
//! [`OpenRouterClient`] preserves catalerum's provider-agnostic trait surface,
//! but delegates HTTP transport, request encoding, response decoding, and SSE
//! parsing to llmleaf's official Rust SDK (`llmleaf-client`). Interactive turns
//! POST the OpenAI Responses dialect (`/v1/responses`, typed SSE events); this
//! module maps between catalerum-core requests/events and those wire types.
//! Only the batch API still speaks chat-completions shapes — that is the SDK's
//! batch contract (`BatchRequestItem.body` is a `ChatRequest`), not a choice
//! made here.

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};

use catalerum_core::error::{Error, Result};
use catalerum_core::llm::{ChatMessage, ChatRequest, MediaInput, ToolChoice};
use catalerum_core::model::{MessageRole, ToolCall};
use catalerum_core::provider::LlmClient;
use catalerum_core::stream::{FinishReason, ReasoningDetail, StreamEvent, Usage};

use crate::trace::{trace_stream, GenerationSpans};
use crate::wire::ProviderRouting;
use crate::LlmTraceConfig;

/// A streaming chat client for llmleaf (OpenRouter wire format), SOUL §7.
///
/// Construct with [`OpenRouterClient::new`] (base URL + API key) or
/// [`OpenRouterClient::builder`] for finer control. Cheap to clone (the inner
/// SDK client wraps an `Arc`-backed [`reqwest::Client`]).
#[derive(Clone, Debug)]
pub struct OpenRouterClient {
    inner: std::result::Result<llmleaf_client::Client, String>,
    base_url: String,
    /// Optional default provider routing applied to every request.
    default_provider: ProviderRouting,
    /// Optional default `models` fallback list applied to every request.
    default_models: Vec<String>,
    /// Per-exporter generation payload capture policy.
    trace: LlmTraceConfig,
}

impl OpenRouterClient {
    /// A client for the llmleaf at `base_url` — the **origin only** (e.g.
    /// `http://llmleaf:8080`), with the given bearer key. The versioned path
    /// (`/v1/responses`, `/v1/embeddings`, …) is appended by the client, so
    /// a `base_url` ending in `/v1` double-prefixes and 404s. Trailing slashes are
    /// trimmed.
    #[must_use]
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::builder().base_url(base_url).api_key(api_key).build()
    }

    /// Start building a client with custom HTTP client / routing defaults.
    #[must_use]
    pub fn builder() -> OpenRouterClientBuilder {
        OpenRouterClientBuilder::default()
    }

    /// The base URL requests are posted under.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Cheap reachability probe for the gateway origin: an HTTP `GET` to
    /// [`base_url`](Self::base_url) with a short timeout. **Any** HTTP response
    /// (even a 404/401) means the gateway is reachable, so `Ok(())`; only a
    /// transport failure (connection refused, timeout, DNS) is an `Err`. This is
    /// a liveness check for the `/status` surface — it deliberately does not send
    /// a chat request (no tokens spent, no model required). Returns the error as a
    /// string so callers needn't depend on `reqwest`.
    pub async fn ping(&self) -> std::result::Result<(), String> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|e| e.to_string())?;
        client
            .get(&self.base_url)
            .send()
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The official llmleaf SDK client underneath this adapter.
    pub(crate) fn sdk(&self) -> Result<&llmleaf_client::Client> {
        self.inner.as_ref().map_err(Error::provider)
    }

    /// Open the streaming connection (`POST /v1/responses`) and return a
    /// catalerum [`StreamEvent`] stream. This is the implementation behind
    /// [`LlmClient::chat_stream`].
    ///
    /// The returned stream always terminates with exactly one
    /// [`StreamEvent::Done`]; any mid-stream transport/parse failure is emitted
    /// as a [`StreamEvent::Error`] immediately before that `Done`.
    pub async fn stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        let spans = GenerationSpans::new(self.trace, &request, &self.base_url);
        let request = self.to_responses_request(&request);
        let sdk = match self.sdk() {
            Ok(sdk) => sdk.clone(),
            Err(error) => {
                spans.record_error(&error.to_string());
                return Err(error);
            }
        };
        match sdk.responses_stream(request).await {
            Ok(events) => Ok(trace_stream(into_event_stream(events), spans)),
            Err(llmleaf_client::Error::Api { message, .. }) => {
                Ok(trace_stream(error_then_done(message), spans))
            }
            Err(e) => {
                spans.record_error(&sdk_error_message(&e));
                Err(map_sdk_error(e))
            }
        }
    }

    /// Non-streaming convenience: run a turn and collect it into a
    /// [`CollectedTurn`] (full text, assembled tool calls, finish reason, usage).
    ///
    /// Internally drives [`Self::stream`] to completion. Returns an `Err` only if
    /// the stream surfaced a terminal [`StreamEvent::Error`].
    pub async fn chat(&self, request: ChatRequest) -> Result<CollectedTurn> {
        let stream = self.stream(request).await?;
        collect_turn(stream).await
    }

    /// Encode a core request in the Responses dialect (the interactive path).
    pub(crate) fn to_responses_request(
        &self,
        request: &ChatRequest,
    ) -> llmleaf_client::ResponsesRequest {
        let mut out = llmleaf_client::ResponsesRequest::new(
            request.model.clone(),
            to_response_items(&request.messages),
        );
        out.temperature = request.temperature;
        out.max_output_tokens = request.max_tokens;
        out.reasoning =
            request
                .reasoning_effort
                .clone()
                .map(|effort| llmleaf_client::ResponsesReasoning {
                    effort: Some(effort),
                    summary: None,
                });
        out.tools = request
            .tools
            .iter()
            .map(|tool| {
                let mut def = llmleaf_client::ResponsesToolDef::function(tool.name.clone());
                def.description = (!tool.description.is_empty()).then(|| tool.description.clone());
                def.parameters = Some(tool.parameters.clone());
                def
            })
            .collect();
        out.tool_choice = request.tool_choice.as_ref().map(to_responses_tool_choice);
        out.extra = (!request.extra.is_empty()).then(|| request.extra.clone());
        self.apply_defaults(&mut out.extra);
        out
    }

    /// Encode a core request in the chat-completions dialect. Only the batch
    /// API uses this: the SDK's `BatchRequestItem.body` is a `ChatRequest`.
    pub(crate) fn to_chat_request(&self, request: &ChatRequest) -> llmleaf_client::ChatRequest {
        let mut out = llmleaf_client::ChatRequest::new(
            request.model.clone(),
            request.messages.iter().map(to_sdk_message).collect(),
        );
        out.temperature = request.temperature;
        out.max_tokens = request.max_tokens;
        out.reasoning_effort = request.reasoning_effort.clone();
        out.tools = request
            .tools
            .iter()
            .map(|tool| {
                llmleaf_client::ToolDef::function(llmleaf_client::FunctionDef {
                    name: tool.name.clone(),
                    description: (!tool.description.is_empty()).then(|| tool.description.clone()),
                    parameters: Some(tool.parameters.clone()),
                })
            })
            .collect();
        out.tool_choice = request.tool_choice.as_ref().map(to_sdk_tool_choice);
        out.extra = (!request.extra.is_empty()).then(|| request.extra.clone());
        self.apply_defaults(&mut out.extra);
        out
    }

    /// Inject the builder's provider-routing / models-fallback defaults into a
    /// request's passthrough map (both dialects merge `extra` at the top level),
    /// without overriding a per-request value.
    fn apply_defaults(&self, extra: &mut Option<serde_json::Map<String, serde_json::Value>>) {
        if !self.default_provider.is_empty() {
            let extra = extra.get_or_insert_with(Default::default);
            if !extra.contains_key("provider") {
                if let Ok(provider) = serde_json::to_value(&self.default_provider) {
                    extra.insert("provider".to_string(), provider);
                }
            }
        }

        if !self.default_models.is_empty() {
            let extra = extra.get_or_insert_with(Default::default);
            if !extra.contains_key("models") {
                extra.insert("models".to_string(), serde_json::json!(self.default_models));
            }
        }
    }
}

#[async_trait]
impl LlmClient for OpenRouterClient {
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        self.stream(request).await
    }
}

/// Builder for [`OpenRouterClient`].
#[derive(Debug, Default)]
pub struct OpenRouterClientBuilder {
    http: Option<reqwest::Client>,
    base_url: Option<String>,
    api_key: Option<String>,
    default_provider: ProviderRouting,
    default_models: Vec<String>,
    trace: LlmTraceConfig,
}

impl OpenRouterClientBuilder {
    /// Set the base URL — the origin only (e.g. `http://llmleaf:8080`); the client
    /// appends the `/v1/…` path, so do not include `/v1` here.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set the bearer API key.
    #[must_use]
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Supply a pre-configured [`reqwest::Client`] (timeouts, proxy, …).
    #[must_use]
    pub fn http_client(mut self, http: reqwest::Client) -> Self {
        self.http = Some(http);
        self
    }

    /// Default OpenRouter `provider` routing for every request (overridable
    /// per-request via `ChatRequest::extra`).
    #[must_use]
    pub fn provider_routing(mut self, provider: ProviderRouting) -> Self {
        self.default_provider = provider;
        self
    }

    /// Default OpenRouter `models` fallback list for every request.
    #[must_use]
    pub fn models(mut self, models: Vec<String>) -> Self {
        self.default_models = models;
        self
    }

    /// Configure independent LLM payload policies for the OTLP and Langfuse
    /// exporters. Disabled destinations should remain `None`.
    #[must_use]
    pub fn tracing(mut self, trace: LlmTraceConfig) -> Self {
        self.trace = trace;
        self
    }

    /// Build the client. Construction errors are retained and surfaced as
    /// provider errors when a request is made, preserving the older infallible
    /// constructor API.
    #[must_use]
    pub fn build(self) -> OpenRouterClient {
        let base_url = self
            .base_url
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        let mut builder =
            llmleaf_client::Client::builder(base_url.clone(), self.api_key.unwrap_or_default());
        if let Some(http) = self.http {
            builder = builder.http_client(http);
        }
        OpenRouterClient {
            inner: builder.build().map_err(|e| sdk_error_message(&e)),
            base_url,
            default_provider: self.default_provider,
            default_models: self.default_models,
            trace: self.trace,
        }
    }
}

/// A fully-collected, non-streaming turn (output of [`OpenRouterClient::chat`]).
///
/// Not `Eq` because [`usage`](Self::usage) carries an `f64` cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollectedTurn {
    /// The concatenated assistant text.
    pub content: String,
    /// Tool calls assembled from the streamed deltas, in index order.
    pub tool_calls: Vec<ToolCall>,
    /// The concatenated visible (open) reasoning text, if the model emitted any.
    pub reasoning: String,
    /// Structured reasoning blocks assembled from the stream, for the same-turn
    /// round-trip back to the provider (SOUL §7).
    pub reasoning_details: Vec<ReasoningDetail>,
    /// Provider finish reason, if reported.
    pub finish_reason: Option<FinishReason>,
    /// Token accounting, if reported.
    pub usage: Option<Usage>,
}

impl CollectedTurn {
    /// True if the model asked to call at least one tool.
    #[must_use]
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Drive a [`StreamEvent`] stream to completion, folding it into a
/// [`CollectedTurn`]. Used by the non-streaming [`OpenRouterClient::chat`]
/// helper; the agent loop folds turns itself (in `agent::stream_turn`) so it can
/// relay each event live.
pub(crate) async fn collect_turn(
    mut stream: BoxStream<'static, Result<StreamEvent>>,
) -> Result<CollectedTurn> {
    let mut content = String::new();
    let mut asm = ToolCallAssembler::default();
    let mut reasoning = String::new();
    let mut reasoning_asm = ReasoningAssembler::default();
    let mut finish_reason = None;
    let mut usage = None;

    while let Some(item) = stream.next().await {
        match item? {
            StreamEvent::TextDelta { text } => content.push_str(&text),
            StreamEvent::ReasoningDelta { text, details } => {
                reasoning.push_str(&text);
                reasoning_asm.extend(details);
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => asm.push(index, id, name, arguments),
            StreamEvent::Done {
                finish_reason: fr,
                usage: u,
            } => {
                finish_reason = fr;
                usage = u;
            }
            StreamEvent::Error { message } => return Err(Error::provider(message)),
            // Synthesized by the agent loop (tool dispatch / auto-compaction),
            // never produced by the model stream this folds — only here to keep
            // the match exhaustive.
            StreamEvent::ToolCallStarted { .. }
            | StreamEvent::ToolResult { .. }
            | StreamEvent::Compacted { .. } => {}
        }
    }

    Ok(CollectedTurn {
        content,
        tool_calls: asm.finish(),
        reasoning,
        reasoning_details: reasoning_asm.finish(),
        finish_reason,
        usage,
    })
}

/// Assembles streamed [`StreamEvent::ToolCallDelta`]s into complete
/// [`ToolCall`]s, keyed by stream index.
#[derive(Debug, Default)]
pub(crate) struct ToolCallAssembler {
    /// (index, id, name, arguments) accumulators, kept in first-seen order.
    slots: Vec<Slot>,
}

#[derive(Debug, Default)]
struct Slot {
    index: u32,
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAssembler {
    /// Apply one delta fragment.
    pub fn push(
        &mut self,
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) {
        let slot = match self.slots.iter_mut().find(|s| s.index == index) {
            Some(s) => s,
            None => {
                self.slots.push(Slot {
                    index,
                    ..Default::default()
                });
                self.slots.last_mut().expect("just pushed")
            }
        };
        if let Some(id) = id {
            if !id.is_empty() {
                slot.id = id;
            }
        }
        if let Some(name) = name {
            if !name.is_empty() {
                slot.name = name;
            }
        }
        if let Some(args) = arguments {
            slot.arguments.push_str(&args);
        }
    }

    /// Finalize into ordered tool calls. Empty/incomplete slots (no name) are
    /// dropped. Missing ids are synthesized so they can still be answered.
    pub fn finish(mut self) -> Vec<ToolCall> {
        self.slots.sort_by_key(|s| s.index);
        self.slots
            .into_iter()
            .filter(|s| !s.name.is_empty())
            .map(|s| ToolCall {
                id: if s.id.is_empty() {
                    format!("call_{}", s.index)
                } else {
                    s.id
                },
                name: s.name,
                arguments: if s.arguments.is_empty() {
                    "{}".to_string()
                } else {
                    s.arguments
                },
            })
            .collect()
    }
}

/// Assembles streamed [`ReasoningDetail`] fragments into complete blocks, keyed
/// by `index` (mirroring [`ToolCallAssembler`]). Fragments sharing an `index`
/// merge: visible `text`/`summary` concatenate; opaque `data`/`signature`/`id`/
/// `format`/`kind` are overwritten when the incoming fragment carries them.
/// Fragments without an `index` are kept as distinct blocks (not merged).
#[derive(Debug, Default)]
pub(crate) struct ReasoningAssembler {
    /// Blocks in first-seen order; `index: None` entries are never merged.
    slots: Vec<ReasoningDetail>,
}

impl ReasoningAssembler {
    /// Merge a batch of incoming fragments.
    pub(crate) fn extend(&mut self, details: Vec<ReasoningDetail>) {
        for detail in details {
            self.push(detail);
        }
    }

    /// Merge one fragment into its slot (by `index`), or start a new block.
    fn push(&mut self, incoming: ReasoningDetail) {
        let slot = incoming
            .index
            .and_then(|idx| self.slots.iter_mut().find(|s| s.index == Some(idx)));
        match slot {
            Some(s) => merge_reasoning(s, incoming),
            None => self.slots.push(incoming),
        }
    }

    /// Finalize into ordered reasoning blocks (sorted by `index`, `None` last).
    pub(crate) fn finish(mut self) -> Vec<ReasoningDetail> {
        self.slots.sort_by_key(|s| s.index.unwrap_or(u32::MAX));
        self.slots
    }
}

/// Fold `incoming` into the accumulating block `slot`: append visible text, set
/// opaque fields when present.
fn merge_reasoning(slot: &mut ReasoningDetail, incoming: ReasoningDetail) {
    if !incoming.kind.is_empty() {
        slot.kind = incoming.kind;
    }
    append_opt(&mut slot.text, incoming.text);
    append_opt(&mut slot.summary, incoming.summary);
    if incoming.data.is_some() {
        slot.data = incoming.data;
    }
    if incoming.signature.is_some() {
        slot.signature = incoming.signature;
    }
    if incoming.id.is_some() {
        slot.id = incoming.id;
    }
    if incoming.format.is_some() {
        slot.format = incoming.format;
    }
}

/// Append an incremental string fragment onto an accumulating optional field.
fn append_opt(acc: &mut Option<String>, fragment: Option<String>) {
    if let Some(frag) = fragment {
        acc.get_or_insert_with(String::new).push_str(&frag);
    }
}

/// Convert the SDK's typed Responses event stream into a `StreamEvent` stream
/// with a guaranteed terminal `Done`.
fn into_event_stream<S>(chunks: S) -> BoxStream<'static, Result<StreamEvent>>
where
    S: futures::Stream<Item = llmleaf_client::Result<llmleaf_client::ResponsesStreamEvent>>
        + Send
        + 'static,
{
    struct State<S> {
        chunks: std::pin::Pin<Box<S>>,
        ready: std::collections::VecDeque<Result<StreamEvent>>,
        ended: bool,
        done_emitted: bool,
        finish_reason: Option<FinishReason>,
        usage: Option<Usage>,
    }

    let state = State {
        chunks: Box::pin(chunks),
        ready: std::collections::VecDeque::new(),
        ended: false,
        done_emitted: false,
        finish_reason: None,
        usage: None,
    };

    stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.ready.pop_front() {
                return Some((ev, st));
            }

            if st.ended {
                if st.done_emitted {
                    return None;
                }
                st.done_emitted = true;
                let done = StreamEvent::Done {
                    finish_reason: st.finish_reason.take(),
                    usage: st.usage.take(),
                };
                return Some((Ok(done), st));
            }

            match st.chunks.next().await {
                Some(Ok(event)) => {
                    map_event(event, &mut st.finish_reason, &mut st.usage, &mut st.ready);
                }
                Some(Err(e)) => {
                    st.ready.push_back(Ok(StreamEvent::Error {
                        message: sdk_error_message(&e),
                    }));
                    st.ended = true;
                }
                None => st.ended = true,
            }
        }
    })
    .boxed()
}

/// Map one typed Responses SSE event into zero or more `StreamEvent`s + Done
/// metadata.
///
/// Live deltas carry the visible stream (text, reasoning text, tool-call
/// arguments). A reasoning item's structured details ride only its bracketing
/// `output_item.done` (which carries the completed block, `encrypted_content`
/// included) — the deltas deliberately carry none, so nothing double-counts
/// when [`ReasoningAssembler`] folds the fragments. The terminal snapshot
/// contributes only the finish reason and usage: its `output` repeats items
/// whose deltas already streamed.
fn map_event(
    event: llmleaf_client::ResponsesStreamEvent,
    finish_reason: &mut Option<FinishReason>,
    usage: &mut Option<Usage>,
    out: &mut std::collections::VecDeque<Result<StreamEvent>>,
) {
    match event.kind.as_str() {
        "response.output_text.delta" => {
            if let Some(text) = event.delta {
                if !text.is_empty() {
                    out.push_back(Ok(StreamEvent::TextDelta { text }));
                }
            }
        }
        "response.reasoning_text.delta" => {
            if let Some(text) = event.delta {
                if !text.is_empty() {
                    out.push_back(Ok(StreamEvent::ReasoningDelta {
                        text,
                        details: Vec::new(),
                    }));
                }
            }
        }
        // A function call opens with its identity (call_id + name)…
        "response.output_item.added" => {
            if let Some(llmleaf_client::ResponseItem::FunctionCall(call)) = event.item {
                out.push_back(Ok(StreamEvent::ToolCallDelta {
                    index: event.output_index.unwrap_or(0),
                    id: Some(call.call_id),
                    name: Some(call.name),
                    arguments: (!call.arguments.is_empty()).then_some(call.arguments),
                }));
            }
        }
        // …then streams its argument fragments under the same output index.
        "response.function_call_arguments.delta" => {
            if let Some(arguments) = event.delta {
                out.push_back(Ok(StreamEvent::ToolCallDelta {
                    index: event.output_index.unwrap_or(0),
                    id: None,
                    name: None,
                    arguments: Some(arguments),
                }));
            }
        }
        "response.output_item.done" => {
            if let Some(llmleaf_client::ResponseItem::Reasoning(item)) = event.item {
                let details = reasoning_details_from_item(item, event.output_index);
                if !details.is_empty() {
                    out.push_back(Ok(StreamEvent::ReasoningDelta {
                        text: String::new(),
                        details,
                    }));
                }
            }
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            let Some(resp) = event.response else {
                *finish_reason = Some(FinishReason::Other);
                return;
            };
            if let Some(u) = resp.usage.as_ref() {
                *usage = Some(map_responses_usage(u));
            }
            *finish_reason = Some(map_responses_finish(&resp));
            if resp.status == "failed" {
                let message = resp
                    .error
                    .map(|e| e.message)
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "response failed".to_string());
                out.push_back(Ok(StreamEvent::Error { message }));
            }
        }
        // Defensive only: the SDK surfaces the `"error"` event as an `Err` item,
        // so it should never reach this Ok-side arm — map it rather than drop it.
        "error" => {
            out.push_back(Ok(StreamEvent::Error {
                message: event
                    .message
                    .unwrap_or_else(|| "provider error".to_string()),
            }));
        }
        // Everything else (created / in_progress snapshots, content-part
        // brackets, `.done` echoes of already-streamed deltas) carries nothing
        // the fold needs.
        _ => {}
    }
}

/// Derive the core [`FinishReason`] from a terminal Responses snapshot: the
/// dialect reports tool use as a plain `"completed"`, so a completed turn with
/// `function_call` items in its output is `ToolCalls` (the agent loop's signal
/// to dispatch and continue).
fn map_responses_finish(resp: &llmleaf_client::ResponsesResponse) -> FinishReason {
    match resp.status.as_str() {
        "completed" => {
            if resp.function_calls().is_empty() {
                FinishReason::Stop
            } else {
                FinishReason::ToolCalls
            }
        }
        "incomplete" => match resp.incomplete_details.as_ref().map(|d| d.reason.as_str()) {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            _ => FinishReason::Other,
        },
        _ => FinishReason::Other,
    }
}

/// Map the Responses dialect's usage names onto the core [`Usage`].
///
/// The SDK's `ResponsesUsage` does not model the gateway's `cost_usd`
/// extension or a cache-write count, so both come back as their empty values on
/// this path; the chat-shaped batch path still carries cost.
fn map_responses_usage(usage: &llmleaf_client::ResponsesUsage) -> Usage {
    Usage {
        prompt_tokens: usage.input_tokens,
        completion_tokens: usage.output_tokens,
        total_tokens: usage.total_tokens,
        cost_usd: None,
        cached_tokens: usage.cached_tokens(),
        cache_creation_tokens: 0,
    }
}

/// A completed `reasoning` output item → core details, one per part, all
/// sharing the item's output index so [`ReasoningAssembler`] folds them into
/// one block per item while later items stay distinct. The SDK's typed item
/// has no `signature` field, so an OpenRouter-signed open-reasoning
/// block arrives without its signature here; [`to_response_reasoning_item`]
/// still replays any signature catalerum has (e.g. from persisted history).
fn reasoning_details_from_item(
    item: llmleaf_client::ResponseReasoningItem,
    output_index: Option<u32>,
) -> Vec<ReasoningDetail> {
    let mut details = Vec::new();
    for entry in item.summary {
        details.push(ReasoningDetail {
            kind: "reasoning.summary".to_string(),
            summary: Some(entry.text),
            id: item.id.clone(),
            index: output_index,
            ..Default::default()
        });
    }
    for entry in item.content {
        details.push(ReasoningDetail {
            kind: "reasoning.text".to_string(),
            text: Some(entry.text),
            id: item.id.clone(),
            index: output_index,
            ..Default::default()
        });
    }
    if let Some(data) = item.encrypted_content {
        details.push(ReasoningDetail {
            kind: "reasoning.encrypted".to_string(),
            data: Some(data),
            id: item.id,
            index: output_index,
            ..Default::default()
        });
    }
    details
}

/// A pre-built `Error -> Done` stream for surfaced API failures (non-2xx, etc.).
fn error_then_done(message: String) -> BoxStream<'static, Result<StreamEvent>> {
    stream::iter(vec![
        Ok(StreamEvent::Error { message }),
        Ok(StreamEvent::Done {
            finish_reason: Some(FinishReason::Other),
            usage: None,
        }),
    ])
    .boxed()
}

pub(crate) fn map_sdk_error(e: llmleaf_client::Error) -> Error {
    Error::provider(sdk_error_message(&e))
}

pub(crate) fn sdk_error_message(e: &llmleaf_client::Error) -> String {
    match e {
        llmleaf_client::Error::Api { message, .. } if !message.is_empty() => message.clone(),
        _ => e.to_string(),
    }
}

/// Flatten catalerum's role-based history into the Responses `input` item list.
///
/// A system/user message is one message item; a `tool` message is the
/// `function_call_output` answering its `tool_call_id`; an assistant message
/// expands to (in order) its reasoning items, its message item (when it has
/// text), and one `function_call` item per tool call — the same reasoning-first
/// order the dialect's own output uses, so a reasoning model's chain survives
/// the mid-turn round-trip (SOUL §7). The dialect has no per-message `name`, so
/// that field does not travel on this path.
/// A user turn as one Responses input item: plain text, or — when the turn
/// carries inline images (a vision seed, SOUL §7/§9) — a message whose content is
/// an `input_text` part (when there's text) followed by one `input_image` part
/// per image url.
fn to_response_user_item(msg: &ChatMessage) -> llmleaf_client::ResponseItem {
    if msg.images.is_empty() && msg.media.is_empty() {
        return llmleaf_client::ResponseItem::user(msg.content.clone());
    }
    let mut parts = Vec::with_capacity(msg.images.len() + msg.media.len() + 1);
    if !msg.content.is_empty() {
        parts.push(llmleaf_client::ResponseContentPart::input_text(
            msg.content.clone(),
        ));
    }
    for url in &msg.images {
        parts.push(llmleaf_client::ResponseContentPart::input_image(
            url.clone(),
        ));
    }
    for media in &msg.media {
        let MediaInput::Image { url } = media;
        parts.push(llmleaf_client::ResponseContentPart::input_image(
            url.clone(),
        ));
    }
    llmleaf_client::ResponseItem::user(parts)
}

fn to_response_items(messages: &[ChatMessage]) -> Vec<llmleaf_client::ResponseItem> {
    let mut items = Vec::new();
    for msg in messages {
        match msg.role {
            MessageRole::System => {
                items.push(llmleaf_client::ResponseItem::system(msg.content.clone()));
            }
            MessageRole::User => {
                items.push(to_response_user_item(msg));
            }
            MessageRole::Tool => {
                items.push(llmleaf_client::ResponseItem::function_call_output(
                    msg.tool_call_id.clone().unwrap_or_default(),
                    msg.content.clone(),
                ));
            }
            MessageRole::Assistant => {
                for detail in &msg.reasoning_details {
                    items.push(to_response_reasoning_item(detail));
                }
                // Open reasoning captured only as visible text (no structured
                // block — e.g. history persisted by an older dialect) still
                // replays, as one plain reasoning item.
                if msg.reasoning_details.is_empty() {
                    if let Some(text) = msg.reasoning.as_deref().filter(|t| !t.is_empty()) {
                        items.push(llmleaf_client::ResponseItem::Reasoning(
                            llmleaf_client::ResponseReasoningItem {
                                content: vec![llmleaf_client::ResponseReasoningText::new(text)],
                                ..Default::default()
                            },
                        ));
                    }
                }
                if !msg.content.is_empty() {
                    items.push(llmleaf_client::ResponseItem::assistant(msg.content.clone()));
                }
                for call in &msg.tool_calls {
                    items.push(llmleaf_client::ResponseItem::function_call(
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    ));
                }
            }
        }
    }
    items
}

/// One core [`ReasoningDetail`] → one `reasoning` input item. Field presence
/// drives the mapping (the `kind` vocabulary varies by upstream): `text` → a
/// `content` entry, `summary` → a `summary` entry, `data` → `encrypted_content`.
/// The SDK's typed item has no `signature` field, but the gateway
/// accepts the OpenRouter dialect's item-level `signature` on input — so a
/// signed detail is spliced into a raw item, keeping the signed block intact on
/// replay (some providers reject an altered/dropped signed block before a tool
/// call).
fn to_response_reasoning_item(detail: &ReasoningDetail) -> llmleaf_client::ResponseItem {
    let mut item = llmleaf_client::ResponseReasoningItem {
        id: detail.id.clone(),
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content: detail.data.clone(),
    };
    if let Some(summary) = &detail.summary {
        item.summary
            .push(llmleaf_client::ResponseReasoningText::new(summary.clone()));
    }
    if let Some(text) = &detail.text {
        item.content
            .push(llmleaf_client::ResponseReasoningText::new(text.clone()));
    }
    let item = llmleaf_client::ResponseItem::Reasoning(item);
    let Some(signature) = &detail.signature else {
        return item;
    };
    match serde_json::to_value(&item) {
        Ok(mut value) => {
            if let Some(obj) = value.as_object_mut() {
                obj.insert("signature".to_string(), serde_json::json!(signature));
            }
            llmleaf_client::ResponseItem::Other(value)
        }
        // Serializing these plain structs cannot realistically fail; if it ever
        // does, replaying the block unsigned beats dropping it.
        Err(_) => item,
    }
}

/// `tool_choice` in the Responses dialect (bare mode string / flat named form).
fn to_responses_tool_choice(choice: &ToolChoice) -> llmleaf_client::ResponsesToolChoice {
    match choice {
        ToolChoice::Auto => llmleaf_client::ResponsesToolChoice::mode("auto"),
        ToolChoice::None => llmleaf_client::ResponsesToolChoice::mode("none"),
        ToolChoice::Required => llmleaf_client::ResponsesToolChoice::mode("required"),
        ToolChoice::Function { name } => llmleaf_client::ResponsesToolChoice::named(name.clone()),
    }
}

/// A message's content in the chat wire: a plain text string, or — when the turn
/// carries inline images (a vision seed, SOUL §7/§9) — an array of content parts
/// (the text part, when present, then one `image_url` part per image url).
fn to_sdk_content(message: &catalerum_core::llm::ChatMessage) -> llmleaf_client::Content {
    if message.images.is_empty() && message.media.is_empty() {
        return llmleaf_client::Content::Text(message.content.clone());
    }
    let mut parts = Vec::with_capacity(message.images.len() + message.media.len() + 1);
    if !message.content.is_empty() {
        parts.push(llmleaf_client::ContentPart::text(message.content.clone()));
    }
    for url in &message.images {
        parts.push(llmleaf_client::ContentPart::image_url(url.clone()));
    }
    for media in &message.media {
        let MediaInput::Image { url } = media;
        parts.push(llmleaf_client::ContentPart::image_url(url.clone()));
    }
    llmleaf_client::Content::Parts(parts)
}

fn to_sdk_message(message: &catalerum_core::llm::ChatMessage) -> llmleaf_client::ChatMessage {
    llmleaf_client::ChatMessage {
        role: to_sdk_role(message.role),
        content: Some(to_sdk_content(message)),
        name: message.name.clone(),
        tool_calls: message.tool_calls.iter().map(to_sdk_tool_call).collect(),
        tool_call_id: message.tool_call_id.clone(),
        // Echo any captured reasoning back verbatim. The agent loop carries an
        // assistant turn's `reasoning_details` onto the next (tool-result) request
        // within a turn so a reasoning model's signed chain survives the round-trip
        // (some providers reject an altered/dropped signed block before a tool call).
        reasoning: message.reasoning.clone(),
        reasoning_details: message
            .reasoning_details
            .iter()
            .map(to_sdk_reasoning)
            .collect(),
    }
}

/// Map a core [`ReasoningDetail`] to the SDK's (round-trip / send path).
fn to_sdk_reasoning(detail: &ReasoningDetail) -> llmleaf_client::ReasoningDetail {
    llmleaf_client::ReasoningDetail {
        kind: detail.kind.clone(),
        text: detail.text.clone(),
        summary: detail.summary.clone(),
        data: detail.data.clone(),
        signature: detail.signature.clone(),
        id: detail.id.clone(),
        format: detail.format.clone(),
        index: detail.index,
    }
}

/// Map the SDK's [`ReasoningDetail`] to a core one (receive path).
fn to_core_reasoning(detail: llmleaf_client::ReasoningDetail) -> ReasoningDetail {
    ReasoningDetail {
        kind: detail.kind,
        text: detail.text,
        summary: detail.summary,
        data: detail.data,
        signature: detail.signature,
        id: detail.id,
        format: detail.format,
        index: detail.index,
    }
}

/// Fold a non-streaming SDK [`ChatResponse`](llmleaf_client::ChatResponse) into a
/// [`CollectedTurn`] — the same shape the streaming path produces. Used by the
/// batch API, whose per-item results are full (non-streamed) completions.
pub(crate) fn collected_from_response(resp: llmleaf_client::ChatResponse) -> CollectedTurn {
    let usage = resp.usage.map(map_usage);
    let Some(choice) = resp.choices.into_iter().next() else {
        return CollectedTurn {
            usage,
            ..Default::default()
        };
    };
    let finish_reason = choice.finish_reason.map(map_finish_reason);
    let msg = choice.message;
    let reasoning = msg.reasoning_text().unwrap_or_default();
    let content = msg.text().map(str::to_string).unwrap_or_default();
    let tool_calls = msg.tool_calls.iter().map(to_core_tool_call).collect();
    let reasoning_details = msg
        .reasoning_details
        .into_iter()
        .map(to_core_reasoning)
        .collect();
    CollectedTurn {
        content,
        tool_calls,
        reasoning,
        reasoning_details,
        finish_reason,
        usage,
    }
}

/// Map an SDK tool call back to a core one (receive path).
fn to_core_tool_call(call: &llmleaf_client::ToolCall) -> ToolCall {
    ToolCall {
        id: call.id.clone(),
        name: call.function.name.clone(),
        arguments: call.function.arguments.clone(),
    }
}

fn to_sdk_tool_call(call: &ToolCall) -> llmleaf_client::ToolCall {
    llmleaf_client::ToolCall {
        id: call.id.clone(),
        kind: "function".to_string(),
        function: llmleaf_client::FunctionCall {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        },
    }
}

fn to_sdk_tool_choice(choice: &ToolChoice) -> llmleaf_client::ToolChoice {
    match choice {
        ToolChoice::Auto => llmleaf_client::ToolChoice::mode("auto"),
        ToolChoice::None => llmleaf_client::ToolChoice::mode("none"),
        ToolChoice::Required => llmleaf_client::ToolChoice::mode("required"),
        ToolChoice::Function { name } => llmleaf_client::ToolChoice::named(name.clone()),
    }
}

fn to_sdk_role(role: MessageRole) -> llmleaf_client::Role {
    match role {
        MessageRole::System => llmleaf_client::Role::System,
        MessageRole::User => llmleaf_client::Role::User,
        MessageRole::Assistant => llmleaf_client::Role::Assistant,
        MessageRole::Tool => llmleaf_client::Role::Tool,
    }
}

fn map_finish_reason(reason: llmleaf_client::FinishReason) -> FinishReason {
    match reason {
        llmleaf_client::FinishReason::Stop => FinishReason::Stop,
        llmleaf_client::FinishReason::ToolCalls => FinishReason::ToolCalls,
        llmleaf_client::FinishReason::Length => FinishReason::Length,
        llmleaf_client::FinishReason::ContentFilter => FinishReason::ContentFilter,
    }
}

pub(crate) fn map_usage(usage: llmleaf_client::Usage) -> Usage {
    Usage {
        prompt_tokens: usage.prompt_tokens,
        completion_tokens: usage.completion_tokens,
        total_tokens: usage.total_tokens,
        cost_usd: usage.cost_usd,
        cached_tokens: usage.cached_tokens(),
        cache_creation_tokens: usage.cache_writes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream::TryStreamExt;

    /// A bare Responses SSE event of the given kind; set the fields a test needs.
    fn ev(kind: &str) -> llmleaf_client::ResponsesStreamEvent {
        llmleaf_client::ResponsesStreamEvent {
            kind: kind.into(),
            sequence_number: 0,
            response: None,
            output_index: None,
            item_id: None,
            content_index: None,
            item: None,
            part: None,
            delta: None,
            text: None,
            arguments: None,
            message: None,
        }
    }

    fn delta_ev(
        kind: &str,
        delta: &str,
        output_index: u32,
    ) -> llmleaf_client::ResponsesStreamEvent {
        let mut e = ev(kind);
        e.delta = Some(delta.into());
        e.output_index = Some(output_index);
        e
    }

    fn item_ev(
        kind: &str,
        item: llmleaf_client::ResponseItem,
        output_index: u32,
    ) -> llmleaf_client::ResponsesStreamEvent {
        let mut e = ev(kind);
        e.item = Some(item);
        e.output_index = Some(output_index);
        e
    }

    fn snapshot(
        status: &str,
        output: Vec<llmleaf_client::ResponseItem>,
    ) -> llmleaf_client::ResponsesResponse {
        llmleaf_client::ResponsesResponse {
            id: "resp_1".into(),
            object: "response".into(),
            created_at: 1,
            status: status.into(),
            incomplete_details: None,
            error: None,
            model: "m".into(),
            output,
            usage: None,
            store: Some(false),
            instructions: None,
            max_output_tokens: None,
            temperature: None,
            top_p: None,
            reasoning: None,
        }
    }

    fn terminal(
        kind: &str,
        resp: llmleaf_client::ResponsesResponse,
    ) -> llmleaf_client::ResponsesStreamEvent {
        let mut e = ev(kind);
        e.response = Some(resp);
        e
    }

    #[tokio::test]
    async fn streams_text_then_done() {
        let mut completed = snapshot("completed", Vec::new());
        completed.usage = Some(llmleaf_client::ResponsesUsage {
            input_tokens: 11,
            input_tokens_details: None,
            output_tokens: 2,
            output_tokens_details: None,
            total_tokens: 13,
        });
        let s = stream::iter(vec![
            Ok(delta_ev("response.output_text.delta", "Hel", 0)),
            Ok(delta_ev("response.output_text.delta", "lo", 0)),
            Ok(terminal("response.completed", completed)),
        ]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta { text: "Hel".into() },
                StreamEvent::TextDelta { text: "lo".into() },
                StreamEvent::Done {
                    finish_reason: Some(FinishReason::Stop),
                    usage: Some(Usage {
                        prompt_tokens: 11,
                        completion_tokens: 2,
                        total_tokens: 13,
                        ..Default::default()
                    }),
                },
            ]
        );
    }

    #[tokio::test]
    async fn always_ends_with_done_on_eof() {
        // Connection drops before the terminal event: still exactly one Done.
        let s = stream::iter(vec![Ok(delta_ev("response.output_text.delta", "hi", 0))]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                finish_reason: None,
                usage: None,
            })
        );
    }

    #[tokio::test]
    async fn assembles_tool_calls() {
        // The item-added bracket carries the identity; argument fragments stream
        // under the same output index; the terminal snapshot's function_call item
        // turns the plain "completed" into a ToolCalls finish.
        let call_item = llmleaf_client::ResponseItem::function_call("c1", "f", "");
        let s = stream::iter(vec![
            Ok(item_ev("response.output_item.added", call_item.clone(), 0)),
            Ok(delta_ev(
                "response.function_call_arguments.delta",
                "{\"a\":",
                0,
            )),
            Ok(delta_ev("response.function_call_arguments.delta", "1}", 0)),
            Ok(terminal(
                "response.completed",
                snapshot(
                    "completed",
                    vec![llmleaf_client::ResponseItem::function_call(
                        "c1",
                        "f",
                        "{\"a\":1}",
                    )],
                ),
            )),
        ]);
        let collected = collect_turn(into_event_stream(s)).await.unwrap();
        assert_eq!(collected.tool_calls.len(), 1);
        assert_eq!(collected.tool_calls[0].id, "c1");
        assert_eq!(collected.tool_calls[0].name, "f");
        assert_eq!(collected.tool_calls[0].arguments, "{\"a\":1}");
        assert_eq!(collected.finish_reason, Some(FinishReason::ToolCalls));
        assert!(collected.wants_tools());
    }

    #[tokio::test]
    async fn forwards_tool_call_fragments_before_terminal_done() {
        let call_item = llmleaf_client::ResponseItem::function_call("call_1", "weather", "");
        let s = stream::iter(vec![
            Ok(item_ev("response.output_item.added", call_item.clone(), 0)),
            Ok(delta_ev(
                "response.function_call_arguments.delta",
                "{\"city\":\"Par",
                0,
            )),
            Ok(delta_ev(
                "response.function_call_arguments.delta",
                "is\"}",
                0,
            )),
            Ok(terminal(
                "response.completed",
                snapshot(
                    "completed",
                    vec![llmleaf_client::ResponseItem::function_call(
                        "call_1",
                        "weather",
                        "{\"city\":\"Paris\"}",
                    )],
                ),
            )),
        ]);

        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("weather".into()),
                    arguments: None,
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: Some("{\"city\":\"Par".into()),
                },
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: Some("is\"}".into()),
                },
                StreamEvent::Done {
                    finish_reason: Some(FinishReason::ToolCalls),
                    usage: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn incomplete_maps_length_and_content_filter() {
        let mut resp = snapshot("incomplete", Vec::new());
        resp.incomplete_details = Some(llmleaf_client::ResponsesIncompleteDetails {
            reason: "max_output_tokens".into(),
        });
        let s = stream::iter(vec![Ok(terminal("response.incomplete", resp))]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                finish_reason: Some(FinishReason::Length),
                usage: None,
            })
        );
    }

    #[tokio::test]
    async fn failed_surfaces_error_then_done() {
        let mut resp = snapshot("failed", Vec::new());
        resp.error = Some(llmleaf_client::ResponsesError {
            message: "upstream exploded".into(),
            kind: None,
            code: None,
        });
        let s = stream::iter(vec![Ok(terminal("response.failed", resp))]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events[0],
            StreamEvent::Error {
                message: "upstream exploded".into()
            }
        );
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                finish_reason: Some(FinishReason::Other),
                usage: None,
            })
        );
    }

    #[test]
    fn tool_call_assembler_handles_multi_call_ordering_and_no_clobber() {
        // Two calls whose deltas interleave and arrive out of index order; id/name
        // are sent once then fragments of arguments stream in; a later **empty**
        // id/name fragment must NOT clobber the established values (the bug this
        // guards). finish() sorts by index.
        let mut asm = ToolCallAssembler::default();
        asm.push(
            1,
            Some("b1".into()),
            Some("beta".into()),
            Some("{\"y\":".into()),
        );
        asm.push(
            0,
            Some("a1".into()),
            Some("alpha".into()),
            Some("{\"x\":".into()),
        );
        asm.push(
            0,
            Some(String::new()),
            Some(String::new()),
            Some("1}".into()),
        );
        asm.push(1, None, None, Some("2}".into()));
        let calls = asm.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].id, "a1",
            "index 0 first; empty delta didn't clobber id"
        );
        assert_eq!(calls[0].name, "alpha", "empty delta didn't clobber name");
        assert_eq!(
            calls[0].arguments, "{\"x\":1}",
            "argument fragments concatenated"
        );
        assert_eq!(calls[1].id, "b1");
        assert_eq!(calls[1].name, "beta");
        assert_eq!(calls[1].arguments, "{\"y\":2}");
    }

    #[test]
    fn tool_call_assembler_synthesizes_id_defaults_args_and_drops_nameless() {
        let mut asm = ToolCallAssembler::default();
        // No id (provider omitted it) and no arguments → synthesized id + "{}".
        asm.push(0, None, Some("f".into()), None);
        // A slot that never gets a name is incomplete and is dropped.
        asm.push(2, Some("x".into()), None, Some("{}".into()));
        let calls = asm.finish();
        assert_eq!(calls.len(), 1, "the nameless slot is dropped");
        assert_eq!(
            calls[0].id, "call_0",
            "missing id synthesized from the index"
        );
        assert_eq!(calls[0].name, "f");
        assert_eq!(
            calls[0].arguments, "{}",
            "empty arguments defaulted to an empty object"
        );
    }

    #[tokio::test]
    async fn surfaces_stream_error_then_done() {
        let s = stream::iter(vec![Err(llmleaf_client::Error::Stream("boom".into()))]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert_eq!(
            events[0],
            StreamEvent::Error {
                message: "malformed stream: boom".into()
            }
        );
        assert!(matches!(events.last(), Some(StreamEvent::Done { .. })));
    }

    #[test]
    fn collected_default_has_no_tools() {
        assert!(!CollectedTurn::default().wants_tools());
    }

    // ---- reasoning / usage ----------------------------------------------

    #[tokio::test]
    async fn streams_reasoning_as_its_own_event() {
        // A reasoning_text delta is surfaced distinctly and never folded into
        // content.
        let s = stream::iter(vec![Ok(delta_ev(
            "response.reasoning_text.delta",
            "hmm",
            0,
        ))]);
        let events: Vec<StreamEvent> = into_event_stream(s).try_collect().await.unwrap();
        assert!(matches!(
            events.first(),
            Some(StreamEvent::ReasoningDelta { text, .. }) if text == "hmm"
        ));
    }

    #[tokio::test]
    async fn collect_turn_captures_reasoning_and_folds_done_item() {
        // Visible text streams as deltas (details-less, so nothing double-counts);
        // the structured block arrives once, on the item's output_item.done.
        let done_item =
            llmleaf_client::ResponseItem::Reasoning(llmleaf_client::ResponseReasoningItem {
                id: Some("rs_1_0".into()),
                summary: Vec::new(),
                content: vec![llmleaf_client::ResponseReasoningText::new("Let me think.")],
                encrypted_content: Some("blob".into()),
            });
        let s = stream::iter(vec![
            Ok(delta_ev("response.reasoning_text.delta", "Let me ", 0)),
            Ok(delta_ev("response.reasoning_text.delta", "think.", 0)),
            Ok(item_ev("response.output_item.done", done_item, 0)),
            Ok(delta_ev("response.output_text.delta", "Hi", 1)),
            Ok(terminal(
                "response.completed",
                snapshot("completed", Vec::new()),
            )),
        ]);
        let collected = collect_turn(into_event_stream(s)).await.unwrap();
        assert_eq!(collected.content, "Hi");
        assert_eq!(collected.reasoning, "Let me think.");
        // The done item's text + encrypted parts share its output index, so the
        // assembler folds them into one block for the same-turn round-trip.
        assert_eq!(collected.reasoning_details.len(), 1);
        let d = &collected.reasoning_details[0];
        assert_eq!(d.text.as_deref(), Some("Let me think."));
        assert_eq!(d.data.as_deref(), Some("blob"));
        assert_eq!(d.id.as_deref(), Some("rs_1_0"));
        assert_eq!(d.index, Some(0));
    }

    #[test]
    fn reasoning_assembler_merges_by_index_keeps_none_separate() {
        let mut asm = ReasoningAssembler::default();
        asm.extend(vec![
            ReasoningDetail {
                kind: "reasoning.text".into(),
                text: Some("a".into()),
                index: Some(1),
                ..Default::default()
            },
            ReasoningDetail {
                kind: "reasoning.text".into(),
                text: Some("b".into()),
                index: Some(0),
                ..Default::default()
            },
        ]);
        asm.extend(vec![
            // Same index 0: merges (text concatenates, signature set, empty kind
            // does not clobber).
            ReasoningDetail {
                kind: String::new(),
                text: Some("c".into()),
                signature: Some("s".into()),
                index: Some(0),
                ..Default::default()
            },
            // No index: kept as a distinct block.
            ReasoningDetail {
                kind: "reasoning.encrypted".into(),
                data: Some("blob".into()),
                index: None,
                ..Default::default()
            },
        ]);
        let out = asm.finish();
        assert_eq!(out.len(), 3);
        // Sorted by index (None sorts last).
        assert_eq!(out[0].index, Some(0));
        assert_eq!(out[0].text.as_deref(), Some("bc"));
        assert_eq!(out[0].signature.as_deref(), Some("s"));
        assert_eq!(out[0].kind, "reasoning.text");
        assert_eq!(out[1].index, Some(1));
        assert_eq!(out[2].index, None);
        assert!(out[2].is_hidden());
    }

    #[test]
    fn to_sdk_message_round_trips_reasoning() {
        let msg = catalerum_core::llm::ChatMessage {
            role: MessageRole::Assistant,
            content: "answer".into(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: Some("because".into()),
            reasoning_details: vec![ReasoningDetail {
                kind: "reasoning.encrypted".into(),
                data: Some("blob".into()),
                signature: Some("sig".into()),
                index: Some(0),
                ..Default::default()
            }],
        };
        let sdk = to_sdk_message(&msg);
        assert_eq!(sdk.reasoning.as_deref(), Some("because"));
        assert_eq!(sdk.reasoning_details.len(), 1);
        assert_eq!(sdk.reasoning_details[0].kind, "reasoning.encrypted");
        assert_eq!(sdk.reasoning_details[0].data.as_deref(), Some("blob"));
        assert_eq!(sdk.reasoning_details[0].signature.as_deref(), Some("sig"));
    }

    #[test]
    fn to_chat_request_sets_reasoning_effort() {
        // The chat-shaped encoding survives for the batch path.
        let c = OpenRouterClient::new("http://example", "k");
        let mut req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        req.reasoning_effort = Some("high".into());
        let sdk = c.to_chat_request(&req);
        assert_eq!(sdk.reasoning_effort.as_deref(), Some("high"));
    }

    // ---- responses request encoding ---------------------------------------

    #[test]
    fn to_responses_request_maps_knobs_tools_and_defaults() {
        let c = OpenRouterClient::builder()
            .base_url("http://example")
            .api_key("k")
            .provider_routing(ProviderRouting {
                order: vec!["openai".into()],
                ..Default::default()
            })
            .models(vec!["fallback".into()])
            .build();
        let mut req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        req.reasoning_effort = Some("high".into());
        req.temperature = Some(0.2);
        req.max_tokens = Some(512);
        req.tools = vec![catalerum_core::llm::ToolSpec {
            name: "f".into(),
            description: "does f".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        req.tool_choice = Some(ToolChoice::Function { name: "f".into() });

        let sdk = c.to_responses_request(&req);
        assert_eq!(sdk.model, "m");
        assert_eq!(sdk.temperature, Some(0.2));
        assert_eq!(sdk.max_output_tokens, Some(512));
        assert_eq!(
            sdk.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
            Some("high")
        );
        // Flat tool definition (no nested `function` object in this dialect).
        assert_eq!(sdk.tools.len(), 1);
        assert_eq!(sdk.tools[0].kind, "function");
        assert_eq!(sdk.tools[0].name, "f");
        assert_eq!(sdk.tools[0].description.as_deref(), Some("does f"));
        assert!(matches!(
            &sdk.tool_choice,
            Some(llmleaf_client::ResponsesToolChoice::Named(n)) if n.name == "f"
        ));
        // Builder defaults land in the top-level passthrough map.
        let extra = sdk.extra.expect("defaults populate extra");
        assert_eq!(extra["provider"]["order"], serde_json::json!(["openai"]));
        assert_eq!(extra["models"], serde_json::json!(["fallback"]));
    }

    #[test]
    fn history_maps_to_input_items() {
        // system/user → message items; assistant → reasoning + message +
        // function_call items (in that order); tool → function_call_output.
        let assistant = ChatMessage {
            role: MessageRole::Assistant,
            content: "calling f".into(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "f".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: None,
            name: None,
            reasoning: Some("because".into()),
            reasoning_details: vec![ReasoningDetail {
                kind: "reasoning.text".into(),
                text: Some("because".into()),
                signature: Some("sig".into()),
                index: Some(0),
                ..Default::default()
            }],
        };
        let tool = ChatMessage {
            role: MessageRole::Tool,
            content: "42".into(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some("c1".into()),
            name: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        };
        let items = to_response_items(&[
            ChatMessage::system("be brief"),
            ChatMessage::user("hi"),
            assistant,
            tool,
        ]);
        let wire = serde_json::to_value(&items).unwrap();
        assert_eq!(
            wire,
            serde_json::json!([
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" },
                // The signed detail rides as a raw item so its signature still
                // replays even though the typed SDK item cannot carry it.
                {
                    "type": "reasoning",
                    "content": [{ "type": "reasoning_text", "text": "because" }],
                    "signature": "sig",
                },
                { "role": "assistant", "content": "calling f" },
                { "type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}" },
                { "type": "function_call_output", "call_id": "c1", "output": "42" },
            ])
        );
    }

    #[test]
    fn user_images_become_multimodal_content_parts() {
        // A user turn carrying inline images (a vision seed, SOUL §7/§9) emits
        // multimodal content parts in BOTH dialects — text first, then one image
        // part per url — instead of a plain text string.
        let mut msg = ChatMessage::user("look at this");
        msg.images = vec!["data:image/png;base64,AAAA".into()];

        // Responses dialect: `input_text` + `input_image` (plain-string image_url).
        let items = to_response_items(std::slice::from_ref(&msg));
        let wire = serde_json::to_value(&items).unwrap();
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(
            wire[0]["content"],
            serde_json::json!([
                { "type": "input_text", "text": "look at this" },
                { "type": "input_image", "image_url": "data:image/png;base64,AAAA" },
            ])
        );

        // Chat dialect: `text` + `image_url` (nested `{url}` object).
        let sdk = serde_json::to_value(to_sdk_message(&msg)).unwrap();
        assert_eq!(sdk["role"], "user");
        assert_eq!(
            sdk["content"],
            serde_json::json!([
                { "type": "text", "text": "look at this" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
            ])
        );
    }

    #[test]
    fn native_tool_image_becomes_a_real_llmleaf_image_part() {
        let mut msg = ChatMessage::user("inspect this");
        msg.media = vec![MediaInput::Image {
            url: "data:image/png;base64,AA==".into(),
        }];
        let value = serde_json::to_value(to_response_items(&[msg])).unwrap();
        let content = value[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,AA==");
    }

    #[test]
    fn reasoning_string_without_details_still_replays() {
        // History persisted before structured details existed: the visible
        // reasoning text alone becomes one plain reasoning item.
        let assistant = ChatMessage {
            role: MessageRole::Assistant,
            content: "hi".into(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: Some("thought".into()),
            reasoning_details: Vec::new(),
        };
        let wire = serde_json::to_value(to_response_items(&[assistant])).unwrap();
        assert_eq!(
            wire,
            serde_json::json!([
                { "type": "reasoning", "content": [{ "type": "reasoning_text", "text": "thought" }] },
                { "role": "assistant", "content": "hi" },
            ])
        );
    }

    #[test]
    fn encrypted_detail_maps_to_encrypted_content() {
        let detail = ReasoningDetail {
            kind: "reasoning.encrypted".into(),
            data: Some("blob".into()),
            ..Default::default()
        };
        let wire = serde_json::to_value(vec![to_response_reasoning_item(&detail)]).unwrap();
        assert_eq!(
            wire,
            serde_json::json!([{ "type": "reasoning", "encrypted_content": "blob" }])
        );
    }

    #[test]
    fn map_responses_usage_carries_cache_reads() {
        let u = llmleaf_client::ResponsesUsage {
            input_tokens: 10,
            input_tokens_details: Some(llmleaf_client::ResponsesInputTokensDetails {
                cached_tokens: Some(4),
            }),
            output_tokens: 5,
            output_tokens_details: None,
            total_tokens: 15,
        };
        let mapped = map_responses_usage(&u);
        assert_eq!(mapped.prompt_tokens, 10);
        assert_eq!(mapped.completion_tokens, 5);
        assert_eq!(mapped.total_tokens, 15);
        assert_eq!(mapped.cached_tokens, 4);
        // Not modelled by the SDK's responses usage — honest empties.
        assert_eq!(mapped.cost_usd, None);
        assert_eq!(mapped.cache_creation_tokens, 0);
    }

    #[test]
    fn map_usage_carries_cost_and_cache() {
        let sdk = llmleaf_client::Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cost_usd: Some(0.002),
            prompt_tokens_details: Some(llmleaf_client::PromptTokensDetails {
                cached_tokens: Some(4),
            }),
            cache_creation_tokens: Some(3),
        };
        let u = map_usage(sdk);
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.cached_tokens, 4);
        assert_eq!(u.cache_creation_tokens, 3);
        assert_eq!(u.cost_usd, Some(0.002));
    }

    #[test]
    fn collected_from_response_folds_message() {
        let resp = llmleaf_client::ChatResponse {
            id: "r".into(),
            object: "chat.completion".into(),
            created: 1,
            model: "m".into(),
            choices: vec![llmleaf_client::Choice {
                index: 0,
                message: llmleaf_client::ChatMessage::assistant("hello"),
                finish_reason: Some(llmleaf_client::FinishReason::Stop),
            }],
            usage: None,
        };
        let turn = collected_from_response(resp);
        assert_eq!(turn.content, "hello");
        assert_eq!(turn.finish_reason, Some(FinishReason::Stop));
    }
}
