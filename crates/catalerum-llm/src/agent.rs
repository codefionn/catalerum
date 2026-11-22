//! The agent loop (SOUL §7).
//!
//! Given a [`ChatRequest`] and a [`ToolRegistry`], [`run_agent`] drives the
//! request→stream→execute-tools→append-results→loop cycle: it streams a turn;
//! if the model emits `tool_calls`, it dispatches each through the registry,
//! appends the assistant turn and the tool results to the conversation, and
//! loops until the model finishes normally — always ending with `message_done`
//! semantics. Shared by web chat, channel chat, MCP clients, and `LlmAgent`
//! automation actions.

mod discovery;
mod dispatch;
mod stream;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use catalerum_core::error::Result;
use catalerum_core::llm::{ChatMessage, ChatRequest, MediaInput};
use catalerum_core::model::{MessageRole, ToolCall};
use catalerum_core::stream::{FinishReason, StreamEvent, Usage};
use catalerum_core::tool::{ToolContext, ToolRegistry};

use crate::client::{CollectedTurn, OpenRouterClient};
use crate::compact::{compact, should_compact, CompactionConfig};

use discovery::{
    discovery_note, rewiden_from_history, widen_advertised, widen_explicitly_advertised,
};
use dispatch::dispatch_one;
use stream::stream_turn;
pub(crate) use stream::summarize_turn;

/// Tunables for the agent loop.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// Maximum number of tool-call rounds before giving up (guards against a
    /// model that loops forever). Each round is one model turn + its tools.
    pub max_iterations: usize,
    /// Stop when the model emits the same tool name + JSON arguments this many
    /// times in a row. Call ids and insignificant JSON formatting/key order are
    /// ignored. This catches deterministic retry loops while still allowing a
    /// small number of intentional polls/retries. `0` disables this guard.
    pub max_identical_tool_calls: usize,
    /// Stop after this many consecutive failed tool invocations, even when the
    /// model keeps changing the call or its arguments. Any successful tool call
    /// resets the streak. `0` disables this guard.
    pub max_consecutive_tool_errors: usize,
    /// Optional per-run cost ceiling in provider cost units (USD), from a grant's
    /// `cost_limit` constraint (SOUL §19). When set, the loop halts before another
    /// (paid) model turn once the run's cumulative `usage.cost_usd` reaches it.
    /// `None` = uncapped. Cost is only known *after* a turn, so the run can overshoot
    /// by its final turn's cost — the cap bounds the run, it can't pre-empt one turn.
    pub cost_limit: Option<f64>,
    /// Cooperative stop signal (the chat "Stop" button, SOUL §12). Cancelling it
    /// halts the run at the next interruption point: mid-stream (the partial text
    /// is kept, any half-assembled tool calls are dropped) or mid-dispatch (the
    /// in-flight call is abandoned and it plus every remaining call get a
    /// synthesized "cancelled" error result, so the transcript never dangles an
    /// unanswered tool call). The outcome is flagged [`AgentOutcome::stopped`].
    /// The default is a fresh token nobody cancels — a run without a stop control.
    pub cancel: CancellationToken,
    /// **Deferred tool advertising** (SOUL §7). Empty (the default) advertises the
    /// whole registry (filtered to `allowed_tools`) as before. Non-empty names the
    /// small pinned subset advertised **up front** instead — typically the discovery
    /// tools (`search_tools`, `list_tools`) plus any tool the run's standing prompts
    /// reference — cutting the per-request token cost of shipping every spec. The
    /// loop then injects a system note mapping the full catalog by area, and any
    /// tool a pinned tool's result *names* (a top-level `"tools": [{"name": …}]`
    /// array — the shape the discovery tools return) is advertised with its full
    /// spec from the next round on. Only applies when the caller didn't pin
    /// `request.tools`; a seed that matches no registered tool falls back to
    /// advertising everything, so a misconfigured run degrades verbose-but-correct.
    pub discovery_tools: Vec<String>,
    /// Input modalities advertised by the active model. These tailor optional
    /// tool descriptions and are injected as hidden, server-controlled dispatch
    /// metadata. The current llmleaf path uses `image`; other binary modalities
    /// remain disabled.
    pub input_modalities: Vec<String>,
    /// **Auto-compaction** (SOUL §7): when the running history approaches the
    /// model's context window, the loop folds its older part into a summary
    /// (one extra tool-less model turn) and continues on the compacted history
    /// instead of overflowing mid-run. On by default for every consumer;
    /// callers that know the model's real window (the gateway catalog's
    /// `context_length`) should set
    /// [`context_window`](CompactionConfig::context_window) — unset falls back
    /// to [`DEFAULT_CONTEXT_WINDOW`](crate::compact::DEFAULT_CONTEXT_WINDOW).
    pub compaction: CompactionConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations: 256,
            max_identical_tool_calls: 3,
            max_consecutive_tool_errors: 8,
            cost_limit: None,
            cancel: CancellationToken::new(),
            discovery_tools: Vec::new(),
            input_modalities: Vec::new(),
            compaction: CompactionConfig::default(),
        }
    }
}

