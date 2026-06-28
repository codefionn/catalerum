//! catalerum-bus — Valkey ephemeral coordination & streaming: HA coordination
//! (locks, leader-election, cache invalidation), per-turn token-stream relay,
//! and Streams workflow dispatch. None of it is a source of truth (SOUL §6.6).
//!
//! Everything here is **rebuildable or transient**: a cold Valkey costs latency,
//! never data. The crate exposes four roles, each behind an object-safe trait
//! with two interchangeable backends:
//!
//! * [`TokenRelay`] — per-turn LLM token-stream pub/sub (§7 cross-pod relay).
//! * [`PushBus`] — best-effort raw byte pub/sub for cross-pod server→client push
//!   fan-out (§26 MCP `GET /mcp` SSE hub bridge).
//! * [`WorkQueue`] — a Redis Streams consumer-group work queue (XADD / XREADGROUP / XACK).
//! * [`DistLock`] — a `SET NX PX` + fenced-token distributed lock with safe release.
//!
//! Each role has a **Valkey-backed** implementation (`redis::aio::ConnectionManager`)
//! and an **in-process fallback** (tokio broadcast / in-memory maps) so a single
//! dev pod runs without Valkey. This is what M1 chat uses by default.
//!
//! The [`Bus`] enum is the top-level handle that bundles all three; construct it
//! with [`Bus::in_process`] for dev or [`Bus::connect`] for a real Valkey URL.

#![forbid(unsafe_code)]

mod conn;
mod error;
mod keys;
mod lock;
mod push;
mod queue;
mod registry;
mod relay;
mod turnbuf;

pub use error::{BusError, BusResult};
pub use keys::{
    conv_ctl_channel, conv_input_stream, lock_key, mcp_push_channel, pod_key, stream_key,
    turn_buffer_key, turn_channel, TurnId,
};
pub use lock::{DistLock, InProcessLock, LockGuard, RedisLock};
pub use push::{InProcessPush, PushBus, RawStream, RedisPush};
pub use queue::{InProcessQueue, RedisQueue, WorkItem, WorkQueue};
pub use registry::{InProcessRegistry, RedisRegistry, Registry};
pub use relay::{InProcessRelay, RedisRelay, TokenRelay, TokenStream};
pub use turnbuf::{
    InProcessTurnBuffer, RedisTurnBuffer, TurnBuffer, TurnEntry, TURN_BUFFER_MAXLEN,
    TURN_BUFFER_TTL_SECS,
};

use std::sync::Arc;

use catalerum_core::stream::StreamEvent;

/// A unified handle over the three bus roles, in either backend.
///
/// Clone is cheap (everything inside is `Arc`-shared). Use [`Bus::in_process`]
/// for single-pod dev (M1 default), or [`Bus::connect`] to attach to Valkey.
#[derive(Clone)]
pub enum Bus {
    /// In-process fallback: tokio broadcast relay + in-memory queue/locks.
    InProcess(Arc<InProcessBus>),
    /// Valkey-backed: pub/sub relay, Streams queue, `SET NX PX` locks.
    Redis(Arc<RedisBus>),
}

/// In-process implementations of all roles, sharing one set of maps.
pub struct InProcessBus {
    relay: InProcessRelay,
    push: InProcessPush,
    queue: InProcessQueue,
    lock: InProcessLock,
    registry: InProcessRegistry,
    turnbuf: InProcessTurnBuffer,
}

/// Valkey-backed implementations, sharing one `ConnectionManager` and `Client`.
/// Blocking stream reads open dedicated connections so they cannot head-of-line
/// block ordinary commands on the shared manager.
pub struct RedisBus {
    relay: RedisRelay,
    push: RedisPush,
    queue: RedisQueue,
    lock: RedisLock,
    registry: RedisRegistry,
    turnbuf: RedisTurnBuffer,
}

impl Bus {
    /// Build the in-process bus used by single-pod dev / M1 chat. Never fails.
    pub fn in_process() -> Self {
        Bus::InProcess(Arc::new(InProcessBus {
            relay: InProcessRelay::new(),
            push: InProcessPush::new(),
            queue: InProcessQueue::new(),
            lock: InProcessLock::new(),
            registry: InProcessRegistry::new(),
            turnbuf: InProcessTurnBuffer::new(),
        }))
    }

