//! The agent loop (SOUL §7).
//!
//! Given a [`ChatRequest`] and a [`ToolRegistry`], [`run_agent`] drives the
//! request→stream→execute-tools→append-results→loop cycle: it streams a turn;
//! if the model emits `tool_calls`, it dispatches each through the registry,
//! appends the assistant turn and the tool results to the conversation, and
//! loops until the model finishes normally — always ending with `message_done`
//! semantics. Shared by web chat, channel chat, MCP clients, and `LlmAgent`
//! automation actions.

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

use catalerum_core::error::Result;
use catalerum_core::llm::{ChatMessage, ChatRequest, MediaInput, ToolSpec};
use catalerum_core::model::{MessageRole, ToolCall};
use catalerum_core::stream::{FinishReason, StreamEvent, Usage};
use catalerum_core::tool::{ToolContext, ToolRegistry, MODEL_MEDIA_RESULT_FIELD};

use crate::client::{CollectedTurn, OpenRouterClient, ReasoningAssembler, ToolCallAssembler};
use crate::compact::{compact, should_compact, CompactionConfig};

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
            })
            .await?;

        // A stream-level error is terminal — return the best-effort transcript
        // (the observer has already forwarded the error event to the client).
        if turn.errored {
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
                outcome.content = turn.collected.content;
                outcome.messages = messages;
                return Ok(outcome);
            }
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

/// The system note injected on a deferred-advertising run (SOUL §7): the full
/// tool catalog mapped by area (capability domain), plus how to load a tool.
/// Deterministic — sorted domains and names — so the prompt prefix stays
/// byte-stable across turns and provider prompt caching keeps working.
fn discovery_note(registry: &ToolRegistry, allowed: Option<&[String]>) -> String {
    let mut note = String::from(
        "## Tool discovery\n\
         To keep your context lean, only a small pinned subset of the available \
         tools is advertised to you up front. Many more exist — their names are \
         listed by area below. To call one, first load it through the advertised \
         discovery tools: describe what you need to `search_tools`, or browse the \
         catalog with `list_tools` (pass `names` to select specific ones). Every \
         tool a discovery result returns becomes callable from your next step. \
         Never guess an unloaded tool's arguments — load it first. Tools loaded \
         through a discovery call earlier in this conversation are still loaded: \
         call them directly, do NOT run `search_tools`/`list_tools` again for a \
         tool you already discovered. Search only for tools you have not loaded \
         yet.\n\n\
         Available tools by area:\n",
    );
    for (domain, names) in registry.domain_groups(allowed) {
        note.push_str("- ");
        note.push_str(&domain);
        note.push_str(": ");
        note.push_str(&names.join(", "));
        note.push('\n');
    }
    note
}

/// Widen a deferred run's advertised tools with those named in a discovery-tool
/// result (SOUL §7). The protocol: a top-level `"tools"` array whose items carry
/// a `"name"` — the shape `search_tools`/`list_tools` return. Names already
/// advertised, outside `allowed`, or unknown to the registry are skipped, so a
/// result can never advertise past the run's allow-list (and advertising grants
/// no authority — dispatch still capability-gates every call).
fn widen_advertised(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    result: &str,
    input_modalities: &[String],
) {
    let Ok(parsed) = serde_json::from_str::<Json>(result) else {
        return;
    };
    let Some(items) = parsed.get("tools").and_then(Json::as_array) else {
        return;
    };
    let mut fresh: Vec<String> = Vec::new();
    for item in items {
        let Some(n) = item.get("name").and_then(Json::as_str) else {
            continue;
        };
        if allowed.is_some_and(|a| !a.iter().any(|x| x == n))
            || advertised.iter().any(|s| s.name == n)
            || fresh.iter().any(|f| f == n)
        {
            continue;
        }
        fresh.push(n.to_string());
    }
    advertised.extend(registry.specs_for_model_names(&fresh, input_modalities));
}

