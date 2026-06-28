//! Automatic context compaction for the agent loop (SOUL §7).
//!
//! A long agent run — many tool rounds, large tool results — grows its running
//! message history until it overflows the model's context window and the run
//! dies mid-task. Compaction folds the *older* part of the history into a dense
//! summary (one extra model turn, no tools) and continues with
//! `system prefix + summary + recent tail`, so the loop keeps going instead.
//!
//! The loop checks before every model turn ([`should_compact`]): when the
//! projected prompt exceeds [`CompactionConfig::trigger_ratio`] of the context
//! window, [`compact`] rewrites the history in place. The trigger uses the best
//! signal available — the provider-reported token usage of the previous turn
//! when there was one, otherwise (and additionally) a chars/4 estimate.
//!
//! Fail-open: a failed or empty summarization leaves the history untouched and
//! the loop proceeds — the turn may still fit, and if it doesn't the provider
//! error surfaces normally. Compaction must never be the thing that kills a run.
//!
//! Chat threads get a second, *persistent* layer on top of this (a rolling
//! summary stored on the conversation, `catalerum-api`); this module is the
//! shared in-run layer every loop consumer — web chat, channel chat, `LlmAgent`
//! automation actions, skills, subagents — inherits through
//! [`AgentConfig`](crate::AgentConfig).

use tokio_util::sync::CancellationToken;
use tracing::warn;

use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::model::MessageRole;
use catalerum_core::stream::Usage;

use crate::agent::TurnStreamer;

/// Assumed context window (tokens) when the caller couldn't resolve the model's
/// real one from the gateway catalog. Most current chat models offer ≥128k; a
/// smaller model overflows before this default triggers, which is no worse than
/// having no compaction at all.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// Marker line opening the summary message that replaces folded history, so the
/// model (and a human reading a transcript) can tell it apart from a real user
/// turn. Shared with the persistent chat-thread summary in `catalerum-api`.
pub const COMPACTION_SUMMARY_PREFIX: &str =
    "[Context compacted — the earlier conversation was folded into this summary]";

/// Rough chars-per-token for estimation (English prose + JSON both land near 4).
const CHARS_PER_TOKEN: usize = 4;

/// Flat per-message token overhead (role/name/framing).
const MESSAGE_OVERHEAD_TOKENS: u32 = 4;

/// Output budget for the summarization turn.
const SUMMARY_MAX_TOKENS: u32 = 2_048;

/// System prompt for the summarization turn (SOUL §7). Kept public so the
/// persistent chat-thread compactor in `catalerum-api` uses the identical
/// instructions.
pub const COMPACTION_SYSTEM_PROMPT: &str = "You are compacting an AI agent's working \
conversation so it can continue with less context. Write a dense summary that preserves \
everything needed to continue seamlessly: the original task or request and its constraints; \
key facts, decisions, and conclusions so far; important tool calls and what they returned \
(names, ids, paths, keys, values); the current state of the work and what remains to be done; \
and any explicit user preferences or corrections. Prefer concrete identifiers over vague \
references. Output only the summary — no preamble, commentary, or advice.";

/// Tunables for in-run auto-compaction, carried on
/// [`AgentConfig`](crate::AgentConfig). The defaults are on for every loop;
/// callers that can resolve the model's real context window (the gateway
/// catalog's `context_length`) should set [`context_window`](Self::context_window).
#[derive(Clone, Debug)]
pub struct CompactionConfig {
    /// Master switch. Default `true` — every agent loop compacts rather than
    /// dies at the window.
    pub enabled: bool,
    /// The model's context window in tokens, when the caller knows it (gateway
    /// catalog `context_length`). `None` → [`DEFAULT_CONTEXT_WINDOW`].
    pub context_window: Option<u32>,
    /// Fraction of the window at which compaction triggers (leaving headroom
    /// for the next turn's output + tool schemas). Default `0.8`.
    pub trigger_ratio: f64,
    /// How many trailing messages are kept verbatim (the working set the model
    /// still needs at full fidelity). The actual kept tail may be slightly
    /// smaller: it is aligned forward so it never opens on an orphaned tool
    /// result. Default `12`.
    pub keep_recent: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            context_window: None,
            trigger_ratio: 0.8,
            keep_recent: 12,
        }
    }
}

impl CompactionConfig {
    /// The token budget above which compaction triggers.
    #[must_use]
    pub fn budget_tokens(&self) -> u32 {
        let window = self.context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW);
        (f64::from(window) * self.trigger_ratio.clamp(0.1, 1.0)) as u32
    }
}

