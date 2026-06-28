//! Per-turn LLM token-stream relay (SOUL §7).
//!
//! The pod *generating* tokens calls [`TokenRelay::publish_delta`] for each
//! [`StreamEvent`]; the pod holding the client WebSocket calls
//! [`TokenRelay::subscribe`] and forwards events. With Valkey this works across
//! pods; the in-process backend makes single-pod dev (M1) work with no Valkey.
//!
//! Payloads are JSON-encoded `StreamEvent`s. Every turn ends with exactly one
//! [`StreamEvent::Done`] (core contract); the [`TokenStream`] surfaces it and
//! then completes.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use catalerum_core::stream::StreamEvent;
use futures::stream::{Stream, StreamExt};
use redis::AsyncTypedCommands;
use tokio::sync::broadcast;

use crate::error::BusResult;
use crate::keys::{turn_channel, TurnId};

/// A stream of decoded [`StreamEvent`]s for one turn.
///
/// Yields `Ok(StreamEvent)` per delta. Ends after the terminal
/// [`StreamEvent::Done`] is delivered (or when the publisher drops, for the
/// in-process backend). Decode failures surface as `Err(BusError::Serde)`.
pub type TokenStream = Pin<Box<dyn Stream<Item = BusResult<StreamEvent>> + Send>>;

/// Relay LLM token deltas for a turn between the generating pod and the pod
/// holding the client connection. Object-safe; both backends implement it.
#[async_trait]
pub trait TokenRelay: Send + Sync {
    /// Publish one streaming event to the turn's channel.
    async fn publish_delta(&self, turn: &TurnId, event: &StreamEvent) -> BusResult<()>;

    /// Subscribe to a turn's token stream. Subscribe **before** the first
    /// `publish_delta` to avoid missing the opening deltas (pub/sub has no
    /// backlog; for replay-from-offset use the Streams [`crate::WorkQueue`]).
    async fn subscribe(&self, turn: &TurnId) -> BusResult<TokenStream>;
}

// ---------------------------------------------------------------------------
// In-process backend (tokio broadcast behind the same trait).
// ---------------------------------------------------------------------------

/// Default per-turn broadcast buffer (events). Generous: a turn is short-lived.
const RELAY_BUFFER: usize = 1024;

/// In-process [`TokenRelay`] backed by a `tokio::sync::broadcast` channel per
/// active turn. This is the M1 default — no Valkey required.
///
/// Channels are created on first publish or subscribe and reaped when the
/// terminal `Done` event flows (or when all handles drop).
#[derive(Clone)]
pub struct InProcessRelay {
    turns: Arc<Mutex<HashMap<TurnId, broadcast::Sender<StreamEvent>>>>,
}

impl InProcessRelay {
    /// Create an empty in-process relay.
    pub fn new() -> Self {
        Self {
            turns: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn sender(&self, turn: &TurnId) -> broadcast::Sender<StreamEvent> {
        let mut map = self.turns.lock().expect("relay mutex poisoned");
        map.entry(*turn)
            .or_insert_with(|| broadcast::channel(RELAY_BUFFER).0)
            .clone()
    }

    fn reap(&self, turn: &TurnId) {
        let mut map = self.turns.lock().expect("relay mutex poisoned");
        map.remove(turn);
    }
}

impl Default for InProcessRelay {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TokenRelay for InProcessRelay {
    async fn publish_delta(&self, turn: &TurnId, event: &StreamEvent) -> BusResult<()> {
        let tx = self.sender(turn);
        // Ignore "no receivers" — a turn with no live subscriber is fine.
        let _ = tx.send(event.clone());
        if matches!(event, StreamEvent::Done { .. }) {
            self.reap(turn);
        }
        Ok(())
    }

    async fn subscribe(&self, turn: &TurnId) -> BusResult<TokenStream> {
        let rx = self.sender(turn).subscribe();
        let stream = futures::stream::unfold(Some(rx), |state| async move {
            let mut rx = state?;
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let done = matches!(ev, StreamEvent::Done { .. });
                        let next = if done { None } else { Some(rx) };
                        return Some((Ok(ev), next));
                    }
                    // Lagged: dropped some events; keep going from the new tail.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

// ---------------------------------------------------------------------------
// Valkey backend (PUBLISH / SUBSCRIBE).
// ---------------------------------------------------------------------------

/// Valkey-backed [`TokenRelay`] using PUBLISH/SUBSCRIBE.
///
/// Publishing uses the shared [`redis::aio::ConnectionManager`]; each
/// `subscribe` opens a fresh dedicated pub/sub socket via the [`redis::Client`]
/// (a connection in subscribe mode can't run normal commands).
#[derive(Clone)]
pub struct RedisRelay {
    client: Arc<redis::Client>,
    conn: redis::aio::ConnectionManager,
}

impl RedisRelay {
    /// Build from a shared client (for pub/sub sockets) and a connection
    /// manager (for publishing).
    pub fn new(client: Arc<redis::Client>, conn: redis::aio::ConnectionManager) -> Self {
        Self { client, conn }
    }
}

#[async_trait]
impl TokenRelay for RedisRelay {
    async fn publish_delta(&self, turn: &TurnId, event: &StreamEvent) -> BusResult<()> {
        let channel = turn_channel(turn);
        let payload = serde_json::to_vec(event)?;
        let mut conn = self.conn.clone();
        conn.publish(channel, payload).await?;
        Ok(())
    }

    async fn subscribe(&self, turn: &TurnId) -> BusResult<TokenStream> {
        let channel = turn_channel(turn);
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(&channel).await?;
        // Decode each message; end the stream right after the terminal Done.
        let stream = pubsub.into_on_message().map(decode_msg);
        Ok(Box::pin(done_terminated(stream)))
    }
}

/// Decode a pub/sub message payload into a `StreamEvent`.
fn decode_msg(msg: redis::Msg) -> BusResult<StreamEvent> {
    let bytes = msg.get_payload_bytes();
    Ok(serde_json::from_slice(bytes)?)
}

/// Wrap a stream so it ends right after the first `Done` event is yielded.
fn done_terminated<S>(inner: S) -> impl Stream<Item = BusResult<StreamEvent>> + Send
where
    S: Stream<Item = BusResult<StreamEvent>> + Send,
{
    futures::stream::unfold(
        (Box::pin(inner), false),
        |(mut inner, finished)| async move {
            if finished {
                return None;
            }
            match inner.next().await {
                Some(item) => {
                    let done = matches!(item, Ok(StreamEvent::Done { .. }));
                    Some((item, (inner, done)))
                }
                None => None,
            }
        },
    )
}
