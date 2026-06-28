//! Streams-based work queue (SOUL §6.2 / §6.6 workflow dispatch).
//!
//! A [`WorkQueue`] is a Redis **Stream** read through a **consumer group**:
//! producers [`push`](WorkQueue::push) (XADD), each worker
//! [`pull`](WorkQueue::pull)s with its own consumer name (XREADGROUP, creating
//! the group lazily with MKSTREAM), and [`ack`](WorkQueue::ack)s on success
//! (XACK). Unacked items stay in the group's PEL for redelivery — at-least-once.
//!
//! The in-process backend is a plain FIFO `VecDeque` per stream with an ack set;
//! it has no cross-pod semantics but lets single-pod dev exercise the same API.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use redis::streams::{StreamReadOptions, StreamReadReply};
use redis::AsyncTypedCommands;

use crate::conn::blocking_read_connection;
use crate::error::{BusError, BusResult};
use crate::keys::stream_key;

/// A single item read from a work queue.
///
/// `id` is the stream entry id (e.g. `"1718200000000-0"`); pass it back to
/// [`WorkQueue::ack`]. `payload` is the opaque bytes supplied to `push`
/// (callers typically JSON-encode a job).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkItem {
    /// Stream entry id, used as the ack handle.
    pub id: String,
    /// Opaque job payload.
    pub payload: Vec<u8>,
}

impl WorkItem {
    /// Decode the payload as JSON into `T`.
    pub fn json<T: serde::de::DeserializeOwned>(&self) -> BusResult<T> {
        Ok(serde_json::from_slice(&self.payload)?)
    }
}

/// Field name under which the payload is stored in each stream entry.
const PAYLOAD_FIELD: &str = "p";

/// A consumer-group work queue over a named stream. Object-safe.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Ensure the consumer group `group` exists for `stream` (idempotent;
    /// creates the stream with MKSTREAM if absent). Call once per worker boot.
    /// A newly created group starts at the stream's **beginning**, so a backlog
    /// pushed before any consumer existed (e.g. a mid-turn "say" racing the
    /// run's group creation) is delivered rather than silently skipped — the
    /// same behaviour as the in-process backend.
    async fn ensure_group(&self, stream: &str, group: &str) -> BusResult<()>;

    /// Append a job to `stream` (XADD `*`). Returns the new entry id.
    async fn push(&self, stream: &str, payload: &[u8]) -> BusResult<String>;

    /// Read up to `count` undelivered items for `group`/`consumer`, blocking up
    /// to `block_ms` (XREADGROUP `>`). Returns an empty vec on timeout.
    /// Creates the group lazily if needed.
    async fn pull(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: u64,
    ) -> BusResult<Vec<WorkItem>>;

    /// Acknowledge a processed item (XACK). Returns the number acked (0 or 1).
    async fn ack(&self, stream: &str, group: &str, id: &str) -> BusResult<u64>;
}

// ---------------------------------------------------------------------------
// In-process backend.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StreamState {
    seq: u64,
    ready: VecDeque<WorkItem>,
    pending: HashMap<String, WorkItem>,
}

/// In-process [`WorkQueue`]: a FIFO per stream plus a pending (delivered,
/// unacked) map. Single-pod only; no real consumer-group fan-out, but the API
/// matches so dev workflows compile and run unchanged.
#[derive(Clone)]
pub struct InProcessQueue {
    streams: Arc<Mutex<HashMap<String, StreamState>>>,
}