/// How many times one run asks the model to continue an answer that was cut off
/// by the output-token limit before giving up and flagging the reply
/// ([`AgentOutcome::hit_output_limit`]).
const MAX_OUTPUT_CONTINUATIONS: usize = 3;

/// The outcome of an [`run_agent`] loop.
///
/// Not `Eq` because [`usage`](Self::usage) carries an `f64` cost.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentOutcome {
    /// The final assistant text (the answer after all tool rounds).
    pub content: String,
    /// The full conversation including appended assistant + tool messages, ready
    /// to persist or to continue from.
    pub messages: Vec<ChatMessage>,
    /// A flat log of every tool call dispatched and its (serialized) result.
    pub tool_invocations: Vec<ToolInvocation>,
    /// The finish reason of the terminal turn.
    pub finish_reason: Option<FinishReason>,
    /// Summed token usage across all turns that reported it.
    pub usage: Option<Usage>,
    /// Number of model turns executed.
    pub iterations: usize,
    /// `true` iff the loop stopped because it reached `max_iterations` while the
    /// model was *still* requesting tools — i.e. the run was **truncated**, not
    /// finished cleanly. The `content`/transcript are best-effort partial results;
    /// callers should treat the answer as incomplete (log it, flag it to the user).
    pub hit_iteration_cap: bool,
    /// `true` iff the loop stopped because calls repeated identically or tools
    /// kept failing without a successful recovery. Like the iteration cap, the
    /// transcript is complete but the requested work may be unfinished.
    pub hit_tool_loop_cap: bool,
    /// `true` iff the loop stopped because the run's cumulative cost reached the
    /// grant's [`cost_limit`](AgentConfig::cost_limit) (§19) while the model was
    /// still requesting tools — truncated by the budget, like `hit_iteration_cap`.
    /// The transcript is a best-effort partial; the answer should be treated as
    /// incomplete.
    pub hit_cost_limit: bool,
    /// `true` iff the final turn ended on the provider's output-token limit
    /// ([`FinishReason::Length`]) and the loop could not finish the answer by
    /// continuing it. The reply stops mid-thought, or is empty when the whole
    /// allowance went to reasoning, so callers must flag it as incomplete.
    pub hit_output_limit: bool,
    /// The provider's error message when the final turn's stream failed. The
    /// transcript is a best-effort partial.
    pub stream_error: Option<String>,
    /// `true` iff the run was halted by [`AgentConfig::cancel`] (the user's Stop).
    /// The transcript is a clean prefix — every persisted tool call carries a
    /// result (real or a synthesized "cancelled" error) — but the answer is
    /// deliberately partial.
    pub stopped: bool,
    /// How many times the loop **auto-compacted** its context mid-run (SOUL §7)
    /// — older history folded into a summary to stay under the model's window.
    /// `0` for the (overwhelmingly common) run that never came close.
    pub compactions: usize,
    /// The final model turn's `prompt + completion` tokens, when the provider
    /// reported usage — approximately the live size of this conversation's
    /// context at the end of the run (unlike [`usage`](Self::usage), which
    /// *sums* every turn). Drives the persistent chat-thread compaction
    /// trigger in `catalerum-api`.
    pub context_tokens: Option<u32>,
}

impl AgentOutcome {
    /// Why the run ended without a clean finish, as a stable slug, or `None` for
    /// a finished answer and for a deliberate Stop. Callers log it and show it to
    /// the user, so no way of ending short passes for a complete reply.
    #[must_use]
    pub fn incomplete_reason(&self) -> Option<&'static str> {
        if self.stopped {
            return None;
        }
        if self.stream_error.is_some() {
            return Some("stream_error");
        }
        if self.hit_cost_limit {
            return Some("cost_limit");
        }
        if self.hit_iteration_cap {
            return Some("iteration_cap");
        }
        if self.hit_tool_loop_cap {
            return Some("tool_loop");
        }
        if self.hit_output_limit {
            return Some("output_limit");
        }
        match self.finish_reason {
            Some(FinishReason::Stop | FinishReason::ToolCalls) => None,
            Some(FinishReason::Length) => Some("output_limit"),
            // The provider's safety filter declined (Anthropic `refusal`).
            Some(FinishReason::ContentFilter) => Some("content_filter"),
            Some(FinishReason::Other) => Some("other"),
            // The stream closed without a terminal event.
            None => Some("no_finish"),
        }
    }
}

