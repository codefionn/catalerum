//! Pure prompt and trigger helpers for the LLM-backed actions: agent/summarize
//! request building, JSON reply extraction, and firing-event lookups.

use catalerum_automation::{ActionOutcome, LlmAgent};
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_llm::AgentOutcome;
use serde_json::{json, Value};

use super::AGENT_TRIGGER_PROMPT;

/// Map an [`AgentOutcome`] to an `LlmAgent` step's [`ActionOutcome`]. An agent that
/// produced **neither text nor any tool call** accomplished nothing — that is a
/// `Failed` step, **surfaced in the run history**, not a silent empty success (e.g.
/// an LLM backend that returned an empty stream). Otherwise it succeeds, carrying
/// the final text + iteration / tool-call counts. Pure, so the rule is unit-testable
/// without a live LLM.
pub(super) fn agent_outcome_to_action(outcome: &AgentOutcome, data: Option<Value>) -> ActionOutcome {
    if outcome.content.trim().is_empty() && outcome.tool_invocations.is_empty() {
        return ActionOutcome::failed(
            "llm agent produced no output (empty response and no tool calls) — the model or \
             LLM backend may have returned nothing",
        );
    }
    let mut out = json!({
        "content": outcome.content,
        "iterations": outcome.iterations,
        "tool_calls": outcome.tool_invocations.len(),
    });
    // Surface the run's cost + truncation status so the run detail can show a cost
    // chip / "budget reached" badge (§19) rather than presenting a budget- or
    // iteration-capped partial reply as a clean finish.
    if let Some(cost) = outcome.usage.as_ref().and_then(|u| u.cost_usd) {
        out["cost_usd"] = json!(cost);
    }
    if outcome.hit_cost_limit {
        out["cost_capped"] = json!(true);
    }
    if outcome.hit_iteration_cap {
        out["iteration_capped"] = json!(true);
    }
    if outcome.hit_tool_loop_cap {
        out["tool_loop_capped"] = json!(true);
    }
    if outcome.hit_output_limit {
        out["output_capped"] = json!(true);
    }
    // A JSON-steered agent attaches its parsed reply as `data` for downstream nodes.
    if let Some(data) = data {
        out["data"] = data;
    }
    ActionOutcome::succeeded(Some(out))
}

/// Appended to a JSON-steered agent's system prompt so its final reply is a single
/// parseable JSON value (the executor extracts it into the step's `data`).
pub(super) const JSON_OUTPUT_INSTRUCTION: &str = "\n\nIMPORTANT: Your final reply MUST be a single \
valid JSON value and NOTHING else — no prose, no explanation, no markdown code fences.";

/// Extract a JSON value from an LLM reply: strip a surrounding ```json … ``` (or
/// bare ``` … ```) markdown fence if present, then parse. `None` if it doesn't
/// parse. Pure + testable.
pub(super) fn extract_json(content: &str) -> Option<Value> {
    let trimmed = content.trim();
    // Strip a leading fence (``` or ```json) and a trailing ``` if present.
    let body = trimmed
        .strip_prefix("```")
        .map(|rest| {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            let rest = rest.trim_start_matches(['\n', '\r']);
            rest.strip_suffix("```").unwrap_or(rest).trim()
        })
        .unwrap_or(trimmed);
    serde_json::from_str(body).ok()
}

/// Compose the agent's system prompt: its base instructions followed by each
/// named skill's runbook (SOUL §23), under a clear header. Pure so the composition
/// is unit-testable; an empty `skill_instructions` returns `base` unchanged.
pub(super) fn system_with_skills(base: &str, skill_instructions: &[String]) -> String {
    if skill_instructions.is_empty() {
        return base.to_string();
    }
    let mut system = base.to_string();
    system.push_str("\n\n# Skills\n\nYou have been given these skills (runbooks); follow them:\n");
    for instructions in skill_instructions {
        system.push_str("\n---\n");
        system.push_str(instructions);
    }
    system
}

/// The seed user turn for an automation agent: it has no human prompt, so describe
/// the firing trigger (if any) then the standing nudge. Pure + testable.
pub(super) fn trigger_prompt(trigger: Option<&Value>) -> String {
    match trigger {
        Some(t) => format!(
            "This automation was triggered by the following event:\n\n```json\n{}\n```\n\n{}",
            serde_json::to_string_pretty(t).unwrap_or_else(|_| t.to_string()),
            AGENT_TRIGGER_PROMPT,
        ),
        None => AGENT_TRIGGER_PROMPT.to_string(),
    }
}

/// The firing event for an action, resolving **both** runtime-context shapes
/// (mirrors [`collect_item`]): in a **graph** run the executor wraps the context as
/// `{ "trigger": <event>, "inputs": … }`, so the event is at `trigger`; in the
/// **linear** loop the context *is* the event. A serialized `TriggerEvent` never
/// carries a top-level `trigger` key, so its presence unambiguously marks the graph
/// shape. `None` for a manual/no-trigger run. Without this, the channel helpers read
/// `kind`/`channel`/`text` off the *wrapper* in graph runs and find nothing — silently
/// breaking the SOUL §25 in-channel reply for graph automations.
pub(super) fn firing_event(trigger: Option<&Value>) -> Option<&Value> {
    let t = trigger?;
    match t.get("trigger") {
        Some(event) => Some(event),
        None => Some(t),
    }
}