impl InProcessQueue {
    /// Create an empty in-process queue.
    pub fn new() -> Self {
        Self {
            streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InProcessQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkQueue for InProcessQueue {
    async fn ensure_group(&self, stream: &str, _group: &str) -> BusResult<()> {
        let mut map = self.streams.lock().expect("queue mutex poisoned");
        map.entry(stream.to_string()).or_default();
        Ok(())
    }

    async fn push(&self, stream: &str, payload: &[u8]) -> BusResult<String> {
        let mut map = self.streams.lock().expect("queue mutex poisoned");
        let st = map.entry(stream.to_string()).or_default();
        st.seq += 1;
        let id = format!("{}-0", st.seq);
        st.ready.push_back(WorkItem {
            id: id.clone(),
            payload: payload.to_vec(),
        });
        Ok(id)
    }

    async fn pull(
        &self,
        stream: &str,
        _group: &str,
        _consumer: &str,
        count: usize,
        _block_ms: u64,
    ) -> BusResult<Vec<WorkItem>> {
        let mut map = self.streams.lock().expect("queue mutex poisoned");
        let st = map.entry(stream.to_string()).or_default();
        let n = count.max(1).min(st.ready.len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(item) = st.ready.pop_front() {
                st.pending.insert(item.id.clone(), item.clone());
                out.push(item);
            }
        }
        Ok(out)
    }

    async fn ack(&self, stream: &str, _group: &str, id: &str) -> BusResult<u64> {
        let mut map = self.streams.lock().expect("queue mutex poisoned");
        let st = map.entry(stream.to_string()).or_default();
        Ok(st.pending.remove(id).map(|_| 1).unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------
// Valkey backend.
// ---------------------------------------------------------------------------

/// Valkey-backed [`WorkQueue`] over Redis Streams + consumer groups.
#[derive(Clone)]
pub struct RedisQueue {
    client: Arc<redis::Client>,
    conn: redis::aio::ConnectionManager,
}

impl RedisQueue {
    /// Build from a shared connection manager.
    pub fn new(client: Arc<redis::Client>, conn: redis::aio::ConnectionManager) -> Self {
        Self { client, conn }
    }
}

#[async_trait]
impl WorkQueue for RedisQueue {
    async fn ensure_group(&self, stream: &str, group: &str) -> BusResult<()> {
        let key = stream_key(stream);
        let mut conn = self.conn.clone();
        // `0` = the group sees the stream's full backlog: entries pushed before
        // the group first existed must not be skipped (a work queue is durable;
        // `$` would drop them). MKSTREAM creates the stream if missing.
        match conn.xgroup_create_mkstream(&key, group, "0").await {
            Ok(()) => Ok(()),
            Err(e) if is_busygroup(&e) => Ok(()), // group already exists
            Err(e) => Err(e.into()),
        }
    }

    async fn push(&self, stream: &str, payload: &[u8]) -> BusResult<String> {
        let key = stream_key(stream);
        let mut conn = self.conn.clone();
        let id = conn
            .xadd(&key, "*", &[(PAYLOAD_FIELD, payload)])
            .await?
            .ok_or_else(|| BusError::other("XADD returned no id (NOMKSTREAM?)"))?;
        Ok(id)
    }

    async fn pull(
        &self,
        stream: &str,
        group: &str,
        consumer: &str,
        count: usize,
        block_ms: u64,
    ) -> BusResult<Vec<WorkItem>> {
        let key = stream_key(stream);
        self.ensure_group(stream, group).await?;
        let opts = StreamReadOptions::default()
            .group(group, consumer)
            .count(count.max(1))
            .block(block_ms as usize);
        // XREADGROUP BLOCK holds the Redis connection at the server. Do not run it
        // on the shared command manager or it head-of-line blocks XADD/SET/XACK —
        // and size the response timeout to the block window (the default 500 ms
        // would kill any longer blocking pull mid-block).
        let mut conn = blocking_read_connection(&self.client, block_ms).await?;
        // `>` = messages never delivered to any consumer in this group.
        // Typed `xread_options` returns `None` when BLOCK times out empty.
        let reply: Option<StreamReadReply> = conn.xread_options(&[&key], &[">"], &opts).await?;
        Ok(reply.map(collect_items).unwrap_or_default())
    }

    async fn ack(&self, stream: &str, group: &str, id: &str) -> BusResult<u64> {
        let key = stream_key(stream);
        let mut conn = self.conn.clone();
        let n = conn.xack(&key, group, &[id]).await?;
        Ok(n as u64)
    }
}

/// Flatten an XREADGROUP reply into [`WorkItem`]s, extracting the payload field.
fn collect_items(reply: StreamReadReply) -> Vec<WorkItem> {
    let mut out = Vec::new();
    for skey in reply.keys {
        for entry in skey.ids {
            let payload = entry
                .map
                .get(PAYLOAD_FIELD)
                .and_then(value_to_bytes)
                .unwrap_or_default();
            out.push(WorkItem {
                id: entry.id,
                payload,
            });
        }
    }
    out
}

/// Best-effort extraction of bytes from a redis `Value` (BulkString/SimpleString).
fn value_to_bytes(v: &redis::Value) -> Option<Vec<u8>> {
    match v {
        redis::Value::BulkString(b) => Some(b.clone()),
        redis::Value::SimpleString(s) => Some(s.clone().into_bytes()),
        redis::Value::Int(i) => Some(i.to_string().into_bytes()),
        _ => None,
    }
}

/// True if the error is the `BUSYGROUP` response (group already exists).
fn is_busygroup(e: &redis::RedisError) -> bool {
    e.code() == Some("BUSYGROUP")
}
