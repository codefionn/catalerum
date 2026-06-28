//! OpenRouter / OpenAI wire types (SOUL §7).
//!
//! [`catalerum_core::ChatRequest`] is the provider-agnostic request the rest of
//! catalerum builds. On the wire we layer a few OpenRouter-specific fields on
//! top of it ([`stream`](WireRequest::stream) and the `provider` / `models`
//! routing knobs) without polluting the core type. The response side models the
//! streamed chat-completion **chunk** shape we parse out of the SSE feed.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use catalerum_core::llm::{ChatRequest, ToolChoice, ToolSpec};
use catalerum_core::model::MessageRole;

/// OpenRouter `provider` routing block (SOUL §7).
///
/// llmleaf is configured `kind = "openrouter"` and forwards these knobs to
/// OpenRouter so a turn can pin a provider order and toggle fallbacks.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRouting {
    /// Ordered provider preference (`["openai", "anthropic", …]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<String>,
    /// Whether to allow falling back to providers outside `order`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_fallbacks: Option<bool>,
    /// Require that the chosen provider supports all requested parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_parameters: Option<bool>,
}

impl ProviderRouting {
    /// True when no routing preference is set (so it can be omitted entirely).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty() && self.allow_fallbacks.is_none() && self.require_parameters.is_none()
    }
}

/// A function/tool advertised to the model, in the OpenAI `tools[]` envelope.
#[derive(Clone, Debug, Serialize)]
pub struct WireTool<'a> {
    /// Always `"function"` for the function-calling tool kind.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: &'a ToolSpec,
}

impl<'a> WireTool<'a> {
    fn function(spec: &'a ToolSpec) -> Self {
        Self {
            kind: "function",
            function: spec,
        }
    }
}

/// `tool_choice` serialized to the OpenAI wire shape (a bare string or a
/// `{type:"function", function:{name}}` object).
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum WireToolChoice {
    Simple(&'static str),
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        function: WireToolChoiceFunction,
    },
}

#[derive(Clone, Debug, Serialize)]
struct WireToolChoiceFunction {
    name: String,
}

impl From<&ToolChoice> for WireToolChoice {
    fn from(c: &ToolChoice) -> Self {
        match c {
            ToolChoice::Auto => WireToolChoice::Simple("auto"),
            ToolChoice::None => WireToolChoice::Simple("none"),
            ToolChoice::Required => WireToolChoice::Simple("required"),
            ToolChoice::Function { name } => WireToolChoice::Function {
                kind: "function",
                function: WireToolChoiceFunction { name: name.clone() },
            },
        }
    }
}

/// A chat message in the OpenAI wire shape. Borrows from a
/// [`catalerum_core::llm::ChatMessage`] so we serialize without cloning bodies.
#[derive(Clone, Debug, Serialize)]
pub struct WireMessage<'a> {
    pub role: MessageRole,
    pub content: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
}

/// A tool call on an assistant message in the OpenAI wire shape.
#[derive(Clone, Debug, Serialize)]
pub struct WireToolCall<'a> {
    pub id: &'a str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: WireFunctionCall<'a>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WireFunctionCall<'a> {
    pub name: &'a str,
    pub arguments: &'a str,
}

/// The full request body POSTed to `{base_url}/chat/completions`.
///
/// Wraps a borrowed [`ChatRequest`] and adds OpenRouter routing fields plus the
/// `stream` flag. Build it with [`WireRequest::new`] / [`WireRequest::streaming`].
#[derive(Clone, Debug, Serialize)]
pub struct WireRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<WireMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<WireToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Request a server-sent-events token stream.
    pub stream: bool,
    /// OpenRouter `provider { order, allow_fallbacks, … }` routing.
    #[serde(skip_serializing_if = "ProviderRouting::is_empty")]
    pub provider: ProviderRouting,
    /// OpenRouter `models` fallback list.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Remaining OpenRouter passthrough fields carried in `ChatRequest::extra`
    /// (`route`, `transforms`, …). `provider`/`models` are lifted into their own
    /// typed fields in [`build`](Self::build) and removed from here, so a caller
    /// who stashes routing in `extra` doesn't get it serialized twice.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Json>,
}

impl<'a> WireRequest<'a> {
    /// Build a wire request from a core [`ChatRequest`].
    ///
    /// `provider` / `models` are pulled out of `req.extra` if present there
    /// (so callers can stuff routing into `extra`), otherwise left empty for the
    /// caller to set via [`with_provider`](Self::with_provider) /
    /// [`with_models`](Self::with_models).
    fn build(req: &'a ChatRequest, stream: bool) -> Self {
        // Lift `provider` / `models` out of the opaque passthrough if the caller
        // stashed them there, **removing** them so they don't also flatten (which
        // would emit a duplicate `provider`/`models` key on the wire); everything
        // else stays in `extra` and flattens.
        let mut extra = req.extra.clone();
        let provider = extra
            .remove("provider")
            .and_then(|v| serde_json::from_value::<ProviderRouting>(v).ok())
            .unwrap_or_default();
        let models = extra
            .remove("models")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();

        Self {
            model: &req.model,
            messages: req.messages.iter().map(WireMessage::from_core).collect(),
            tools: req.tools.iter().map(WireTool::function).collect(),
            tool_choice: req.tool_choice.as_ref().map(WireToolChoice::from),
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            stream,
            provider,
            models,
            extra,
        }
    }