/// One tool call dispatched during the loop, with its result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolInvocation {
    /// The tool call as the model emitted it.
    pub call: ToolCall,
    /// The result string appended back as the `tool` message content. On a
    /// dispatch error this holds a JSON `{"error": …}` object (so the model can
    /// recover) and `is_error` is set.
    pub result: String,
    /// True if the tool returned an error rather than a normal result.
    pub is_error: bool,
    /// Wall-clock time the dispatch took, in milliseconds. Surfaced live in the
    /// [`StreamEvent::ToolResult`] frame and persisted for replay (SOUL §12).
    pub duration_ms: u64,
    /// Native media removed from the textual result and attached ephemerally to
    /// the following model turn.
    #[doc(hidden)]
    pub media: Vec<MediaInput>,
}

/// A transcript message the loop has just finalized and appended — handed to
/// [`TurnObserver::on_message`] so a caller can persist it **incrementally**, the
/// moment it completes, rather than batching the whole transcript at the end of
/// the loop (SOUL §7/§12). One is emitted per appended assistant turn (text + any
/// tool calls) and per tool result, in transcript order. The seed/system prefix
/// is never emitted (it is ephemeral, not part of the durable transcript).
///
/// Borrows from the loop's transient per-turn state — valid only for the
/// `on_message` call; do not retain it.
#[derive(Clone, Copy, Debug)]
pub struct CompletedMessage<'a> {
    /// [`Assistant`](MessageRole::Assistant) for a model turn, [`Tool`](MessageRole::Tool)
    /// for a tool result.
    pub role: MessageRole,
    /// The message text (a tool result's serialized payload, or an assistant
    /// turn's prose — possibly empty for a pure tool-call turn).
    pub content: &'a str,
    /// Tool calls the assistant turn emitted (empty for a tool result).
    pub tool_calls: &'a [ToolCall],
    /// For a tool result, the id of the call it answers (`None` for an assistant
    /// turn).
    pub tool_call_id: Option<&'a str>,
    /// For a tool result, whether the dispatch failed. Always `false` for an
    /// assistant turn.
    pub tool_is_error: bool,
    /// For a tool result, the dispatch's wall-clock duration in milliseconds.
    /// `None` for an assistant turn.
    pub tool_duration_ms: Option<i64>,
    /// The reasoning blocks an assistant turn emitted, signatures included, so
    /// the observer can persist them for a later replay. Empty for a tool result
    /// and for a turn the user stopped mid-stream.
    pub reasoning_details: &'a [catalerum_core::stream::ReasoningDetail],
}

/// Observes the raw [`StreamEvent`]s of each model turn as they arrive, so a
/// caller can relay token deltas live — web/channel chat forwards them to the
/// client WebSocket and the cross-pod [`bus`](catalerum_bus) relay (SOUL §7) —
/// and (via [`on_message`](Self::on_message)) persist each completed transcript
/// message as it lands. Distinct from the final [`AgentOutcome`], which is the
/// collected result.
#[async_trait]
pub trait TurnObserver: Send {
    /// Called once per [`StreamEvent`] (text / tool-call deltas, each turn's own
    /// terminal `Done`, and any stream-level `Error`) in arrival order. Returning
    /// an `Err` aborts the loop (e.g. the client socket went away).
    async fn on_event(&mut self, event: &StreamEvent) -> Result<()>;

    /// Called once for each transcript message the loop appends — every assistant
    /// turn and every tool result, in order — so a caller can persist it the
    /// moment it completes (SOUL §7/§12) instead of waiting for the loop to finish
    /// and batching the whole [`AgentOutcome::messages`] tail. A long multi-round
    /// turn is thus durable round-by-round: a crash or dropped socket mid-loop
    /// keeps everything persisted so far. Returning an `Err` aborts the loop (so a
    /// persistence failure stops the run with the prefix already saved).
    ///
    /// Default: a no-op — the non-streaming entry point and any caller that
    /// persists from the returned [`AgentOutcome`] (or not at all) ignore it.
    async fn on_message(&mut self, _message: &CompletedMessage<'_>) -> Result<()> {
        Ok(())
    }

    /// Called at every round boundary — right after a round's tool results are
    /// appended, and again when the model finishes without tool calls — to drain
    /// any user messages that arrived **while the loop was running** (the chat
    /// composer stays live during generation, SOUL §12). Returned messages are
    /// appended to the conversation at that boundary (so the model sees them next
    /// round), and a non-empty drain at the finish point keeps the loop going for
    /// one more round instead of ending the turn. The caller is responsible for
    /// persisting what it hands over (this loop only persists what *it* appends,
    /// via [`on_message`](Self::on_message)).
    ///
    /// Default: nothing queued — callers without a live input channel never loop.
    async fn poll_user_input(&mut self) -> Result<Vec<ChatMessage>> {
        Ok(Vec::new())
    }
}