/// Rough token estimate for one message: content + reasoning + tool-call
/// payloads at [`CHARS_PER_TOKEN`], plus a flat framing overhead.
#[must_use]
pub fn estimate_message_tokens(m: &ChatMessage) -> u32 {
    let mut chars = m.content.len() + m.reasoning.as_deref().map_or(0, str::len);
    for tc in &m.tool_calls {
        chars += tc.name.len() + tc.arguments.len() + 16;
    }
    (chars / CHARS_PER_TOKEN) as u32 + MESSAGE_OVERHEAD_TOKENS
}

/// Rough token estimate for a whole message history (saturating).
#[must_use]
pub fn estimate_tokens(messages: &[ChatMessage]) -> u32 {
    messages
        .iter()
        .map(estimate_message_tokens)
        .fold(0u32, u32::saturating_add)
}

/// Whether the next model turn is projected to exceed the compaction budget.
///
/// `last_usage` is the previous turn's provider-reported usage, when one exists
/// — its `prompt + completion` tokens are (almost exactly) the next prompt's
/// size, missing only what was appended since (tool results / queued user
/// input), which the chars/4 estimate covers. The projection takes the max of
/// both signals so neither a poor estimate nor missing usage under-triggers.
#[must_use]
pub(crate) fn should_compact(
    messages: &[ChatMessage],
    last_usage: Option<&Usage>,
    config: &CompactionConfig,
) -> bool {
    if !config.enabled {
        return false;
    }
    let reported = last_usage.map_or(0, |u| u.prompt_tokens.saturating_add(u.completion_tokens));
    let projected = estimate_tokens(messages).max(reported);
    projected > config.budget_tokens()
}

/// The split of a history for compaction: `messages[..prefix_end]` is the
/// leading system prefix (kept verbatim), `messages[prefix_end..tail_start]` is
/// the head to fold into a summary, `messages[tail_start..]` is the tail kept
/// verbatim.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CompactionSplit {
    pub prefix_end: usize,
    pub tail_start: usize,
}

/// Choose the fold boundary: keep the leading system prefix and the last
/// `keep_recent` messages, aligned **forward past tool results** so the kept
/// tail never opens with a tool result whose originating assistant tool-call
/// was folded away (which model APIs reject). `None` when there is nothing
/// worth folding (fewer than 2 head messages — folding a lone message, e.g. a
/// previous summary, cannot shrink anything).
#[must_use]
pub(crate) fn split_for_compaction(
    messages: &[ChatMessage],
    keep_recent: usize,
) -> Option<CompactionSplit> {
    let prefix_end = messages
        .iter()
        .position(|m| m.role != MessageRole::System)
        .unwrap_or(messages.len());
    let mut tail_start = messages.len().saturating_sub(keep_recent).max(prefix_end);
    while tail_start < messages.len() && messages[tail_start].role == MessageRole::Tool {
        tail_start += 1;
    }
    // Require at least 2 head messages and a non-empty tail: the current
    // working turn must survive verbatim, and a 1-message head is a no-op fold.
    if tail_start.saturating_sub(prefix_end) < 2 || tail_start >= messages.len() {
        return None;
    }
    Some(CompactionSplit {
        prefix_end,
        tail_start,
    })
}

/// Render messages into the plain-text transcript the summarizer reads. Tool
/// calls and results are included (they often carry the ids/paths the summary
/// must preserve); reasoning is not (ephemeral, often enormous).
#[must_use]
pub fn render_transcript(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool result",
        };
        out.push_str("## ");
        out.push_str(role);
        if let Some(name) = m.name.as_deref() {
            out.push_str(" (");
            out.push_str(name);
            out.push(')');
        }
        out.push('\n');
        if !m.content.is_empty() {
            out.push_str(&m.content);
            out.push('\n');
        }
        for tc in &m.tool_calls {
            out.push_str(&format!("→ tool call `{}` {}\n", tc.name, tc.arguments));
        }
        out.push('\n');
    }
    out
}

/// Cap a transcript render to `max_chars`, keeping the **end** (the most recent
/// part matters most when the head itself is too big to summarize whole). Cuts
/// on a char boundary and marks the cut.
#[must_use]
pub fn cap_transcript(transcript: String, max_chars: usize) -> String {
    if transcript.len() <= max_chars {
        return transcript;
    }
    let mut start = transcript.len() - max_chars;
    while start < transcript.len() && !transcript.is_char_boundary(start) {
        start += 1;
    }
    format!("[…older transcript truncated…]\n{}", &transcript[start..])
}

