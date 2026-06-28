//! Per-turn **replayable** frame buffer (SOUL §7/§12: detached chat streaming).
//!
//! Unlike the pub/sub [`crate::TokenRelay`] (no backlog), a [`TurnBuffer`] is a
//! Redis **Stream** used as a fan-out, replay-from-offset log for exactly one
//! turn: the pod running the detached agent loop [`append`](TurnBuffer::append)s
//! each serialized `ServerFrame` (XADD, capped + TTL'd), and any pod holding a
//! client socket [`read`](TurnBuffer::read)s from a caller-supplied cursor
//! (XREAD BLOCK), forwarding frames to the socket. Because the stream lives in
//! Valkey it is inherently cross-pod, and because it retains entries a client
//! can reconnect and resume from its last-seen id with **zero gap** — the
//! properties bare pub/sub can't give.
//!
//! Payloads are **opaque bytes** (the API layer JSON-encodes its `ServerFrame`),
//! so the bus stays decoupled from api types — same discipline as
//! [`crate::WorkQueue`]. Nothing here is a source of truth: the buffer is
//! throwaway (TTL'd) and a cold Valkey degrades to a Postgres refetch (§6.6).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::streams::{StreamMaxlen, StreamReadOptions, StreamReadReply};
use redis::AsyncTypedCommands;
use tokio::sync::watch;

use crate::conn::blocking_read_connection;
use crate::error::BusResult;
use crate::keys::{turn_buffer_key, TurnId};

/// Field name under which the payload is stored in each stream entry.
const PAYLOAD_FIELD: &str = "p";

/// Approximate cap on retained entries per turn. Token deltas are tiny; a turn
/// is short-lived, so this bounds a pathological long turn without ever trimming
/// a normal one. Trimmed prefixes are backstopped by Postgres (completed rounds
/// persist as `messages` rows).
pub const TURN_BUFFER_MAXLEN: usize = 10_000;

/// Time-to-live (seconds) refreshed on every append: the buffer lives exactly as
/// long as the turn plus a short reconnect grace, then evaporates on its own.
pub const TURN_BUFFER_TTL_SECS: i64 = 300;

/// How many entries a single [`TurnBuffer::read`] batches at most.
const READ_COUNT: usize = 512;

/// One buffered frame: its stream entry id (the wire cursor) + opaque payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnEntry {
    /// Stream entry id (e.g. `"1718200000000-0"`); pass back as `after` to resume.
    pub id: String,
    /// Opaque frame bytes supplied to [`TurnBuffer::append`].
    pub payload: Vec<u8>,
}

/// A per-turn replayable frame log. Object-safe; both backends implement it.
#[async_trait]
pub trait TurnBuffer: Send + Sync {
    /// Append one frame to the turn's buffer (XADD, MAXLEN-capped + TTL-refreshed).
    /// Returns the new entry id.
    async fn append(&self, turn: &TurnId, payload: &[u8]) -> BusResult<String>;

    /// Read frames with an id strictly greater than `after`, blocking up to
    /// `block_ms` for new ones. Pass `after = "0"` for a full replay from the
    /// start of the (retained) buffer. Returns an empty vec on timeout so the
    /// caller can loop.
    async fn read(&self, turn: &TurnId, after: &str, block_ms: u64) -> BusResult<Vec<TurnEntry>>;

    /// Drop every retained frame for `turn` (DEL / map removal). Called at the
    /// start of a run that **reuses** a turn key — a regenerate re-answers its
    /// anchor user message under the same key — so a forwarder replaying from
    /// `"0"` can never see the prior run's stale frames (and terminate early on
    /// its old terminal). A no-op when the buffer doesn't exist.
    async fn reset(&self, turn: &TurnId) -> BusResult<()>;
}

// ---------------------------------------------------------------------------
// In-process backend (replayable Vec + watch, no Valkey).
// ---------------------------------------------------------------------------