    /// A non-streaming (`stream: false`) request body.
    #[must_use]
    pub fn new(req: &'a ChatRequest) -> Self {
        Self::build(req, false)
    }

    /// A streaming (`stream: true`) request body.
    #[must_use]
    pub fn streaming(req: &'a ChatRequest) -> Self {
        Self::build(req, true)
    }

    /// Override the OpenRouter `provider` routing block.
    #[must_use]
    pub fn with_provider(mut self, provider: ProviderRouting) -> Self {
        self.provider = provider;
        self
    }

    /// Override the OpenRouter `models` fallback list.
    #[must_use]
    pub fn with_models(mut self, models: Vec<String>) -> Self {
        self.models = models;
        self
    }
}

impl<'a> WireMessage<'a> {
    fn from_core(m: &'a catalerum_core::llm::ChatMessage) -> Self {
        Self {
            role: m.role,
            content: &m.content,
            tool_calls: m
                .tool_calls
                .iter()
                .map(|tc| WireToolCall {
                    id: &tc.id,
                    kind: "function",
                    function: WireFunctionCall {
                        name: &tc.name,
                        arguments: &tc.arguments,
                    },
                })
                .collect(),
            tool_call_id: m.tool_call_id.as_deref(),
            name: m.name.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Response (streamed chunk) shapes
// ---------------------------------------------------------------------------

/// One streamed `chat.completion.chunk` (the object after each SSE `data:`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
    /// Some providers surface a top-level error object inside the stream.
    #[serde(default)]
    pub error: Option<WireError>,
}

/// One choice within a streamed chunk.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChunkChoice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The incremental `delta` payload on a streamed choice.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
}

/// A streamed fragment of one tool call.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

/// The `function` part of a streamed tool-call fragment.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

/// Usage accounting reported (usually on the final chunk).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// An error object embedded in a stream chunk / error response.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct WireError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub code: Option<Json>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::llm::ChatMessage;

    #[test]
    fn serializes_basic_request() {
        let req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        let wire = WireRequest::streaming(&req);
        let v = serde_json::to_value(&wire).unwrap();
        assert_eq!(v["model"], "m");
        assert_eq!(v["stream"], true);
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hi");
        // Empty tools / provider / models are omitted.
        assert!(v.get("tools").is_none());
        assert!(v.get("provider").is_none());
        assert!(v.get("models").is_none());
    }

    #[test]
    fn lifts_provider_and_models_from_extra() {
        let mut req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        req.extra.insert(
            "provider".into(),
            serde_json::json!({ "order": ["openai"], "allow_fallbacks": false }),
        );
        req.extra
            .insert("models".into(), serde_json::json!(["a", "b"]));
        let wire = WireRequest::new(&req);
        let v = serde_json::to_value(&wire).unwrap();
        assert_eq!(v["provider"]["order"][0], "openai");
        assert_eq!(v["provider"]["allow_fallbacks"], false);
        assert_eq!(v["models"][1], "b");
    }

    #[test]
    fn lifted_provider_and_models_are_not_serialized_twice() {
        // Regression: `build` lifts `provider`/`models` from `extra` into typed
        // fields; if it left them in the flattened `extra` too, the wire body would
        // carry a duplicate key. Check the serialized *string* (a `Value` map would
        // silently dedup and hide the bug), and confirm other `extra` keys survive.
        let mut req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        req.extra.insert(
            "provider".into(),
            serde_json::json!({ "order": ["openai"] }),
        );
        req.extra
            .insert("models".into(), serde_json::json!(["a", "b"]));
        req.extra
            .insert("route".into(), serde_json::json!("fallback"));

        let s = serde_json::to_string(&WireRequest::new(&req)).unwrap();
        assert_eq!(
            s.matches("\"provider\"").count(),
            1,
            "no duplicate provider: {s}"
        );
        assert_eq!(
            s.matches("\"models\"").count(),
            1,
            "no duplicate models: {s}"
        );
        // A non-lifted passthrough key still flattens through.
        assert!(
            s.contains("\"route\":\"fallback\""),
            "extra passthrough kept: {s}"
        );
        // And it still parses back to a single, correct value.
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["provider"]["order"][0], "openai");
        assert_eq!(v["models"][1], "b");
    }

    #[test]
    fn tool_choice_shapes() {
        let mut req = ChatRequest::new("m", vec![ChatMessage::user("hi")]);
        req.tool_choice = Some(ToolChoice::Auto);
        let v = serde_json::to_value(WireRequest::new(&req)).unwrap();
        assert_eq!(v["tool_choice"], "auto");

        req.tool_choice = Some(ToolChoice::Function { name: "f".into() });
        let v = serde_json::to_value(WireRequest::new(&req)).unwrap();
        assert_eq!(v["tool_choice"]["type"], "function");
        assert_eq!(v["tool_choice"]["function"]["name"], "f");
    }
}