/// A [`TurnObserver`] that drops every event — used by the non-streaming
/// [`run_agent`] entry point.
struct NoopObserver;

#[async_trait]
impl TurnObserver for NoopObserver {
    async fn on_event(&mut self, _event: &StreamEvent) -> Result<()> {
        Ok(())
    }
}

/// Opens a model-turn [`StreamEvent`] stream for a request. Implemented by
/// [`OpenRouterClient`]; abstracted so the loop is unit-testable with a scripted
/// stream source (the public entry points always pass a real client).
#[async_trait]
pub(crate) trait TurnStreamer {
    async fn open(&self, request: ChatRequest) -> Result<BoxStream<'static, Result<StreamEvent>>>;
}

#[async_trait]
impl TurnStreamer for OpenRouterClient {
    async fn open(&self, request: ChatRequest) -> Result<BoxStream<'static, Result<StreamEvent>>> {
        self.stream(request).await
    }
}

/// Run the agent loop against an [`OpenRouterClient`].
///
/// `request` supplies the model, the seed messages, and (optionally) `tools` /
/// `tool_choice`; if `request.tools` is empty it's populated from `registry`
/// (filtered to `allowed_tools` when given). `ctx` is passed to every tool
/// dispatch. The returned [`AgentOutcome`] carries the final text plus the full
/// message history (seed + assistant/tool turns).
///
/// Always terminates: either the model finishes without tool calls, or
/// `max_iterations` is hit (returning the best-effort transcript so far).
pub async fn run_agent(
    client: &OpenRouterClient,
    request: ChatRequest,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    config: &AgentConfig,
    allowed_tools: Option<&[String]>,
) -> Result<AgentOutcome> {
    let mut observer = NoopObserver;
    run_loop(
        client,
        request,
        registry,
        ctx,
        config,
        allowed_tools,
        &mut observer,
    )
    .await
}

/// Like [`run_agent`], but forwards every [`StreamEvent`] to `observer` as it
/// arrives — the streaming path behind web/channel chat (SOUL §7, §12). The
/// observer relays token deltas to the client + the cross-pod bus while the loop
/// still collects, dispatches tool calls, and returns the full [`AgentOutcome`]
/// (seed messages + appended assistant/tool turns) for durable persistence.
pub async fn run_agent_streaming<O: TurnObserver>(
    client: &OpenRouterClient,
    request: ChatRequest,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    config: &AgentConfig,
    allowed_tools: Option<&[String]>,
    observer: &mut O,
) -> Result<AgentOutcome> {
    run_loop(
        client,
        request,
        registry,
        ctx,
        config,
        allowed_tools,
        observer,
    )
    .await
}