    /// Connect to a Valkey/Redis instance (e.g. `redis://127.0.0.1:6379`).
    ///
    /// Opens a multiplexed [`redis::aio::ConnectionManager`] (auto-reconnecting)
    /// for commands and keeps the [`redis::Client`] for fresh pub/sub sockets.
    pub async fn connect(url: impl AsRef<str>) -> BusResult<Self> {
        let client = redis::Client::open(url.as_ref())?;
        // The manager carries every non-blocking command (XADD appends, locks,
        // registry announces). redis 1.x defaults its response timeout to a
        // twitchy 500 ms — one hiccup and a load-bearing append (e.g. a turn's
        // terminal frame) is dropped. Bound it generously instead; blocking
        // stream reads never ride this manager (see `conn`).
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_response_timeout(Some(std::time::Duration::from_secs(5)));
        let manager = client.get_connection_manager_with_config(config).await?;
        let client = Arc::new(client);
        Ok(Bus::Redis(Arc::new(RedisBus {
            relay: RedisRelay::new(client.clone(), manager.clone()),
            push: RedisPush::new(client.clone(), manager.clone()),
            queue: RedisQueue::new(client.clone(), manager.clone()),
            lock: RedisLock::new(manager.clone()),
            registry: RedisRegistry::new(manager.clone()),
            turnbuf: RedisTurnBuffer::new(client, manager),
        })))
    }

    /// Access the [`TokenRelay`] role.
    pub fn relay(&self) -> &dyn TokenRelay {
        match self {
            Bus::InProcess(b) => &b.relay,
            Bus::Redis(b) => &b.relay,
        }
    }

    /// Access the [`PushBus`] role (best-effort raw cross-pod push fan-out).
    pub fn push(&self) -> &dyn PushBus {
        match self {
            Bus::InProcess(b) => &b.push,
            Bus::Redis(b) => &b.push,
        }
    }

    /// Access the [`WorkQueue`] role.
    pub fn queue(&self) -> &dyn WorkQueue {
        match self {
            Bus::InProcess(b) => &b.queue,
            Bus::Redis(b) => &b.queue,
        }
    }

    /// Access the [`DistLock`] role.
    pub fn lock(&self) -> &dyn DistLock {
        match self {
            Bus::InProcess(b) => &b.lock,
            Bus::Redis(b) => &b.lock,
        }
    }

    /// Access the [`Registry`] role (TTL'd service-discovery announcements).
    pub fn registry(&self) -> &dyn Registry {
        match self {
            Bus::InProcess(b) => &b.registry,
            Bus::Redis(b) => &b.registry,
        }
    }

    /// Access the [`TurnBuffer`] role (per-turn replayable frame log).
    pub fn turnbuf(&self) -> &dyn TurnBuffer {
        match self {
            Bus::InProcess(b) => &b.turnbuf,
            Bus::Redis(b) => &b.turnbuf,
        }
    }

    /// `true` if this bus talks to a real Valkey instance.
    pub fn is_distributed(&self) -> bool {
        matches!(self, Bus::Redis(_))
    }
}

/// Convenience: publish one [`StreamEvent`] delta for a turn over this bus.
///
/// Equivalent to `self.relay().publish_delta(turn, ev).await`; provided so the
/// API/LLM layer can write `bus.publish_delta(...)` without naming the trait.
impl Bus {
    /// Publish a single token-stream delta to a turn's channel.
    pub async fn publish_delta(&self, turn: &TurnId, event: &StreamEvent) -> BusResult<()> {
        self.relay().publish_delta(turn, event).await
    }