/// Widen from an explicit `"advertise_tools": ["name", ...]` result. Unlike the
/// legacy discovery-tool protocol above, this may be returned by any tool and is
/// intentionally unambiguous: ordinary business data containing a `tools` field
/// cannot accidentally widen the model surface. Capability/allow-list filtering
/// and exact registry lookup still apply, so this grants no authority.
fn widen_explicitly_advertised(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    result: &str,
    input_modalities: &[String],
) {
    let Ok(parsed) = serde_json::from_str::<Json>(result) else {
        return;
    };
    let Some(items) = parsed.get("advertise_tools").and_then(Json::as_array) else {
        return;
    };
    let mut fresh: Vec<String> = Vec::new();
    for item in items {
        let Some(name) = item.as_str() else {
            continue;
        };
        if allowed.is_some_and(|a| !a.iter().any(|candidate| candidate == name))
            || advertised.iter().any(|spec| spec.name == name)
            || fresh.iter().any(|candidate| candidate == name)
        {
            continue;
        }
        fresh.push(name.to_string());
    }
    advertised.extend(registry.specs_for_model_names(&fresh, input_modalities));
}

/// Re-widen a deferred run's advertised tools from replayed history (SOUL §7):
/// every discovery-tool result already in the conversation names tools the model
/// loaded on an earlier turn. Without this, each new chat message reopens the run
/// with only the pinned subset and the model has to re-run `search_tools` for
/// tools it already found. Replayed messages carry no error flag, but that's
/// harmless — [`widen_advertised`] only acts on the `"tools": [{"name"}]` result
/// shape, which an error payload never has, and the allow-list still gates every
/// name.
fn rewiden_from_history(
    advertised: &mut Vec<ToolSpec>,
    registry: &ToolRegistry,
    allowed: Option<&[String]>,
    history: &[ChatMessage],
    discovery_tools: &[String],
    input_modalities: &[String],
) {
    let discovery_call_ids: std::collections::HashSet<&str> = history
        .iter()
        .filter(|m| m.role == MessageRole::Assistant)
        .flat_map(|m| m.tool_calls.iter())
        .filter(|c| discovery_tools.iter().any(|d| d == &c.name))
        .map(|c| c.id.as_str())
        .collect();
    for message in history {
        if message.role != MessageRole::Tool {
            continue;
        }
        if message
            .tool_call_id
            .as_deref()
            .is_some_and(|id| discovery_call_ids.contains(id))
        {
            widen_advertised(
                advertised,
                registry,
                allowed,
                &message.content,
                input_modalities,
            );
        }
        widen_explicitly_advertised(
            advertised,
            registry,
            allowed,
            &message.content,
            input_modalities,
        );
    }
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

/// One streamed model turn, collected while forwarding each event to an observer.
struct StreamedTurn {
    /// The folded turn (text, assembled tool calls, finish reason, usage).
    collected: CollectedTurn,
    /// True if the stream surfaced a terminal [`StreamEvent::Error`].
    errored: bool,
    /// True if the turn was cut short by the caller's cancel token (the user's
    /// Stop): `collected` holds whatever had streamed by then.
    stopped: bool,
}

/// Drive a single turn's [`StreamEvent`] stream: forward every event to
/// `observer` (for live relay) and fold it into a [`CollectedTurn`]. The
/// streaming analogue of [`crate::client::collect_turn`]. Cancelling `cancel`
/// abandons the stream mid-turn (dropping it aborts the underlying request) and
/// returns the partial fold flagged `stopped`.
async fn stream_turn<S, O>(
    streamer: &S,
    request: ChatRequest,
    cancel: &CancellationToken,
    observer: &mut O,
) -> Result<StreamedTurn>
where
    S: TurnStreamer + ?Sized,
    O: TurnObserver,
{
    let mut stream = streamer.open(request).await?;
    let mut content = String::new();
    let mut asm = ToolCallAssembler::default();
    let mut reasoning = String::new();
    let mut reasoning_asm = ReasoningAssembler::default();
    let mut finish_reason = None;
    let mut usage = None;
    let mut errored = false;
    let mut stopped = false;

    loop {
        let item = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                stopped = true;
                break;
            }
            item = stream.next() => match item {
                Some(item) => item,
                None => break,
            },
        };
        let event = item?;
        // Relay first so the client sees deltas with minimal latency; an observer
        // error (e.g. the socket closed) aborts the loop.
        observer.on_event(&event).await?;
        match event {
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
            StreamEvent::Error { .. } => errored = true,
            // Synthesized by the dispatch loop, never produced by the model
            // stream — only here to keep the match exhaustive.
            StreamEvent::ToolCallStarted { .. }
            | StreamEvent::ToolResult { .. }
            | StreamEvent::Compacted { .. } => {}
        }
    }

    Ok(StreamedTurn {
        collected: CollectedTurn {
            content,
            tool_calls: asm.finish(),
            reasoning,
            reasoning_details: reasoning_asm.finish(),
            finish_reason,
            usage,
        },
        errored,
        stopped,
    })
}