/// The shared agent loop, generic over the turn source ([`TurnStreamer`]) and the
/// event observer ([`TurnObserver`]). Both public entry points funnel here, so
/// streaming and non-streaming share one implementation.
async fn run_loop<S, O>(
    streamer: &S,
    mut request: ChatRequest,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    config: &AgentConfig,
    allowed_tools: Option<&[String]>,
    observer: &mut O,
) -> Result<AgentOutcome>
where
    S: TurnStreamer + ?Sized,
    O: TurnObserver,
{
    // Advertise the registry's tools if the caller didn't pin an explicit set.
    // With `discovery_tools` configured, advertise only that pinned subset up
    // front (deferred advertising, SOUL §7) — the model loads the rest through
    // the discovery tools, whose results widen the set below. A seed that matches
    // nothing registered falls back to advertising everything.
    let mut discovery = false;
    if request.tools.is_empty() {
        if !config.discovery_tools.is_empty() {
            let seed: Vec<String> = config
                .discovery_tools
                .iter()
                .filter(|n| allowed_tools.is_none_or(|a| a.contains(n)))
                .cloned()
                .collect();
            request.tools = registry.specs_for_model_names(&seed, &config.input_modalities);
            discovery = !request.tools.is_empty();
        }
        if !discovery {
            request.tools = registry.specs_for_model(allowed_tools, &config.input_modalities);
        }
    }

    let mut messages = std::mem::take(&mut request.messages);
    // A deferred run gets a standing system note mapping the full catalog by
    // area, so the model knows what exists beyond the pinned subset and how to
    // load it. Inserted after any caller system prefix; ephemeral like the rest
    // of the seed (the observer never persists seed messages).
    if discovery {
        // Tools discovered on an earlier turn of a replayed conversation stay
        // discovered: re-widen the advertised set from the discovery-tool
        // results already in the history, so the model calls them directly
        // instead of re-running `search_tools` every message.
        rewiden_from_history(
            &mut request.tools,
            registry,
            allowed_tools,
            &messages,
            &config.discovery_tools,
            &config.input_modalities,
        );
        let at = messages
            .iter()
            .take_while(|m| m.role == MessageRole::System)
            .count();
        messages.insert(
            at,
            ChatMessage::system(discovery_note(registry, allowed_tools)),
        );
    }
    let mut outcome = AgentOutcome::default();
    // Some providers occasionally emit a clean `stop` with no text after a
    // successful tool round. One bounded recovery turn prevents the user from
    // receiving a blank final response without risking an infinite loop.
    let mut retried_empty_finish = false;
    // A turn cut off by the output-token limit is not a finished answer. Ask the
    // model to pick up where it stopped, a bounded number of times, and join the
    // pieces into one reply.
    let mut output_continuations = 0usize;
    let mut continued_text = String::new();
    // The previous turn's provider-reported usage — the compaction trigger's
    // grounded signal for how big the next prompt really is (the chars/4
    // estimate can undercount). Reset after a compaction: it describes the
    // pre-fold history.
    let mut last_turn_usage: Option<Usage> = None;
    // Separate from the generous overall round allowance: these streaks catch a
    // model stuck retrying deterministic failures (or pointlessly polling the
    // exact same successful call) before it can burn through the whole budget.
    let mut previous_tool_signature: Option<String> = None;
    let mut identical_tool_call_streak = 0usize;
    let mut consecutive_tool_errors = 0usize;

    for round in 0..config.max_iterations.max(1) {
        outcome.iterations = round + 1;

        // Stop pressed between rounds: don't open another (paid) model turn.
        if config.cancel.is_cancelled() {
            outcome.stopped = true;
            outcome.content = last_assistant_text(&messages);
            outcome.messages = messages;
            return Ok(outcome);
        }

        // Auto-compaction (SOUL §7): before opening the next (paid) turn, check
        // whether the history is projected to blow the context window; if so,
        // fold its older part into a summary via one extra tool-less model turn
        // and continue on `system prefix + summary + recent tail`. Fail-open —
        // a failed/empty/cancelled summarize leaves the history untouched and
        // this turn proceeds as it would have. The summarize turn's own usage
        // joins the run's accounting (it is real spend the §19 cost cap must see),
        // and the synthesized `Compacted` event rides the observer relay so a
        // live client can mark the fold.
        if should_compact(&messages, last_turn_usage.as_ref(), &config.compaction) {
            if let Some(folded) = compact(
                streamer,
                &request.model,
                &mut messages,
                &config.compaction,
                &config.cancel,
            )
            .await
            {
                accumulate_usage(&mut outcome.usage, folded.usage);
                last_turn_usage = None;
                outcome.compactions += 1;
                observer
                    .on_event(&StreamEvent::Compacted {
                        folded: folded.folded as u32,
                        summary: folded.summary,
                    })
                    .await?;
            }
        }

        // Build this turn's request from the running message history.
        let mut turn_req = request.clone();
        turn_req.messages = messages.clone();

        let turn = stream_turn(streamer, turn_req, &config.cancel, observer).await?;

        accumulate_usage(&mut outcome.usage, turn.collected.usage);
        if let Some(u) = turn.collected.usage {
            last_turn_usage = Some(u);
            outcome.context_tokens = Some(u.prompt_tokens.saturating_add(u.completion_tokens));
        }
        outcome.finish_reason = turn.collected.finish_reason;

        // Stopped mid-stream: keep the partial text but DROP any half-assembled
        // tool calls — they were never dispatched, and persisting them would leave
        // the transcript with dangling calls (which a later replay must not feed
        // back to the model). An all-empty partial appends nothing.
        if turn.stopped {
            if !turn.collected.content.is_empty() {
                messages.push(ChatMessage {
                    role: MessageRole::Assistant,
                    content: turn.collected.content.clone(),
                    images: Vec::new(),
                    media: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                    reasoning: None,
                    reasoning_details: Vec::new(),
                });
                observer
                    .on_message(&CompletedMessage {
                        role: MessageRole::Assistant,
                        content: &turn.collected.content,
                        tool_calls: &[],
                        tool_call_id: None,
                        tool_is_error: false,
                        tool_duration_ms: None,
                        reasoning_details: &[],
                    })
                    .await?;
            }
            outcome.stopped = true;
            outcome.content = turn.collected.content;
            outcome.messages = messages;
            return Ok(outcome);
        }

        // Append the assistant turn (text + any tool calls) to the history, and
        // hand it to the observer for incremental persistence (SOUL §7/§12) — the
        // final answer turn and every intermediate tool-call turn alike, each as
        // its own durable row the instant it completes.
        messages.push(assistant_message(&turn.collected));
        observer
            .on_message(&CompletedMessage {
                role: MessageRole::Assistant,
                content: &turn.collected.content,
                tool_calls: &turn.collected.tool_calls,
                tool_call_id: None,
                tool_is_error: false,
                tool_duration_ms: None,
                reasoning_details: &turn.collected.reasoning_details,
            })
            .await?;

        // A stream-level error is terminal — return the best-effort transcript
        // (the observer has already forwarded the error event to the client).
        if let Some(error) = turn.error {
            outcome.stream_error = Some(error);
            outcome.content = turn.collected.content;
            outcome.messages = messages;
            return Ok(outcome);
        }

        // No tool calls — normally the finished answer. But if the user typed
        // more while this turn streamed (SOUL §12), fold the queued messages in
        // and keep the loop going so this same exchange answers them; only an
        // empty queue actually ends the turn.
        if turn.collected.tool_calls.is_empty() {
            let queued = observer.poll_user_input().await?;
            if queued.is_empty() {
                let rounds_left = round + 1 < config.max_iterations.max(1);
                let cut_off = turn.collected.finish_reason == Some(FinishReason::Length);
                // An empty cut-off turn spent its whole allowance on reasoning;
                // the single empty-response retry below covers it. Continuing
                // only makes sense when there is text to continue from.
                if cut_off && !turn.collected.content.trim().is_empty() {
                    if output_continuations < MAX_OUTPUT_CONTINUATIONS && rounds_left {
                        output_continuations += 1;
                        continued_text.push_str(&turn.collected.content);
                        // A user-role nudge, not a system one: gateways hoist
                        // system messages out of the history, which would leave
                        // the cut-off assistant text as the last message, and
                        // current models reject that as a prefill.
                        messages.push(ChatMessage::user(
                            "Your previous assistant response was cut off by the output-token \
                             limit. Continue exactly where it stopped. Do not repeat or \
                             summarize what you already wrote, and do not restart the answer.",
                        ));
                        continue;
                    }
                    outcome.hit_output_limit = true;
                }
                if turn.collected.content.trim().is_empty()
                    && !retried_empty_finish
                    && round + 1 < config.max_iterations.max(1)
                {
                    retried_empty_finish = true;
                    messages.push(ChatMessage::system(
                        "Your previous assistant response was empty. Continue the user's request \
                         now and provide a non-empty final answer. If tools were used, finish the \
                         requested work. Correct tool errors instead of repeating unchanged calls.",
                    ));
                    continue;
                }
                outcome.hit_output_limit |= cut_off;
                continued_text.push_str(&turn.collected.content);
                outcome.content = continued_text;
                outcome.messages = messages;
                return Ok(outcome);
            }
            // The user spoke mid-turn: the next turn answers them, so it is a new
            // reply and not a continuation of the cut-off one.
            continued_text.clear();
            messages.extend(queued);
            continue;
        }

        // Cost ceiling (§19 grant `cost_limit`): the model wants another tool round,
        // but if this run's cumulative spend has reached the cap, stop before another
        // (paid) turn rather than dispatch more tools. Cost is only known post-turn,
        // so this can overshoot by the just-finished turn's cost — the cap bounds the
        // run, it can't pre-empt a single turn. Return the best-effort transcript,
        // flagged as cost-capped (parallel to `hit_iteration_cap`).
        if let Some(limit) = config.cost_limit {
            let spent = outcome.usage.as_ref().and_then(|u| u.cost_usd);
            if spent.is_some_and(|c| c >= limit) {
                outcome.hit_cost_limit = true;
                outcome.content = turn.collected.content;
                outcome.messages = messages;
                return Ok(outcome);
            }
        }

        // Dispatch each tool call and append its result as a `tool` message.
        // Bracket each dispatch with synthesized lifecycle events so a streaming
        // client can show the call running and then resolve it (SOUL §7/§12).
        // These are emitted directly to the observer here (the model stream never
        // produces them); they ride the same relay path as token deltas.
        //
        // A stop mid-dispatch abandons the in-flight call; it and every remaining
        // call still get a synthesized "cancelled" error result — emitted, appended,
        // and persisted like a real one — so every tool call of the just-persisted
        // assistant turn is answered and the transcript can be replayed.
        let mut stopped_mid_tools = false;
        let mut tool_loop_capped = false;
        let mut round_media = Vec::new();
        for call in &turn.collected.tool_calls {
            observer
                .on_event(&StreamEvent::ToolCallStarted {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .await?;
            let invocation = if stopped_mid_tools {
                cancelled_invocation(call)
            } else {
                tokio::select! {
                    biased;
                    _ = config.cancel.cancelled() => {
                        stopped_mid_tools = true;
                        cancelled_invocation(call)
                    }
                    inv = dispatch_one(registry, ctx, call, &config.input_modalities) => inv,
                }
            };
            // Cap the result on the wire so a huge payload can't bloat the socket;
            // the full result is still appended to the transcript and persisted.
            let (wire_result, truncated) = cap_result(&invocation.result);
            observer
                .on_event(&StreamEvent::ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    result: wire_result,
                    is_error: invocation.is_error,
                    duration_ms: Some(invocation.duration_ms),
                    truncated,
                })
                .await?;
            messages.push(tool_message(call, &invocation.result));
            // Persist the tool result incrementally too, carrying its dispatch
            // status + timing so a replayed row shows the same error flag / duration
            // as the live card.
            observer
                .on_message(&CompletedMessage {
                    role: MessageRole::Tool,
                    content: &invocation.result,
                    tool_calls: &[],
                    tool_call_id: Some(&call.id),
                    tool_is_error: invocation.is_error,
                    tool_duration_ms: Some(invocation.duration_ms as i64),
                    reasoning_details: &[],
                })
                .await?;
            // Deferred advertising (SOUL §7): discovery tools use the historical
            // `tools: [{name}]` result protocol. Any tool may additionally opt in
            // explicitly with `advertise_tools: [name]` — e.g. `use_skill` loads
            // the tools in its runbook and `open_terminal` loads the companion
            // session tools it just told the model to use.
            if discovery && !invocation.is_error {
                if config.discovery_tools.contains(&call.name) {
                    widen_advertised(
                        &mut request.tools,
                        registry,
                        allowed_tools,
                        &invocation.result,
                        &config.input_modalities,
                    );
                }
                widen_explicitly_advertised(
                    &mut request.tools,
                    registry,
                    allowed_tools,
                    &invocation.result,
                    &config.input_modalities,
                );
            }
            if !stopped_mid_tools {
                let signature = tool_call_signature(call);
                if previous_tool_signature.as_deref() == Some(signature.as_str()) {
                    identical_tool_call_streak += 1;
                } else {
                    previous_tool_signature = Some(signature);
                    identical_tool_call_streak = 1;
                }
                if invocation.is_error {
                    consecutive_tool_errors += 1;
                } else {
                    consecutive_tool_errors = 0;
                }
                tool_loop_capped |= config.max_identical_tool_calls > 0
                    && identical_tool_call_streak >= config.max_identical_tool_calls;
                tool_loop_capped |= config.max_consecutive_tool_errors > 0
                    && consecutive_tool_errors >= config.max_consecutive_tool_errors;
            }
            round_media.extend(invocation.media.iter().cloned());
            outcome.tool_invocations.push(invocation);
        }
        if stopped_mid_tools {
            outcome.stopped = true;
            outcome.content = turn.collected.content;
            outcome.messages = messages;
            return Ok(outcome);
        }
        if tool_loop_capped {
            outcome.hit_tool_loop_cap = true;
            outcome.content = last_assistant_text(&messages);
            outcome.messages = messages;
            return Ok(outcome);
        }
        // Function-call outputs must remain contiguous after their assistant
        // turn, especially when the model emitted parallel calls. Attach all
        // native media only after every textual tool result has been appended.
        if !round_media.is_empty() {
            let mut message = ChatMessage::user(
                "Native image returned by the preceding file tool call(s). Analyze the attached \
                 image directly.",
            );
            message.media = round_media;
            messages.push(message);
        }
        // Fold in anything the user typed while this round ran — it lands right
        // after the round's tool results, so the model sees it next round.
        messages.extend(observer.poll_user_input().await?);
        // ...and loop to let the model react to the tool results.
    }

    // Hit the iteration cap without a clean finish (a model that kept requesting
    // tools). Surface the last assistant turn's text as the result so the
    // `content` contract (the final assistant text) isn't left empty, flag the run
    // as truncated, and return the best-effort transcript so far.
    outcome.hit_iteration_cap = true;
    outcome.content = last_assistant_text(&messages);
    outcome.messages = messages;
    Ok(outcome)
}


/// The most recent assistant turn's text, or empty — the best-effort `content`
/// for a run that ended without a clean final answer (cap hit / stopped).
fn last_assistant_text(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == MessageRole::Assistant)
        .map(|m| m.content.clone())
        .unwrap_or_default()
}

