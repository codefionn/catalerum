//! Distributed lock (SOUL §6.6 HA coordination: locks, leader-election).
//!
//! Acquire with `SET key token NX PX <ttl>`; the random `token` fences the
//! holder. Release runs a CAS Lua script (`GET == token` then `DEL`) so a holder
//! whose TTL already lapsed — and whose lock was re-acquired by someone else —
//! never deletes the new owner's lock. This is the standard single-instance
//! Redis lock; it is a *coordination hint*, never a correctness oracle, exactly
//! as §6.6 requires.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::{AsyncTypedCommands, ExistenceCheck, SetExpiry, SetOptions};

use crate::error::BusResult;
use crate::keys::lock_key;

/// A held distributed lock. Carries the resource name and the fencing token; on
/// drop it is **not** auto-released (release is async) — call
/// [`DistLock::release`] explicitly, or let the TTL expire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockGuard {
    /// Logical resource name (unprefixed) this lock protects.
    pub resource: String,
    /// Random fencing token proving ownership; required to release/refresh.
    pub token: String,
}

impl LockGuard {
    /// The fencing token, monotonic-ish per acquisition; pass to fenced writes.
    pub fn token(&self) -> &str {
        &self.token
    }
}

/// A best-effort distributed mutex. Object-safe.
#[async_trait]
pub trait DistLock: Send + Sync {
    /// Try to acquire `resource` for `ttl`. Returns `Some(guard)` if acquired,
    /// `None` if currently held by someone else.
    async fn try_acquire(&self, resource: &str, ttl: Duration) -> BusResult<Option<LockGuard>>;

    /// Release a held lock iff the token still matches (CAS). Returns `true` if
    /// this call actually deleted the lock, `false` if it had already been lost.
    async fn release(&self, guard: &LockGuard) -> BusResult<bool>;

    /// Extend a still-held lock's TTL iff the token matches. Returns `true` on
    /// success, `false` if the lock was lost.
    async fn refresh(&self, guard: &LockGuard, ttl: Duration) -> BusResult<bool>;
}

/// Generate a random fencing token (UUID v4 hex).
fn new_token() -> String {
    catalerum_core::id::MessageId::new()
        .as_uuid()
        .simple()
        .to_string()
}

// ---------------------------------------------------------------------------
// In-process backend.
// ---------------------------------------------------------------------------

struct Held {
    token: String,
    expires_at: Instant,
}

/// In-process [`DistLock`] backed by a map with wall-clock TTLs. Correct within
/// one process; gives single-pod dev the same acquire/release/refresh API.
#[derive(Clone)]
pub struct InProcessLock {
    locks: Arc<Mutex<HashMap<String, Held>>>,
}

impl InProcessLock {
    /// Create an empty in-process lock table.
    pub fn new() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InProcessLock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DistLock for InProcessLock {
    async fn try_acquire(&self, resource: &str, ttl: Duration) -> BusResult<Option<LockGuard>> {
        let mut map = self.locks.lock().expect("lock mutex poisoned");
        let now = Instant::now();
        if let Some(held) = map.get(resource) {
            if held.expires_at > now {
                return Ok(None); // still held by someone
            }
        }
        let token = new_token();
        map.insert(
            resource.to_string(),
            Held {
                token: token.clone(),
                expires_at: now + ttl,
            },
        );
        Ok(Some(LockGuard {
            resource: resource.to_string(),
            token,
        }))
    }

    async fn release(&self, guard: &LockGuard) -> BusResult<bool> {
        let mut map = self.locks.lock().expect("lock mutex poisoned");
        match map.get(&guard.resource) {
            Some(held) if held.token == guard.token => {
                map.remove(&guard.resource);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn refresh(&self, guard: &LockGuard, ttl: Duration) -> BusResult<bool> {
        let mut map = self.locks.lock().expect("lock mutex poisoned");
        match map.get_mut(&guard.resource) {
            Some(held) if held.token == guard.token => {
                held.expires_at = Instant::now() + ttl;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Valkey backend.
// ---------------------------------------------------------------------------

/// CAS release: delete only if the stored value matches our token.
const RELEASE_LUA: &str = r#"
if redis.call("GET", KEYS[1]) == ARGV[1] then
    return redis.call("DEL", KEYS[1])
else
    return 0
end
"#;

/// CAS refresh: re-set the PX TTL only if the stored value matches our token.
const REFRESH_LUA: &str = r#"
if redis.call("GET", KEYS[1]) == ARGV[1] then
    return redis.call("PEXPIRE", KEYS[1], ARGV[2])
else
    return 0
end
"#;

/// Valkey-backed [`DistLock`] using `SET NX PX` + a CAS-release Lua script.
#[derive(Clone)]
pub struct RedisLock {
    conn: redis::aio::ConnectionManager,
}

impl RedisLock {
    /// Build from a shared connection manager.
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl DistLock for RedisLock {
    async fn try_acquire(&self, resource: &str, ttl: Duration) -> BusResult<Option<LockGuard>> {
        let key = lock_key(resource);
        let token = new_token();
        let opts = SetOptions::default()
            .conditional_set(ExistenceCheck::NX)
            .with_expiration(SetExpiry::PX(ttl.as_millis() as u64));
        let mut conn = self.conn.clone();
        // `set_options` returns the old value or None; with NX a successful set
        // returns `Some("OK")`-equivalent and a failed one returns `None`.
        let res: Option<String> = conn.set_options(&key, &token, opts).await?;
        match res {
            Some(_) => Ok(Some(LockGuard {
                resource: resource.to_string(),
                token,
            })),
            None => Ok(None),
        }
    }

    async fn release(&self, guard: &LockGuard) -> BusResult<bool> {
        let key = lock_key(&guard.resource);
        let script = redis::Script::new(RELEASE_LUA);
        let mut conn = self.conn.clone();
        let deleted: i64 = script
            .key(key)
            .arg(&guard.token)
            .invoke_async(&mut conn)
            .await?;
        Ok(deleted == 1)
    }

    async fn refresh(&self, guard: &LockGuard, ttl: Duration) -> BusResult<bool> {
        let key = lock_key(&guard.resource);
        let script = redis::Script::new(REFRESH_LUA);
        let mut conn = self.conn.clone();
        let ok: i64 = script
            .key(key)
            .arg(&guard.token)
            .arg(ttl.as_millis() as u64)
            .invoke_async(&mut conn)
            .await?;
        Ok(ok == 1)
    }
}