/// Run one **auxiliary, tool-less** model turn for the context compactor
/// (SOUL §7): stream it against a private no-op observer — its deltas must NOT
/// reach the caller's observer, the summary is loop machinery rather than
/// answer text — and return the collected fold. A stream-level error or a
/// cancel mid-turn is an `Err`; the compactor fails open on it.
pub(crate) async fn summarize_turn<S>(
    streamer: &S,
    request: ChatRequest,
    cancel: &CancellationToken,
) -> Result<CollectedTurn>
where
    S: TurnStreamer + ?Sized,
{
    let mut observer = NoopObserver;
    let turn = stream_turn(streamer, request, cancel, &mut observer).await?;
    if turn.errored {
        return Err(catalerum_core::error::Error::provider(
            "compaction summarize stream errored",
        ));
    }
    if turn.stopped {
        return Err(catalerum_core::error::Error::provider(
            "compaction summarize turn cancelled",
        ));
    }
    Ok(turn.collected)
}

/// Dispatch a single tool call, capturing success or error as a string result
/// and timing the dispatch (for the live [`StreamEvent::ToolResult`] + replay).
async fn dispatch_one(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    call: &ToolCall,
    input_modalities: &[String],
) -> ToolInvocation {
    let started = std::time::Instant::now();
    let (result, is_error, media) = run_tool(registry, ctx, call, input_modalities).await;
    ToolInvocation {
        call: call.clone(),
        result,
        is_error,
        duration_ms: started.elapsed().as_millis() as u64,
        media,
    }
}

/// Run a tool call, returning its `(result_string, is_error)`. Split out from
/// [`dispatch_one`] so the latter can stamp a single duration over all paths.
async fn run_tool(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    call: &ToolCall,
    input_modalities: &[String],
) -> (String, bool, Vec<MediaInput>) {
    // Parse the JSON arguments string; treat empty as `{}`.
    let args: Json = if call.arguments.trim().is_empty() {
        Json::Object(serde_json::Map::new())
    } else {
        match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                return (
                    error_result(&format!("invalid tool arguments: {e}")),
                    true,
                    Vec::new(),
                )
            }
        }
    };
    match registry
        .dispatch_for_model(&call.name, args, ctx, input_modalities)
        .await
    {
        Ok(mut value) => {
            let encoded_media = value
                .as_object_mut()
                .and_then(|object| object.remove(MODEL_MEDIA_RESULT_FIELD));
            let media = if input_modalities
                .iter()
                .any(|modality| modality.eq_ignore_ascii_case("image"))
            {
                encoded_media
                    .and_then(|media| serde_json::from_value(media).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            (value_to_string(&value), false, media)
        }
        Err(e) => (error_result(&e.to_string()), true, Vec::new()),
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
mod tests {
    use super::*;
    use async_trait::async_trait;
    use catalerum_core::tool::Tool;
    use std::sync::Arc;

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
        let ctx = ToolContext::default();

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
            run_tool(&registry, &ToolContext::default(), &call, &["image".into()]).await;
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
            &ToolContext::default(),
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
        let ctx = ToolContext::default();
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
            &ToolContext::default(),
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
            &ToolContext::default(),
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
            &ToolContext::default(),
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
            &ToolContext::default(),
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
        let ctx = ToolContext::default();
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
}
