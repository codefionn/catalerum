//! Provider-agnostic LLM request/response shapes (SOUL §7).
//!
//! The wire format targets the OpenRouter superset (OpenAI-compatible). These
//! types are what the [`LlmClient`](crate::provider::LlmClient) trait consumes
//! and produces; `catalerum-llm` maps them to/from llmleaf's HTTP API and
//! parses the SSE stream into [`StreamEvent`](crate::stream::StreamEvent)s.

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::model::{MessageRole, ToolCall};
use crate::stream::ReasoningDetail;

/// Binary media attached to a model input turn.
///
/// llmleaf currently supports inline images on Catalerum's Responses path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaInput {
    Image { url: String },
}

/// One message in a chat request (OpenAI/OpenRouter shape, decoupled from the
/// persisted [`Message`](crate::model::Message)).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: MessageRole,
    #[serde(default)]
    pub content: String,
    /// Inline image content for a multimodal (vision) turn (SOUL §7/§9): each
    /// entry is an image URL — a `data:<mime>;base64,…` URI (how catalerum
    /// inlines an uploaded image) or a remote `http(s)` URL. Populated only for a
    /// user turn whose attachments include images **and** whose model advertises
    /// `image` input; empty otherwise (the file still rides as the text reference
    /// block). The `catalerum-llm` send path emits these as multimodal content
    /// parts alongside `content`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<String>,
    /// Ephemeral image inputs produced inside an agent run (for example by a
    /// binary-aware `read_file` tool). These are emitted as real multimodal
    /// content parts on the next model turn, never flattened into textual
    /// base64.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaInput>,
    /// Tool calls emitted by an assistant turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For a `tool` message, the id of the call it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Optional name (e.g. tool name) per the OpenAI shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Visible (open) reasoning text the assistant emitted, if any. Carried so an
    /// assistant turn produced by the agent loop can echo its reasoning back on
    /// the following (tool-result) request within the same turn (SOUL §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Structured reasoning blocks (open + signed/encrypted) to echo back
    /// verbatim across a tool-call round-trip. Empty for plain or replayed
    /// messages; see [`ReasoningDetail`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<ReasoningDetail>,
}

impl ChatMessage {
    /// A system message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(MessageRole::System, content)
    }

    /// A user message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::text(MessageRole::User, content)
    }

    /// An assistant message.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(MessageRole::Assistant, content)
    }

    fn text(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        }
    }
}

/// A tool advertised to the model in a request. The `parameters` is a JSON
/// Schema object describing the arguments (SOUL §7). Mirrors the OpenAI
/// `function` tool spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: Json,
}

/// How the model may use tools (OpenRouter `tool_choice`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    Auto,
    /// Model must not call a tool.
    None,
    /// Model must call at least one tool.
    Required,
    /// Force a specific tool by name.
    Function { name: String },
}

/// A chat completion request (OpenRouter superset, SOUL §7). Streaming is
/// requested via the trait method, not a field here.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// Model id (or routing alias understood by llmleaf/OpenRouter).
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Reasoning ("thinking") effort for reasoning-capable models:
    /// `"low" | "medium" | "high"`. `None` leaves it to the provider default
    /// (no reasoning requested). Surfaced back as
    /// [`StreamEvent::ReasoningDelta`](crate::stream::StreamEvent::ReasoningDelta).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// OpenRouter passthrough fields (`provider`, `models`, `route`,
    /// `transforms`, …) kept opaque so core need not track the full superset.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty", flatten)]
    pub extra: serde_json::Map<String, Json>,
}

impl ChatRequest {
    /// A minimal request for `model` with the given messages.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            tool_choice: None,
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            extra: serde_json::Map::new(),
        }
    }
}