/// Build the one-shot, tool-less summarization request over a rendered
/// transcript (optionally continuing a `prior_summary`). Shared by the in-run
/// compactor below and the persistent chat-thread compactor in `catalerum-api`.
#[must_use]
pub fn summarize_request(
    model: &str,
    prior_summary: Option<&str>,
    transcript: String,
) -> ChatRequest {
    let body = match prior_summary {
        Some(prior) if !prior.trim().is_empty() => format!(
            "A summary of the conversation so far already exists:\n\n{prior}\n\n\
             Fold it together with the transcript below into ONE updated summary.\n\n\
             Transcript:\n\n{transcript}"
        ),
        _ => format!("Summarize this transcript:\n\n{transcript}"),
    };
    let mut req = ChatRequest::new(
        model,
        vec![
            ChatMessage::system(COMPACTION_SYSTEM_PROMPT),
            ChatMessage::user(body),
        ],
    );
    req.max_tokens = Some(SUMMARY_MAX_TOKENS);
    req
}

/// The message that stands in for folded history in the compacted run.
#[must_use]
pub fn summary_message(summary: &str) -> ChatMessage {
    ChatMessage::user(format!("{COMPACTION_SUMMARY_PREFIX}\n\n{summary}"))
}

/// The outcome of one in-run compaction: how many messages were folded and the
/// summary that replaced them, plus the summarization turn's own usage (it is a
/// paid model turn — the loop folds it into the run's accounting so grant
/// `cost_limit` bookkeeping stays honest).
pub(crate) struct Compacted {
    pub folded: usize,
    pub summary: String,
    pub usage: Option<Usage>,
}