/// The channel a `ChannelMessage` trigger fired on, if this run was triggered by an
/// inbound channel message — so a chatbot agent's reply can be delivered there.
/// `None` for any other trigger (or a malformed one). Pure + testable.
pub(super) fn channel_from_trigger(trigger: Option<&Value>) -> Option<String> {
    let t = firing_event(trigger)?;
    if t.get("kind").and_then(Value::as_str) != Some("channel_message") {
        return None;
    }
    t.get("channel").and_then(Value::as_str).map(str::to_string)
}

/// The message text of a `ChannelMessage` trigger, if this run was fired by an
/// inbound channel message — so a `RunProfile` action responds to what was actually
/// said. `None` for any other trigger (or a textless one). Pure + testable.
pub(super) fn channel_text_from_trigger(trigger: Option<&Value>) -> Option<String> {
    let t = firing_event(trigger)?;
    if t.get("kind").and_then(Value::as_str) != Some("channel_message") {
        return None;
    }
    t.get("text")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// Build the seed [`ChatRequest`] + advertised tool subset for an [`LlmAgent`]
/// action. Pure (no I/O) so the seeding/model/tool-restriction rules are unit
/// testable: the model defaults to `default_model` unless the action pins one; the
/// seed is `[system, user-turn]` (the prebuilt `system`; the user turn describes the
/// `trigger`); and an empty `tools` list means "advertise the whole registry"
/// (`None`), while a non-empty list confines the loop to that subset.
/// The default system prompt for a `Summarize` action.
const SUMMARIZE_SYSTEM: &str = "You are a summarizer. Condense the material the user provides \
into a clear, faithful summary. Reply with the summary only — no preamble, no meta-commentary.";

/// Cap on the bytes of input a `Summarize` action feeds the model — a defensive
/// bound so a huge upstream output (a whole fetched page, a big file's text)
/// degrades to a truncated-input summary instead of a hard model-context error.
pub(super) const MAX_SUMMARIZE_INPUT_BYTES: usize = 200 * 1024;

/// Build the one-shot [`ChatRequest`] for a `Summarize` action (SOUL §11). Pure
/// (no I/O) so the input/instruction/model rules are unit-testable. `params`
/// must already be template-rendered: the summarized `input` is the param when
/// set (a non-string JSON — e.g. a whole-object `{{ inputs.x }}` reference — is
/// pretty-printed), else the firing trigger event; over-long input is truncated
/// on a char boundary with an explicit marker. `instructions` and a `max_words`
/// bound extend the system prompt; `model` falls back to `default_model`.
pub(super) fn summarize_request(
    params: &serde_json::Map<String, Value>,
    trigger: Option<&Value>,
    default_model: &str,
) -> Result<ChatRequest, String> {
    let mut input = match params.get("input") {
        None | Some(Value::Null) => match firing_event(trigger) {
            Some(event) => {
                serde_json::to_string_pretty(event).unwrap_or_else(|_| event.to_string())
            }
            None => {
                return Err(
                    "summarize: no `input` param and no firing trigger to summarize".to_string(),
                )
            }
        },
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err(
                    "summarize: `input` resolved to an empty string — check the \
                     {{ path }} reference"
                        .to_string(),
                );
            }
            s.to_string()
        }
        // A whole-value template (`"{{ inputs.fetch }}"`) resolves to non-string
        // JSON — summarize its pretty form rather than erroring.
        Some(other) => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
    };
    if input.len() > MAX_SUMMARIZE_INPUT_BYTES {
        let mut end = MAX_SUMMARIZE_INPUT_BYTES;
        while end > 0 && !input.is_char_boundary(end) {
            end -= 1;
        }
        input.truncate(end);
        input.push_str("\n\n[input truncated]");
    }

    let mut system = SUMMARIZE_SYSTEM.to_string();
    if let Some(instructions) = params
        .get("instructions")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        system.push_str("\n\n");
        system.push_str(instructions);
    }
    if let Some(words) = params
        .get("max_words")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)
    {
        system.push_str(&format!("\n\nKeep the summary under {words} words."));
    }

    let model = params
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_model)
        .to_string();
    Ok(ChatRequest::new(
        model,
        vec![ChatMessage::system(system), ChatMessage::user(input)],
    ))
}

pub(super) fn agent_request(
    agent: &LlmAgent,
    default_model: &str,
    system: String,
    trigger: Option<&Value>,
) -> (ChatRequest, Option<Vec<String>>) {
    let seed = vec![
        ChatMessage::system(system),
        ChatMessage::user(trigger_prompt(trigger)),
    ];
    let model = agent
        .model
        .clone()
        .unwrap_or_else(|| default_model.to_string());
    let allowed = if agent.tools.is_empty() {
        None
    } else {
        Some(agent.tools.clone())
    };
    let mut request = ChatRequest::new(model, seed);
    // The node's "thinking" picker (SOUL §7/§11): a non-empty effort is requested
    // for the loop; empty/absent leaves it to the provider default.
    request.reasoning_effort = agent
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (request, allowed)
}