/// Stable identity for loop detection: tool name plus semantic JSON arguments.
/// Object keys are sorted recursively, so whitespace and key order do not let an
/// otherwise identical call evade the guard. Invalid JSON falls back to trimmed
/// source text; dispatch will report that syntax error in the usual way.
fn tool_call_signature(call: &ToolCall) -> String {
    let arguments = match serde_json::from_str::<Json>(&call.arguments) {
        Ok(value) => canonical_json(&value),
        Err(_) => call.arguments.trim().to_string(),
    };
    format!("{}\0{arguments}", call.name)
}

fn canonical_json(value: &Json) -> String {
    fn write(value: &Json, out: &mut String) {
        match value {
            Json::Null => out.push_str("null"),
            Json::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Json::Number(value) => out.push_str(&value.to_string()),
            Json::String(value) => {
                out.push_str(&serde_json::to_string(value).expect("JSON string serialization"));
            }
            Json::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(value, out);
                }
                out.push(']');
            }
            Json::Object(values) => {
                out.push('{');
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_unstable();
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(
                        &serde_json::to_string(key).expect("JSON object key serialization"),
                    );
                    out.push(':');
                    write(&values[key], out);
                }
                out.push('}');
            }
        }
    }

    let mut out = String::new();
    write(value, &mut out);
    out
}

