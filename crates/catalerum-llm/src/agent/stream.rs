//! Streaming one model turn through an observer, and the tool-less summarize
//! turn the context compactor runs.

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use catalerum_core::error::Result;
use catalerum_core::llm::ChatRequest;
use catalerum_core::stream::StreamEvent;

use super::{NoopObserver, TurnObserver, TurnStreamer};
use crate::client::{CollectedTurn, ReasoningAssembler, ToolCallAssembler};

/// One streamed model turn, collected while forwarding each event to an observer.
pub(super) struct StreamedTurn {
    /// The folded turn (text, assembled tool calls, finish reason, usage).
    pub(super) collected: CollectedTurn,
    /// True if the stream surfaced a terminal [`StreamEvent::Error`].
    pub(super) errored: bool,
    /// True if the turn was cut short by the caller's cancel token (the user's
    /// Stop): `collected` holds whatever had streamed by then.
    pub(super) stopped: bool,
}

/// Drive a single turn's [`StreamEvent`] stream: forward every event to
/// `observer` (for live relay) and fold it into a [`CollectedTurn`]. The
/// streaming analogue of [`crate::client::collect_turn`]. Cancelling `cancel`
/// abandons the stream mid-turn (dropping it aborts the underlying request) and
/// returns the partial fold flagged `stopped`.
pub(super) async fn stream_turn<S, O>(
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