struct BufState {
    /// Retained entries `(seq, payload)`, oldest first, capped at MAXLEN.
    entries: Vec<(u64, Vec<u8>)>,
    /// When the buffer was last appended to — the in-process analogue of the
    /// Valkey TTL: buffers idle past [`TURN_BUFFER_TTL_SECS`] are swept.
    last_append: Instant,
}

impl Default for BufState {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            last_append: Instant::now(),
        }
    }
}

struct Buf {
    state: Mutex<BufState>,
    /// Bumped to the latest seq on each append so blocked readers wake.
    tx: watch::Sender<u64>,
}

/// In-process [`TurnBuffer`]: a replayable per-turn `Vec` woken by a `watch`
/// channel. Single-pod dev default — same replay/fan-out semantics as Redis,
/// so a second reader (reconnect / second tab) resumes identically.
#[derive(Clone, Default)]
pub struct InProcessTurnBuffer {
    turns: Arc<Mutex<HashMap<TurnId, Arc<Buf>>>>,
    /// Global monotonic id counter shared by **all** buffers, mirroring Redis's
    /// timestamp-based ids: an entry appended later — to any buffer — always has
    /// a larger id, so a stale cursor from an earlier turn resumes cleanly (from
    /// the start of the newer buffer) instead of silently skipping frames.
    seq: Arc<AtomicU64>,
}

impl InProcessTurnBuffer {
    /// Create an empty in-process buffer set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn buf(&self, turn: &TurnId) -> Arc<Buf> {
        let mut map = self.turns.lock().expect("turnbuf mutex poisoned");
        map.entry(*turn)
            .or_insert_with(|| {
                Arc::new(Buf {
                    state: Mutex::new(BufState::default()),
                    tx: watch::channel(0).0,
                })
            })
            .clone()
    }

    /// Drop buffers idle past `ttl` — the in-process analogue of the Valkey
    /// EXPIRE, so a long-lived single-pod process doesn't retain every turn ever
    /// run. Amortized over appends. A reader blocked on a swept buffer times out
    /// its current `read` and re-fetches a fresh (empty) buffer on the next call.
    fn sweep_expired(&self, ttl: Duration) {
        let mut map = self.turns.lock().expect("turnbuf mutex poisoned");
        map.retain(|_, buf| {
            buf.state
                .lock()
                .expect("turnbuf state poisoned")
                .last_append
                .elapsed()
                <= ttl
        });
    }
}