/// The synthesized result for a tool call abandoned by a user stop: an error
/// payload the model (on a later regenerate/replay) can recognize and recover
/// from, flagged as an error so the UI card resolves to ✗.
fn cancelled_invocation(call: &ToolCall) -> ToolInvocation {
    ToolInvocation {
        call: call.clone(),
        result: error_result(
            "cancelled — the user stopped generation before this tool call completed",
        ),
        is_error: true,
        duration_ms: 0,
        media: Vec::new(),
    }
}


/// Max bytes of a tool result shipped on the live wire. The full result is always
/// appended to the transcript and persisted (and shown on reload); this only
/// bounds the streamed [`StreamEvent::ToolResult`] frame so a huge payload can't
/// bloat the WebSocket.
const TOOL_RESULT_WIRE_CAP: usize = 16 * 1024;

/// Cap a tool result for the live wire, returning `(text, truncated)`. Cuts on a
/// UTF-8 char boundary so the frame stays valid.
fn cap_result(result: &str) -> (String, bool) {
    if result.len() <= TOOL_RESULT_WIRE_CAP {
        return (result.to_string(), false);
    }
    let mut end = TOOL_RESULT_WIRE_CAP;
    while end > 0 && !result.is_char_boundary(end) {
        end -= 1;
    }
    (result[..end].to_string(), true)
}