/// Fold the compactable head of `messages` into a summary via one extra
/// (tool-less) model turn, rewriting `messages` in place to
/// `system prefix + summary + tail`.
///
/// **Fail-open**: any failure — nothing to fold, the summarize turn erroring,
/// an empty summary, a cancel mid-summarize — leaves `messages` untouched and
/// returns `None`; the loop proceeds uncompacted and the original error surface
/// (or the cancel path) takes over.
pub(crate) async fn compact<S>(
    streamer: &S,
    model: &str,
    messages: &mut Vec<ChatMessage>,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Option<Compacted>
where
    S: TurnStreamer + ?Sized,
{
    let split = split_for_compaction(messages, config.keep_recent)?;
    let head = &messages[split.prefix_end..split.tail_start];

    // Bound the summarize prompt itself: it must fit the same window. Reserve
    // half the budget for the transcript (the rest is headroom for the system
    // prompt, prior formatting, and the summary output).
    let max_chars = (config.budget_tokens() as usize / 2).saturating_mul(CHARS_PER_TOKEN);
    let transcript = cap_transcript(render_transcript(head), max_chars);
    let request = summarize_request(model, None, transcript);

    let turn = match crate::agent::summarize_turn(streamer, request, cancel).await {
        Ok(turn) => turn,
        Err(e) => {
            warn!(error = %e, "context compaction summarize turn failed; continuing uncompacted");
            return None;
        }
    };
    let summary = turn.content.trim().to_string();
    if summary.is_empty() {
        warn!("context compaction produced an empty summary; continuing uncompacted");
        return None;
    }

    let folded = split.tail_start - split.prefix_end;
    messages.splice(
        split.prefix_end..split.tail_start,
        [summary_message(&summary)],
    );
    Some(Compacted {
        folded,
        summary,
        usage: turn.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::model::ToolCall;

    fn msg(role: MessageRole, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            images: Vec::new(),
            media: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            reasoning: None,
            reasoning_details: Vec::new(),
        }
    }

    #[test]
    fn estimate_counts_content_tool_calls_and_overhead() {
        let mut m = msg(MessageRole::Assistant, &"x".repeat(400));
        assert_eq!(estimate_message_tokens(&m), 100 + MESSAGE_OVERHEAD_TOKENS);
        m.tool_calls.push(ToolCall {
            id: "c1".into(),
            name: "abcd".into(),
            arguments: "x".repeat(380),
        });
        // +400 chars of call payload (name 4 + args 380 + 16) → +100 tokens.
        assert_eq!(estimate_message_tokens(&m), 200 + MESSAGE_OVERHEAD_TOKENS);
        assert_eq!(
            estimate_tokens(&[msg(MessageRole::User, &"y".repeat(40))]),
            10 + MESSAGE_OVERHEAD_TOKENS
        );
    }

    #[test]
    fn trigger_uses_max_of_estimate_and_reported_usage() {
        let config = CompactionConfig {
            context_window: Some(1_000),
            ..CompactionConfig::default()
        };
        // Budget = 800 tokens. A tiny history alone doesn't trigger…
        let small = vec![msg(MessageRole::User, "hi")];
        assert!(!should_compact(&small, None, &config));
        // …but a previous turn that *reported* a near-window prompt does (the
        // estimate can undercount, e.g. image tokens or provider framing).
        let big_usage = Usage {
            prompt_tokens: 900,
            completion_tokens: 50,
            total_tokens: 950,
            ..Usage::default()
        };
        assert!(should_compact(&small, Some(&big_usage), &config));
        // A big char estimate triggers even without usage.
        let big = vec![msg(MessageRole::User, &"x".repeat(4_000))];
        assert!(should_compact(&big, None, &config));
        // Disabled → never.
        let off = CompactionConfig {
            enabled: false,
            ..config
        };
        assert!(!should_compact(&big, None, &off));
    }

    #[test]
    fn split_keeps_system_prefix_and_aligned_tail() {
        use MessageRole::{Assistant, System, Tool, User};
        let history = vec![
            msg(System, "sys1"),
            msg(System, "sys2"),
            msg(User, "q1"),
            msg(Assistant, "a1"),
            msg(User, "q2"),
            msg(Assistant, "calls"),
            msg(Tool, "r1"),
            msg(Tool, "r2"),
            msg(Assistant, "a2"),
            msg(User, "q3"),
        ];
        // keep_recent = 5 → naive tail starts at index 5 (assistant) — already
        // safe, kept as-is. Head = [2..5).
        let s = split_for_compaction(&history, 5).unwrap();
        assert_eq!(s.prefix_end, 2);
        assert_eq!(s.tail_start, 5);

        // keep_recent = 4 → naive tail would open on a Tool result (index 6);
        // the boundary advances past both results to the assistant at 8.
        let s = split_for_compaction(&history, 4).unwrap();
        assert_eq!(s.tail_start, 8);

        // Nothing to fold: history is (almost) all tail.
        assert!(split_for_compaction(&history, 9).is_none());
        // A 1-message head (e.g. just a previous summary) is not worth folding.
        let short = vec![msg(System, "s"), msg(User, "u"), msg(Assistant, "a")];
        assert!(split_for_compaction(&short, 1).is_none());
        // All-system / empty histories never split.
        assert!(split_for_compaction(&[msg(System, "s")], 2).is_none());
        assert!(split_for_compaction(&[], 2).is_none());
    }

    #[test]
    fn transcript_renders_roles_tools_and_caps_from_the_front() {
        let history = vec![
            msg(MessageRole::User, "find the report"),
            ChatMessage {
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    name: "search".into(),
                    arguments: "{\"q\":\"report\"}".into(),
                }],
                ..msg(MessageRole::Assistant, "")
            },
            ChatMessage {
                name: Some("search".into()),
                tool_call_id: Some("c1".into()),
                ..msg(MessageRole::Tool, "{\"hits\":[\"a.pdf\"]}")
            },
        ];
        let t = render_transcript(&history);
        assert!(t.contains("## user\nfind the report"));
        assert!(t.contains("→ tool call `search` {\"q\":\"report\"}"));
        assert!(t.contains("## tool result (search)\n{\"hits\":[\"a.pdf\"]}"));

        let capped = cap_transcript(t.clone(), 30);
        assert!(capped.starts_with("[…older transcript truncated…]"));
        assert!(capped.ends_with(&t[t.len() - 30..]));
        // Under the cap → unchanged.
        assert_eq!(cap_transcript("short".into(), 30), "short");
    }

    #[test]
    fn summarize_request_is_toolless_and_folds_a_prior_summary() {
        let req = summarize_request("m", None, "T".into());
        assert!(req.tools.is_empty());
        assert_eq!(req.max_tokens, Some(SUMMARY_MAX_TOKENS));
        assert_eq!(req.messages.len(), 2);
        assert!(req.messages[1]
            .content
            .contains("Summarize this transcript"));

        let req = summarize_request("m", Some("PRIOR"), "T".into());
        assert!(req.messages[1].content.contains("PRIOR"));
        assert!(req.messages[1].content.contains("ONE updated summary"));

        assert!(summary_message("S")
            .content
            .starts_with(COMPACTION_SUMMARY_PREFIX));
    }
}
