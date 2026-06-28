//! Streaming events for the LLM layer (SOUL §7).
//!
//! The LLM client streams an SSE response which `catalerum-llm` parses into a
//! sequence of [`StreamEvent`]s. Every turn ends with a guaranteed
//! [`StreamEvent::Done`] (the `message_done` contract).

use serde::{Deserialize, Serialize};

/// One incremental event in a streamed model turn (SOUL §7).
///
/// A typical turn is a run of [`TextDelta`](StreamEvent::TextDelta) and/or
/// [`ToolCallDelta`](StreamEvent::ToolCallDelta) events, terminated by exactly
/// one [`Done`](StreamEvent::Done) — or an [`Error`](StreamEvent::Error) if the
/// stream fails.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A chunk of assistant text content.
    TextDelta {
        /// The text fragment to append.
        text: String,
    },

    /// A chunk of the model's *reasoning* (thinking) for this turn — distinct
    /// from the user-visible [`TextDelta`](StreamEvent::TextDelta) answer. Surfaced
    /// so a UI can show a "thinking" trace live (SOUL §7).
    ReasoningDelta {
        /// The visible (open) reasoning text fragment to append.
        text: String,
        /// Structured reasoning blocks (open + signed/encrypted) carried so the
        /// agent loop can echo them back **verbatim** within the same turn — some
        /// providers reject an altered/dropped signed block before a tool call.
        /// In-process only: never serialized to the client or the bus (the signed
        /// blobs are opaque and needn't leave the pod).
        #[serde(default, skip)]
        details: Vec<ReasoningDetail>,
    },

    /// A fragment of a tool call being assembled. Deltas with the same `index`
    /// accumulate into one [`ToolCall`](crate::model::ToolCall); any field may
    /// arrive incrementally (the OpenRouter streaming shape).
    ToolCallDelta {
        /// Position of this tool call within the turn's `tool_calls` array.
        index: u32,
        /// The call id, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        /// The function/tool name, once known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// A fragment of the JSON arguments string to append.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<String>,
    },

    /// A tool call has started executing — dispatched by the agent loop after the
    /// model requested it. Emitted once per call, *before* the tool runs, so a UI
    /// can show a live "running" card. Synthesized by the agent loop from the
    /// assembled call; the model stream itself never produces this (SOUL §7/§12).
    ToolCallStarted {
        /// The tool call id (correlates the later [`ToolResult`](StreamEvent::ToolResult)
        /// and the model's [`ToolCallDelta`](StreamEvent::ToolCallDelta)).
        id: String,
        /// The tool/function name dispatched.
        name: String,
        /// The fully-assembled JSON arguments string.
        arguments: String,
    },

    /// A tool call finished executing. Carries the result (or error payload) and
    /// timing so a UI can resolve the live card to success/failure. Synthesized by
    /// the agent loop from the dispatch outcome (SOUL §7/§12).
    ToolResult {
        /// The tool call id this answers (matches a prior
        /// [`ToolCallStarted`](StreamEvent::ToolCallStarted)).
        id: String,
        /// The tool/function name (denormalized for convenience).
        name: String,
        /// The tool's result string (raw JSON / text), byte-capped for the wire
        /// when large — see [`truncated`](Self::ToolResult::truncated).
        result: String,
        /// Whether the call failed (the `result` holds the error payload).
        is_error: bool,
        /// Wall-clock execution time in milliseconds, when measured.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        /// Whether `result` was truncated for the wire (full text is persisted).
        #[serde(default, skip_serializing_if = "is_false")]
        truncated: bool,
    },

    /// The agent loop **auto-compacted** its conversation context (SOUL §7):
    /// older transcript messages were folded into a rolling summary so the run
    /// can keep going instead of overflowing the model's context window.
    /// Synthesized by the agent loop between model turns — the model stream
    /// itself never produces this. Informational: a UI may show a subtle
    /// "context compacted" marker; clients that don't know the tag ignore it.
    Compacted {
        /// How many transcript messages were folded into the summary.
        folded: u32,
        /// The summary text that now stands in for them.
        summary: String,
    },

    /// The turn finished. Carries the terminal metadata.
    Done {
        /// Provider finish reason (`stop`, `tool_calls`, `length`, …), if given.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
        /// Token accounting for the turn, if reported.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },

    /// The stream errored mid-turn.
    Error {
        /// Human-readable message.
        message: String,
    },
}

/// Why a streamed turn ended (SOUL §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// Natural stop / stop sequence.
    Stop,
    /// The model wants to call tools — run them and loop (SOUL §7 agent loop).
    ToolCalls,
    /// Hit the max-tokens limit.
    Length,
    /// Provider content filter.
    ContentFilter,
    /// Any other / provider-specific reason.
    Other,
}

/// Token accounting for a completion (OpenAI/OpenRouter shape), plus llmleaf's
/// cost/cache extensions. Not `Eq` because [`cost_usd`](Self::cost_usd) is an
/// `f64`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// llmleaf's USD cost estimate for the turn, when the model has a known
    /// price. Summed across turns by the agent loop; feeds the §19 `cost_limit`
    /// capability accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Prompt tokens served from the provider's prompt cache this turn — a cache
    /// *read* (hit). `0` when the upstream reported no caching.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cached_tokens: u32,
    /// Prompt tokens written to the provider's prompt cache this turn — a cache
    /// *write* (creation). `0` when there were none / the provider doesn't report.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_creation_tokens: u32,
}

/// Serde helper: skip a zero count so the wire shape is unchanged when a provider
/// reports no cache activity (keeps the `done`/usage frame backward-compatible).
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Serde helper: skip a `false` flag so a non-truncated tool result keeps a
/// minimal wire shape (and older decoders default it to `false`).
fn is_false(b: &bool) -> bool {
    !*b
}

/// One structured reasoning ("thinking") block emitted by the model, mirroring
/// the OpenRouter `reasoning_details` shape (SOUL §7).
///
/// A block is either *open* (visible [`text`](Self::text)/`summary`) or *hidden*
/// (redacted/encrypted, carrying opaque `data` + a `signature`). The agent loop
/// echoes these back **verbatim** on the next request within a turn so a
/// reasoning model's signed chain survives a tool-call round-trip — providers
/// reject an altered or dropped block.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningDetail {
    /// Block kind tag (e.g. `reasoning.text`, `reasoning.encrypted`).
    #[serde(rename = "type")]
    pub kind: String,
    /// Visible reasoning text, if this is an open text block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Visible summary, if the provider summarized rather than streamed full text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Opaque payload for a hidden/encrypted block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Signature authenticating a hidden block (echo verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Provider block id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Provider-specific format tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Position of this block within the turn's reasoning, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
}

impl ReasoningDetail {
    /// Whether this block is hidden (redacted/encrypted) rather than open text.
    #[must_use]
    pub fn is_hidden(&self) -> bool {
        self.kind == "reasoning.encrypted" || (self.data.is_some() && self.text.is_none())
    }

    /// The visible reasoning text of an open block — its `text`, falling back to
    /// its `summary`. `None` for a hidden block.
    #[must_use]
    pub fn open_text(&self) -> Option<&str> {
        self.text.as_deref().or(self.summary.as_deref())
    }
}