/// Build the assistant message recording a model turn. The turn's reasoning is
/// carried so the next (tool-result) request in this loop echoes it back verbatim
/// — a reasoning model's signed chain must survive the tool-call round-trip
/// (SOUL §7). Reasoning is ephemeral to the loop; it is not persisted.
fn assistant_message(turn: &CollectedTurn) -> ChatMessage {
    ChatMessage {
        role: MessageRole::Assistant,
        content: turn.content.clone(),
        images: Vec::new(),
        media: Vec::new(),
        tool_calls: turn.tool_calls.clone(),
        tool_call_id: None,
        name: None,
        reasoning: (!turn.reasoning.is_empty()).then(|| turn.reasoning.clone()),
        reasoning_details: turn.reasoning_details.clone(),
    }
}

/// Build a `tool` result message answering a specific call.
fn tool_message(call: &ToolCall, result: &str) -> ChatMessage {
    ChatMessage {
        role: MessageRole::Tool,
        content: result.to_string(),
        images: Vec::new(),
        media: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: Some(call.id.clone()),
        name: Some(call.name.clone()),
        reasoning: None,
        reasoning_details: Vec::new(),
    }
}

/// Render a tool result `Json` as the string content of a `tool` message.
/// Strings pass through verbatim; everything else is JSON-encoded.
fn value_to_string(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A JSON `{"error": msg}` payload for a failed tool call.
fn error_result(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Sum optional usages (used to total tokens across turns).
fn accumulate_usage(acc: &mut Option<Usage>, turn: Option<Usage>) {
    let Some(t) = turn else { return };
    match acc {
        Some(a) => {
            a.prompt_tokens += t.prompt_tokens;
            a.completion_tokens += t.completion_tokens;
            a.total_tokens += t.total_tokens;
            a.cached_tokens += t.cached_tokens;
            a.cache_creation_tokens += t.cache_creation_tokens;
            a.cost_usd = match (a.cost_usd, t.cost_usd) {
                (Some(x), Some(y)) => Some(x + y),
                (x, y) => x.or(y),
            };
        }
        None => *acc = Some(t),
    }
}

#[cfg(test)]
mod tests;
