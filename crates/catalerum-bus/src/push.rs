//! Best-effort **raw** JSON pub/sub for cross-pod fan-out of transient
//! server→client push (SOUL §6.2/§26).
//!
//! Unlike [`TokenRelay`](crate::TokenRelay) — which carries a typed
//! [`StreamEvent`](catalerum_core::stream::StreamEvent) keyed by a `TurnId` and
//! ends on the terminal `Done` — this is an **untyped byte channel**: the caller
//! owns the payload schema and the channel name, and the stream never terminates
//! on its own (it lives until the publisher socket drops / the process ends).
//!
//! It exists for fire-and-forget push that must reach *every* pod, not a single
//! reader: the MCP `GET /mcp` [`SessionHub`](catalerum_mcp) bridge publishes each
//! per-workspace push here so peers can re-broadcast it into their own local SSE
//! fan-out. Losing a message costs a client one missed notification (latency, not
//! correctness — the push half of MCP streamable-HTTP is best-effort by design),
//! never data, so a cold/absent Valkey is safe: the in-process backend keeps
//! single-pod dev working with no Valkey, exactly like the token relay.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{Stream, StreamExt};
use redis::AsyncTypedCommands;
use tokio::sync::broadcast;

use crate::error::BusResult;

/// A stream of raw payloads received on a subscribed channel.
///
/// Yields `Ok(bytes)` per published message. A lagged (slow) subscriber silently
/// skips the dropped messages — these are best-effort — and the stream ends only
/// when the channel closes (in-process: all senders dropped; Valkey: socket gone).
pub type RawStream = Pin<Box<dyn Stream<Item = BusResult<Vec<u8>>> + Send>>;

/// Publish/subscribe raw byte payloads on a named channel. Object-safe; both
/// backends implement it. The payload schema and channel namespacing are the
/// caller's concern (see [`crate::mcp_push_channel`]).
#[async_trait]
pub trait PushBus: Send + Sync {
    /// Publish one payload to `channel`. Fire-and-forget: delivery to zero
    /// subscribers is success (a push with no audience is simply dropped).
    async fn publish_raw(&self, channel: &str, payload: Vec<u8>) -> BusResult<()>;

    /// Subscribe to `channel`. Subscribe **before** publishing to avoid missing
    /// messages (pub/sub has no backlog). The returned stream is endless until the
    /// channel closes.
    async fn subscribe_raw(&self, channel: &str) -> BusResult<RawStream>;
}

/// Per-channel broadcast buffer (messages) before a slow subscriber begins to lag.
const PUSH_BUFFER: usize = 256;

// ---------------------------------------------------------------------------
// In-process backend (tokio broadcast behind the same trait).
// ---------------------------------------------------------------------------

/// In-process [`PushBus`] backed by one `tokio::sync::broadcast` channel per
/// channel name. The single-pod / no-Valkey default: two handles cloned from the
/// same instance (or two subscribers) share the channel registry and see each
/// other's publishes, so a single process wires end-to-end with no Valkey.
#[derive(Clone, Default)]
pub struct InProcessPush {
    channels: Arc<Mutex<HashMap<String, broadcast::Sender<Vec<u8>>>>>,
}

impl InProcessPush {
    /// Create an empty in-process push bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The broadcast sender for `channel`, created on first use.
    fn sender(&self, channel: &str) -> broadcast::Sender<Vec<u8>> {
        let mut map = self.channels.lock().expect("push mutex poisoned");
        if let Some(tx) = map.get(channel) {
            return tx.clone();
        }
        let tx = broadcast::channel(PUSH_BUFFER).0;
        map.insert(channel.to_string(), tx.clone());
        tx
    }
}

#[async_trait]
impl PushBus for InProcessPush {
    async fn publish_raw(&self, channel: &str, payload: Vec<u8>) -> BusResult<()> {
        // Ignore "no receivers" — a channel with no live subscriber is fine.
        let _ = self.sender(channel).send(payload);
        Ok(())
    }

    async fn subscribe_raw(&self, channel: &str) -> BusResult<RawStream> {
        let rx = self.sender(channel).subscribe();
        let stream = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(bytes) => return Some((Ok(bytes), rx)),
                    // Lagged: dropped some payloads; keep going from the new tail.
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

/// Valkey-backed [`PushBus`] using PUBLISH/SUBSCRIBE, mirroring
/// [`RedisRelay`](crate::RedisRelay): publishing rides the shared
/// [`redis::aio::ConnectionManager`]; each `subscribe_raw` opens a fresh dedicated
/// pub/sub socket via the [`redis::Client`] (a subscribe-mode connection can't run
/// normal commands).
#[derive(Clone)]
pub struct RedisPush {
    client: Arc<redis::Client>,
    conn: redis::aio::ConnectionManager,
}

impl RedisPush {
    /// Build from a shared client (for pub/sub sockets) and a connection manager
    /// (for publishing).
    #[must_use]
    pub fn new(client: Arc<redis::Client>, conn: redis::aio::ConnectionManager) -> Self {
        Self { client, conn }
    }
}

#[async_trait]
impl PushBus for RedisPush {
    async fn publish_raw(&self, channel: &str, payload: Vec<u8>) -> BusResult<()> {
        let mut conn = self.conn.clone();
        conn.publish(channel, payload).await?;
        Ok(())
    }

    async fn subscribe_raw(&self, channel: &str) -> BusResult<RawStream> {
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(channel).await?;
        let stream = pubsub.into_on_message().map(raw_payload);
        Ok(Box::pin(stream))
    }
}

/// Extract a pub/sub message's raw payload bytes.
fn raw_payload(msg: redis::Msg) -> BusResult<Vec<u8>> {
    Ok(msg.get_payload_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_reaches_a_subscriber_once() {
        let bus = InProcessPush::new();
        let mut sub = bus.subscribe_raw("chan").await.unwrap();
        bus.publish_raw("chan", b"hello".to_vec()).await.unwrap();
        assert_eq!(sub.next().await.unwrap().unwrap(), b"hello");
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_ok() {
        let bus = InProcessPush::new();
        // No panic, no error — a push with no audience is dropped.
        bus.publish_raw("chan", b"x".to_vec()).await.unwrap();
    }

    #[tokio::test]
    async fn a_publish_is_scoped_to_its_channel() {
        let bus = InProcessPush::new();
        let mut other = bus.subscribe_raw("other").await.unwrap();
        bus.publish_raw("chan", b"x".to_vec()).await.unwrap();
        // The subscriber on a different channel receives nothing (proven by a
        // second publish on its own channel arriving first).
        bus.publish_raw("other", b"y".to_vec()).await.unwrap();
        assert_eq!(other.next().await.unwrap().unwrap(), b"y");
    }

    #[tokio::test]
    async fn two_handles_share_the_registry() {
        // Cloned handles (the shape the Bus enum uses) see each other's publishes.
        let a = InProcessPush::new();
        let b = a.clone();
        let mut sub = b.subscribe_raw("chan").await.unwrap();
        a.publish_raw("chan", b"via-a".to_vec()).await.unwrap();
        assert_eq!(sub.next().await.unwrap().unwrap(), b"via-a");
    }
}