/// Parse the numeric sequence out of an id like `"7-0"` (or `"0"`). Unparseable
/// → 0, so a garbage cursor replays from the start rather than dropping frames.
fn seq_of(id: &str) -> u64 {
    id.split('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

#[async_trait]
impl TurnBuffer for InProcessTurnBuffer {
    async fn append(&self, turn: &TurnId, payload: &[u8]) -> BusResult<String> {
        // Amortized TTL sweep (before `buf` so an expired-but-reused buffer is
        // recreated fresh rather than kept alive with its stale entries).
        self.sweep_expired(Duration::from_secs(TURN_BUFFER_TTL_SECS as u64));
        let buf = self.buf(turn);
        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut st = buf.state.lock().expect("turnbuf state poisoned");
            st.entries.push((id, payload.to_vec()));
            st.last_append = Instant::now();
            // Approximate MAXLEN: drop the oldest overflow (Postgres backstops it).
            if st.entries.len() > TURN_BUFFER_MAXLEN {
                let drop = st.entries.len() - TURN_BUFFER_MAXLEN;
                st.entries.drain(0..drop);
            }
        }
        // Wake blocked readers (ignore "no receivers").
        let _ = buf.tx.send(id);
        Ok(format!("{id}-0"))
    }

    async fn read(&self, turn: &TurnId, after: &str, block_ms: u64) -> BusResult<Vec<TurnEntry>> {
        let buf = self.buf(turn);
        let after_seq = seq_of(after);
        let mut rx = buf.tx.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(block_ms);
        loop {
            // Mark the current value seen BEFORE checking state, so an append that
            // lands between the check and the wait still fires `changed()`.
            rx.borrow_and_update();
            {
                let st = buf.state.lock().expect("turnbuf state poisoned");
                let out: Vec<TurnEntry> = st
                    .entries
                    .iter()
                    .filter(|(seq, _)| *seq > after_seq)
                    .map(|(seq, payload)| TurnEntry {
                        id: format!("{seq}-0"),
                        payload: payload.clone(),
                    })
                    .collect();
                if !out.is_empty() {
                    return Ok(out);
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(Vec::new());
            }
            match tokio::time::timeout(deadline - now, rx.changed()).await {
                Ok(Ok(())) => continue,
                // Timed out, or the sender dropped — either way, no new frames.
                _ => return Ok(Vec::new()),
            }
        }
    }

    async fn reset(&self, turn: &TurnId) -> BusResult<()> {
        // Drop the whole buffer; ids are globally monotonic, so a reader holding
        // a pre-reset cursor still resumes correctly against the fresh buffer.
        self.turns
            .lock()
            .expect("turnbuf mutex poisoned")
            .remove(turn);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Valkey backend (XADD MAXLEN ~ + EXPIRE / XREAD BLOCK).
// ---------------------------------------------------------------------------

/// Valkey-backed [`TurnBuffer`] over a single Redis Stream per turn.
#[derive(Clone)]
pub struct RedisTurnBuffer {
    client: Arc<redis::Client>,
    conn: redis::aio::ConnectionManager,
}

impl RedisTurnBuffer {
    /// Build from a shared connection manager.
    pub fn new(client: Arc<redis::Client>, conn: redis::aio::ConnectionManager) -> Self {
        Self { client, conn }
    }
}

#[async_trait]
impl TurnBuffer for RedisTurnBuffer {
    async fn append(&self, turn: &TurnId, payload: &[u8]) -> BusResult<String> {
        let key = turn_buffer_key(turn);
        let mut conn = self.conn.clone();
        // XADD key MAXLEN ~ <cap> * p <payload> — approximate trim is cheap and
        // caps a pathological turn without exact-trim overhead. Typed command (as
        // in `WorkQueue`) — the raw `cmd(...).query_async` path proved flaky on the
        // multiplexed manager.
        let id = conn
            .xadd_maxlen(
                &key,
                StreamMaxlen::Approx(TURN_BUFFER_MAXLEN),
                "*",
                &[(PAYLOAD_FIELD, payload)],
            )
            .await?
            .ok_or_else(|| crate::error::BusError::other("XADD returned no id"))?;
        // Refresh the TTL so the key outlives the turn by the grace window and no
        // longer (best-effort; a failed EXPIRE just means the key lives its prior TTL).
        let _ = conn.expire(&key, TURN_BUFFER_TTL_SECS).await;
        Ok(id)
    }

    async fn read(&self, turn: &TurnId, after: &str, block_ms: u64) -> BusResult<Vec<TurnEntry>> {
        let key = turn_buffer_key(turn);
        let opts = StreamReadOptions::default()
            .count(READ_COUNT)
            .block(block_ms as usize);
        // XREAD BLOCK occupies its Redis connection until data arrives or the
        // block expires. Use a dedicated connection per read so active WebSocket
        // forwarders cannot stall appends, locks, registry announces, or queue
        // ops — with a response timeout sized to the block window (the default
        // 500 ms would kill the read mid-block).
        let mut conn = blocking_read_connection(&self.client, block_ms).await?;
        // Plain XREAD (no group): entries with id strictly greater than `after`.
        // `None` when BLOCK times out empty.
        let reply: Option<StreamReadReply> = conn.xread_options(&[&key], &[after], &opts).await?;
        Ok(reply.map(collect_entries).unwrap_or_default())
    }

    async fn reset(&self, turn: &TurnId) -> BusResult<()> {
        let key = turn_buffer_key(turn);
        let mut conn = self.conn.clone();
        // DEL is a no-op on a missing key; new XADD ids are timestamp-based, so
        // a reader's pre-reset cursor still resumes correctly afterwards.
        let _ = conn.del(&key).await?;
        Ok(())
    }
}

/// Flatten an XREAD reply into ordered [`TurnEntry`]s, extracting the payload.
fn collect_entries(reply: StreamReadReply) -> Vec<TurnEntry> {
    let mut out = Vec::new();
    for skey in reply.keys {
        for entry in skey.ids {
            let payload = entry
                .map
                .get(PAYLOAD_FIELD)
                .and_then(value_to_bytes)
                .unwrap_or_default();
            out.push(TurnEntry {
                id: entry.id,
                payload,
            });
        }
    }
    out
}

/// Best-effort extraction of bytes from a redis `Value`.
fn value_to_bytes(v: &redis::Value) -> Option<Vec<u8>> {
    match v {
        redis::Value::BulkString(b) => Some(b.clone()),
        redis::Value::SimpleString(s) => Some(s.clone().into_bytes()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::{ConversationId, MessageId};

    fn turn() -> TurnId {
        TurnId::new(ConversationId::new(), MessageId::new())
    }

    /// `reset` drops a turn's retained frames, so a run reusing the turn key (a
    /// regenerate) starts from an empty buffer — a forwarder replaying from "0"
    /// sees only the new run's frames, never the prior run's stale terminal.
    #[tokio::test]
    async fn reset_clears_retained_frames_and_ids_stay_monotonic() {
        let buf = InProcessTurnBuffer::new();
        let t = turn();
        buf.append(&t, b"old-token").await.unwrap();
        let old_terminal = buf.append(&t, b"old-done").await.unwrap();

        buf.reset(&t).await.unwrap();
        assert!(
            buf.read(&t, "0", 10).await.unwrap().is_empty(),
            "a full replay after reset sees nothing"
        );

        // The re-run's frames get ids beyond the old cursor (global counter), so
        // even a reader that kept a pre-reset cursor resumes onto the new run.
        let new_id = buf.append(&t, b"new-token").await.unwrap();
        assert!(seq_of(&new_id) > seq_of(&old_terminal));
        let resumed = buf.read(&t, &old_terminal, 10).await.unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].payload, b"new-token");
    }

    /// Ids are monotonic ACROSS buffers (like Redis timestamp ids): a stale
    /// cursor from an earlier turn replays a later turn's buffer from the start
    /// instead of skipping its first frames.
    #[tokio::test]
    async fn ids_are_monotonic_across_buffers() {
        let buf = InProcessTurnBuffer::new();
        let (a, b) = (turn(), turn());
        buf.append(&a, b"a1").await.unwrap();
        let a2 = buf.append(&a, b"a2").await.unwrap();
        buf.append(&b, b"b1").await.unwrap();
        buf.append(&b, b"b2").await.unwrap();

        // Resuming buffer B from A's (stale, older) cursor yields all of B.
        let got = buf.read(&b, &a2, 10).await.unwrap();
        assert_eq!(
            got.iter().map(|e| e.payload.clone()).collect::<Vec<_>>(),
            vec![b"b1".to_vec(), b"b2".to_vec()],
        );
    }

    /// Buffers idle past the TTL are swept on a later append — the in-process
    /// analogue of the Valkey EXPIRE, so a long-lived single-pod process doesn't
    /// accumulate every turn ever run.
    #[tokio::test]
    async fn idle_buffers_are_swept_after_ttl() {
        let buf = InProcessTurnBuffer::new();
        let (stale, fresh) = (turn(), turn());
        buf.append(&stale, b"s1").await.unwrap();
        // Backdate the stale buffer's last append past the TTL.
        {
            let map = buf.turns.lock().unwrap();
            map[&stale].state.lock().unwrap().last_append = Instant::now()
                - Duration::from_secs(TURN_BUFFER_TTL_SECS as u64)
                - Duration::from_secs(1);
        }
        // Any append sweeps: the stale buffer is gone, the fresh one retained.
        buf.append(&fresh, b"f1").await.unwrap();
        {
            let map = buf.turns.lock().unwrap();
            assert!(!map.contains_key(&stale), "idle buffer swept");
            assert!(map.contains_key(&fresh), "active buffer retained");
        }
        assert!(buf.read(&stale, "0", 10).await.unwrap().is_empty());
    }
}