    /// Subscribe to a turn's token stream.
    pub async fn subscribe(&self, turn: &TurnId) -> BusResult<TokenStream> {
        self.relay().subscribe(turn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::stream::{FinishReason, StreamEvent};
    use catalerum_core::{ConversationId, MessageId};
    use futures::StreamExt;
    use std::time::Duration;

    fn turn() -> TurnId {
        TurnId::new(ConversationId::new(), MessageId::new())
    }

    #[tokio::test]
    async fn relay_streams_deltas_then_done() {
        let bus = Bus::in_process();
        let t = turn();
        let mut sub = bus.subscribe(&t).await.unwrap();

        bus.publish_delta(&t, &StreamEvent::TextDelta { text: "he".into() })
            .await
            .unwrap();
        bus.publish_delta(&t, &StreamEvent::TextDelta { text: "llo".into() })
            .await
            .unwrap();
        bus.publish_delta(
            &t,
            &StreamEvent::Done {
                finish_reason: Some(FinishReason::Stop),
                usage: None,
            },
        )
        .await
        .unwrap();

        let mut texts = String::new();
        let mut saw_done = false;
        while let Some(ev) = sub.next().await {
            match ev.unwrap() {
                StreamEvent::TextDelta { text } => texts.push_str(&text),
                StreamEvent::Done { .. } => {
                    saw_done = true;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(texts, "hello");
        assert!(saw_done);
        // Stream completes after Done.
        assert!(sub.next().await.is_none());
    }

    #[tokio::test]
    async fn relay_broadcasts_to_multiple_subscribers() {
        // Two clients watching the same turn (e.g. the conversation open in two
        // tabs) must each receive every delta and the Done, and each stream ends —
        // the relay uses a broadcast channel precisely to fan out to all of them.
        let bus = Bus::in_process();
        let t = turn();
        let mut a = bus.subscribe(&t).await.unwrap();
        let mut b = bus.subscribe(&t).await.unwrap();

        bus.publish_delta(&t, &StreamEvent::TextDelta { text: "hi".into() })
            .await
            .unwrap();
        bus.publish_delta(
            &t,
            &StreamEvent::Done {
                finish_reason: Some(FinishReason::Stop),
                usage: None,
            },
        )
        .await
        .unwrap();

        for sub in [&mut a, &mut b] {
            assert!(
                matches!(
                    sub.next().await.unwrap().unwrap(),
                    StreamEvent::TextDelta { .. }
                ),
                "each subscriber gets the delta"
            );
            assert!(
                matches!(sub.next().await.unwrap().unwrap(), StreamEvent::Done { .. }),
                "each subscriber gets the done"
            );
            assert!(sub.next().await.is_none(), "each stream ends after Done");
        }
    }

    #[tokio::test]
    async fn turnbuf_replays_from_offset_and_fans_out() {
        // A detached run appends frames; two readers (originating socket + a
        // reconnecting tab) each replay from their own cursor. Reader A reads from
        // the start; reader B resumes after A's second frame and sees only the tail.
        let bus = Bus::in_process();
        let buf = bus.turnbuf();
        let t = turn();

        let id0 = buf.append(&t, b"f0").await.unwrap();
        let _id1 = buf.append(&t, b"f1").await.unwrap();
        let id2 = buf.append(&t, b"f2").await.unwrap();

        // Full replay from "0" sees every frame in order.
        let all = buf.read(&t, "0", 50).await.unwrap();
        assert_eq!(
            all.iter().map(|e| e.payload.clone()).collect::<Vec<_>>(),
            vec![b"f0".to_vec(), b"f1".to_vec(), b"f2".to_vec()],
        );
        assert_eq!(all[0].id, id0);

        // Resume after the second frame's id → only the tail.
        let tail = buf.read(&t, &all[1].id, 50).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].payload, b"f2");
        assert_eq!(tail[0].id, id2);

        // A cursor at the head yields nothing (times out empty).
        let none = buf.read(&t, &id2, 20).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn turnbuf_blocks_then_wakes_on_append() {
        // A reader caught up to the tail blocks; a later append wakes it — this is
        // what keeps the forwarder streaming live rather than busy-polling.
        let bus = Bus::in_process();
        let t = turn();
        let writer = bus.clone();
        let w = writer.turnbuf();
        // Establish the buffer + cursor.
        let id0 = w.append(&t, b"first").await.unwrap();

        let reader = tokio::spawn(async move { bus.turnbuf().read(&t, &id0, 1000).await.unwrap() });
        // Give the reader a moment to block, then append.
        tokio::time::sleep(Duration::from_millis(20)).await;
        w.append(&t, b"second").await.unwrap();

        let got = reader.await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, b"second");
    }

    #[tokio::test]
    async fn queue_push_pull_ack_roundtrip() {
        let bus = Bus::in_process();
        let q = bus.queue();
        q.ensure_group("jobs", "g1").await.unwrap();
        let id = q.push("jobs", b"payload-1").await.unwrap();

        let items = q.pull("jobs", "g1", "c1", 10, 0).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].payload, b"payload-1");
        assert_eq!(items[0].id, id);

        assert_eq!(q.ack("jobs", "g1", &id).await.unwrap(), 1);
        // Second ack is a no-op.
        assert_eq!(q.ack("jobs", "g1", &id).await.unwrap(), 0);
        // Nothing left ready.
        assert!(q.pull("jobs", "g1", "c1", 10, 0).await.unwrap().is_empty());
    }

    /// A job pushed BEFORE the consumer group first exists is still delivered —
    /// the group starts at the stream's beginning (Redis: XGROUP CREATE at `0`),
    /// so e.g. a mid-turn "say" racing the run's group creation isn't lost.
    #[tokio::test]
    async fn queue_delivers_backlog_pushed_before_group_creation() {
        let bus = Bus::in_process();
        let q = bus.queue();
        let id = q.push("early", b"pre-group").await.unwrap();
        q.ensure_group("early", "g1").await.unwrap();
        let items = q.pull("early", "g1", "c1", 10, 0).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
    }

    #[tokio::test]
    async fn lock_acquire_excludes_and_releases() {
        let bus = Bus::in_process();
        let lock = bus.lock();
        let g = lock
            .try_acquire("leader", Duration::from_secs(30))
            .await
            .unwrap()
            .expect("first acquire succeeds");
        // Contended acquire fails while held.
        assert!(lock
            .try_acquire("leader", Duration::from_secs(30))
            .await
            .unwrap()
            .is_none());
        // Refresh keeps it.
        assert!(lock.refresh(&g, Duration::from_secs(30)).await.unwrap());
        // Release with the token works once.
        assert!(lock.release(&g).await.unwrap());
        assert!(!lock.release(&g).await.unwrap());
        // Now re-acquirable.
        assert!(lock
            .try_acquire("leader", Duration::from_secs(30))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn lock_expires_after_ttl() {
        let bus = Bus::in_process();
        let lock = bus.lock();
        let _g = lock
            .try_acquire("short", Duration::from_millis(10))
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(lock
            .try_acquire("short", Duration::from_secs(1))
            .await
            .unwrap()
            .is_some());
    }

    /// Token-safety across the expiry→re-acquire race (the property the CAS-release
    /// provides): a stale guard whose lock expired and was re-acquired by another
    /// holder must NOT release/refresh the *new* holder's lock — otherwise a third
    /// caller could acquire and double-fire the occurrence (SOUL §11/§6.2).
    #[tokio::test]
    async fn stale_guard_cannot_release_or_refresh_a_reacquired_lock() {
        let bus = Bus::in_process();
        let lock = bus.lock();
        // A acquires with a short TTL; it then expires.
        let stale = lock
            .try_acquire("occ", Duration::from_millis(10))
            .await
            .unwrap()
            .expect("A acquires");
        tokio::time::sleep(Duration::from_millis(20)).await;
        // B re-acquires the now-expired lock.
        let fresh = lock
            .try_acquire("occ", Duration::from_secs(30))
            .await
            .unwrap()
            .expect("B re-acquires after expiry");
        // A's stale guard must not free or extend B's lock (token mismatch).
        assert!(
            !lock.release(&stale).await.unwrap(),
            "a stale guard must not release the re-acquired lock"
        );
        assert!(
            !lock.refresh(&stale, Duration::from_secs(30)).await.unwrap(),
            "a stale guard must not refresh the re-acquired lock"
        );
        // B still holds it: a contended acquire still fails, and B can release it.
        assert!(lock
            .try_acquire("occ", Duration::from_secs(30))
            .await
            .unwrap()
            .is_none());
        assert!(
            lock.release(&fresh).await.unwrap(),
            "B releases with its own token"
        );
    }
}
