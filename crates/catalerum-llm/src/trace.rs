//! OpenTelemetry/GenAI span enrichment for streamed LLM generations.

use futures::stream::{self, BoxStream, StreamExt};

use catalerum_core::error::Result;
use catalerum_core::llm::ChatRequest;
use catalerum_core::stream::StreamEvent;

use crate::client::ToolCallAssembler;

/// Tracing target routed exclusively to the vendor-neutral OTLP exporter.
pub const OTEL_LLM_TARGET: &str = "catalerum_otel_llm";
/// Tracing target routed exclusively to the Langfuse OTLP exporter.
pub const LANGFUSE_LLM_TARGET: &str = "catalerum_langfuse_llm";

/// LLM payload capture policy for one trace destination.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TraceContent {
    #[default]
    MetadataOnly,
    AllExceptSystemPrompts,
    Everything,
}

/// Per-destination LLM tracing configuration. Ordinary application spans are
/// configured by the binary; this controls generation attributes only.
#[derive(Clone, Copy, Debug, Default)]
pub struct LlmTraceConfig {
    pub otlp: Option<TraceContent>,
    pub langfuse: Option<TraceContent>,
}

impl LlmTraceConfig {
    #[must_use]
    pub fn is_enabled(self) -> bool {
        self.otlp.is_some() || self.langfuse.is_some()
    }
}

#[derive(Debug)]
struct GenerationSpan {
    span: tracing::Span,
    content: TraceContent,
}

impl GenerationSpan {
    fn record(&self, key: &'static str, value: impl tracing::field::Value) {
        self.span.record(key, value);
    }

    fn record_error(&self, message: &str) {
        self.record("otel.status_code", "ERROR");
        self.record("error.type", "llm_provider");
        self.record("error.message", message);
        self.record("langfuse.observation.level", "ERROR");
        self.record("langfuse.observation.status_message", message);
    }
}

/// A pair of generation spans, one per enabled destination. Separate tracing
/// targets let the binary route each span to exactly one exporter, which makes
/// their content policies independent.
#[derive(Debug, Default)]
pub(crate) struct GenerationSpans {
    spans: Vec<GenerationSpan>,
}

impl GenerationSpans {
    pub(crate) fn new(config: LlmTraceConfig, request: &ChatRequest, base_url: &str) -> Self {
        let mut spans = Vec::with_capacity(2);
        if let Some(content) = config.otlp {
            spans.push(new_otel_span(content, request, base_url));
        }
        if let Some(content) = config.langfuse {
            spans.push(new_langfuse_span(content, request, base_url));
        }
        Self { spans }
    }

    fn captures_content(&self) -> bool {
        self.spans
            .iter()
            .any(|span| span.content != TraceContent::MetadataOnly)
    }

    pub(crate) fn record_error(&self, message: &str) {
        for span in &self.spans {
            span.record_error(message);
        }
    }
}

fn new_otel_span(content: TraceContent, request: &ChatRequest, base_url: &str) -> GenerationSpan {
    let span = tracing::info_span!(
        target: "catalerum_otel_llm",
        "gen_ai.chat",
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "llmleaf",
        gen_ai.system = "llmleaf",
        gen_ai.request.model = %request.model,
        gen_ai.request.temperature = request.temperature,
        gen_ai.request.max_tokens = request.max_tokens,
        gen_ai.tool.count = request.tools.len(),
        server.address = %base_url,
        gen_ai.prompt = tracing::field::Empty,
        gen_ai.completion = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        langfuse.observation.type = "generation",
        langfuse.observation.model.name = %request.model,
        langfuse.observation.model.parameters = tracing::field::Empty,
        langfuse.observation.input = tracing::field::Empty,
        langfuse.observation.output = tracing::field::Empty,
        langfuse.observation.usage_details = tracing::field::Empty,
        langfuse.observation.cost_details = tracing::field::Empty,
        langfuse.observation.level = tracing::field::Empty,
        langfuse.observation.status_message = tracing::field::Empty,
    );
    initialize_span(span, content, request)
}

fn new_langfuse_span(
    content: TraceContent,
    request: &ChatRequest,
    base_url: &str,
) -> GenerationSpan {
    let span = tracing::info_span!(
        target: "catalerum_langfuse_llm",
        "gen_ai.chat",
        otel.kind = "client",
        otel.status_code = tracing::field::Empty,
        gen_ai.operation.name = "chat",
        gen_ai.provider.name = "llmleaf",
        gen_ai.system = "llmleaf",
        gen_ai.request.model = %request.model,
        gen_ai.request.temperature = request.temperature,
        gen_ai.request.max_tokens = request.max_tokens,
        gen_ai.tool.count = request.tools.len(),
        server.address = %base_url,
        gen_ai.prompt = tracing::field::Empty,
        gen_ai.completion = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        langfuse.observation.type = "generation",
        langfuse.observation.model.name = %request.model,
        langfuse.observation.model.parameters = tracing::field::Empty,
        langfuse.observation.input = tracing::field::Empty,
        langfuse.observation.output = tracing::field::Empty,
        langfuse.observation.usage_details = tracing::field::Empty,
        langfuse.observation.cost_details = tracing::field::Empty,
        langfuse.observation.level = tracing::field::Empty,
        langfuse.observation.status_message = tracing::field::Empty,
    );
    initialize_span(span, content, request)
}

fn initialize_span(
    span: tracing::Span,
    content: TraceContent,
    request: &ChatRequest,
) -> GenerationSpan {
    let generation = GenerationSpan { span, content };
    let parameters = serde_json::json!({
        "temperature": request.temperature,
        "max_tokens": request.max_tokens,
        "reasoning_effort": request.reasoning_effort,
        "tool_choice": request.tool_choice,
    })
    .to_string();
    generation.record("langfuse.observation.model.parameters", parameters.as_str());

    if let Some(input) = serialized_input(request, content) {
        generation.record("gen_ai.prompt", input.as_str());
        generation.record("langfuse.observation.input", input.as_str());
    }
    generation
}

fn serialized_input(request: &ChatRequest, content: TraceContent) -> Option<String> {
    if content == TraceContent::MetadataOnly {
        return None;
    }
    let mut input = serde_json::to_value(request).ok()?;
    if content == TraceContent::AllExceptSystemPrompts {
        if let Some(messages) = input.get_mut("messages").and_then(|v| v.as_array_mut()) {
            messages
                .retain(|message| message.get("role").and_then(|v| v.as_str()) != Some("system"));
        }
    }
    serde_json::to_string(&input).ok()
}

struct TraceState {
    stream: BoxStream<'static, Result<StreamEvent>>,
    spans: GenerationSpans,
    content: String,
    reasoning: String,
    tools: ToolCallAssembler,
    errored: bool,
    completed: bool,
    finished: bool,
}

impl TraceState {
    fn observe(&mut self, event: &Result<StreamEvent>) {
        match event {
            Ok(StreamEvent::TextDelta { text }) => self.content.push_str(text),
            Ok(StreamEvent::ReasoningDelta { text, .. }) => self.reasoning.push_str(text),
            Ok(StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            }) => self
                .tools
                .push(*index, id.clone(), name.clone(), arguments.clone()),
            Ok(StreamEvent::Done {
                finish_reason,
                usage,
            }) => {
                if let Some(reason) = finish_reason {
                    let reason = serde_json::to_string(reason).unwrap_or_default();
                    for span in &self.spans.spans {
                        span.record("gen_ai.response.finish_reasons", reason.as_str());
                    }
                }
                if let Some(usage) = usage {
                    let details = serde_json::json!({
                        "input": usage.prompt_tokens,
                        "output": usage.completion_tokens,
                        "total": usage.total_tokens,
                        "cached": usage.cached_tokens,
                        "cache_creation": usage.cache_creation_tokens,
                    })
                    .to_string();
                    let costs = usage
                        .cost_usd
                        .map(|total| serde_json::json!({ "total": total }).to_string());
                    for span in &self.spans.spans {
                        span.record("gen_ai.usage.input_tokens", usage.prompt_tokens);
                        span.record("gen_ai.usage.output_tokens", usage.completion_tokens);
                        span.record("langfuse.observation.usage_details", details.as_str());
                        if let Some(costs) = &costs {
                            span.record("langfuse.observation.cost_details", costs.as_str());
                        }
                    }
                }
                self.completed = true;
                self.finish();
            }
            Ok(StreamEvent::Error { message }) => {
                self.errored = true;
                self.spans.record_error(message);
            }
            Err(error) => {
                self.errored = true;
                self.spans.record_error(&error.to_string());
            }
            Ok(
                StreamEvent::ToolCallStarted { .. }
                | StreamEvent::ToolResult { .. }
                | StreamEvent::Compacted { .. },
            ) => {}
        }
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        let tools = std::mem::take(&mut self.tools).finish();
        let output = serde_json::json!({
            "content": self.content,
            "reasoning": self.reasoning,
            "tool_calls": tools,
        })
        .to_string();
        for span in &self.spans.spans {
            if span.content != TraceContent::MetadataOnly {
                span.record("gen_ai.completion", output.as_str());
                span.record("langfuse.observation.output", output.as_str());
            }
            if self.completed && !self.errored {
                span.record("otel.status_code", "OK");
            }
        }
    }
}

impl Drop for TraceState {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(crate) fn trace_stream(
    stream: BoxStream<'static, Result<StreamEvent>>,
    spans: GenerationSpans,
) -> BoxStream<'static, Result<StreamEvent>> {
    if spans.spans.is_empty() {
        return stream;
    }
    let capture = spans.captures_content();
    let state = TraceState {
        stream,
        spans,
        content: String::new(),
        reasoning: String::new(),
        tools: ToolCallAssembler::default(),
        errored: false,
        completed: false,
        finished: false,
    };
    stream::unfold(state, move |mut state| async move {
        let event = state.stream.next().await?;
        if capture {
            state.observe(&event);
        } else {
            // Metadata-only still records completion state, usage, and errors.
            match &event {
                Ok(StreamEvent::TextDelta { .. })
                | Ok(StreamEvent::ReasoningDelta { .. })
                | Ok(StreamEvent::ToolCallDelta { .. }) => {}
                _ => state.observe(&event),
            }
        }
        Some((event, state))
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use catalerum_core::llm::ChatMessage;

    use super::*;

    #[test]
    fn without_system_prompts_removes_every_system_message() {
        let request = ChatRequest::new(
            "model",
            vec![
                ChatMessage::system("secret system"),
                ChatMessage::user("hello"),
                ChatMessage::system("secret skill"),
            ],
        );
        let value: serde_json::Value = serde_json::from_str(
            &serialized_input(&request, TraceContent::AllExceptSystemPrompts).unwrap(),
        )
        .unwrap();
        let messages = value["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"], "hello");
    }

    #[test]
    fn metadata_only_serializes_no_payload() {
        let request = ChatRequest::new("model", vec![ChatMessage::system("secret")]);
        assert!(serialized_input(&request, TraceContent::MetadataOnly).is_none());
    }
}
