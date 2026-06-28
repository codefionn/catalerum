//! Ephemeral TTL'd service-discovery registry (SOUL §6.6 / §16 M7).
//!
//! Pods **announce** themselves under a key (`cat:pod:{pod_id}` → reachable
//! address, see [`crate::pod_key`]) with a TTL and re-announce on their heartbeat
//! clock; peers **look up** an announcement to route pod-local work (a terminal
//! session's PTY lives only on the pod that opened it) to its owner. Like every
//! bus role this is *never* a source of truth: a missed announcement costs a
//! routable request (the caller degrades to its precise "owner unreachable"
//! error), never data. The in-process backend keeps single-pod dev working with
//! no Valkey, where a lookup of any *other* pod simply misses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use redis::{AsyncTypedCommands, SetExpiry, SetOptions};

use crate::error::BusResult;

/// TTL'd announce/lookup key-value registry. Object-safe; both backends
/// implement it. Key namespacing is the caller's concern (see [`crate::pod_key`]).
#[async_trait]
pub trait Registry: Send + Sync {
    /// Announce `value` under `key` for `ttl`, overwriting any previous
    /// announcement. Refresh by re-announcing before the TTL lapses.
    async fn announce(&self, key: &str, value: Vec<u8>, ttl: Duration) -> BusResult<()>;

    /// Look up a still-live announcement. `None` once the TTL has lapsed or the
    /// key was never announced / was withdrawn.
    async fn lookup(&self, key: &str) -> BusResult<Option<Vec<u8>>>;

    /// Withdraw an announcement early (idempotent; the TTL handles crashes).
    async fn withdraw(&self, key: &str) -> BusResult<()>;
}

// ---------------------------------------------------------------------------
// In-process backend.
// ---------------------------------------------------------------------------

struct Announced {
    value: Vec<u8>,
    expires_at: Instant,
}

/// In-process [`Registry`] backed by a map with wall-clock TTLs — the single-pod
/// / no-Valkey default, mirroring [`InProcessLock`](crate::InProcessLock).
#[derive(Clone, Default)]
pub struct InProcessRegistry {
    entries: Arc<Mutex<HashMap<String, Announced>>>,
}

impl InProcessRegistry {
    /// Create an empty in-process registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Registry for InProcessRegistry {
    async fn announce(&self, key: &str, value: Vec<u8>, ttl: Duration) -> BusResult<()> {
        let mut map = self.entries.lock().expect("registry mutex poisoned");
        map.insert(
            key.to_string(),
            Announced {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
        Ok(())
    }

    async fn lookup(&self, key: &str) -> BusResult<Option<Vec<u8>>> {
        let mut map = self.entries.lock().expect("registry mutex poisoned");
        match map.get(key) {
            Some(e) if e.expires_at > Instant::now() => Ok(Some(e.value.clone())),
            Some(_) => {
                // Lapsed — drop it so the map doesn't accumulate dead entries.
                map.remove(key);
                Ok(None)
            }
            None => Ok(None),
        }
    }

    async fn withdraw(&self, key: &str) -> BusResult<()> {
        let mut map = self.entries.lock().expect("registry mutex poisoned");
        map.remove(key);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Valkey backend (SET PX / GET / DEL).
// ---------------------------------------------------------------------------

/// Valkey-backed [`Registry`]: `SET key value PX ttl` announcements, plain `GET`
/// lookups (Valkey expires the key itself), `DEL` withdrawal.
#[derive(Clone)]
pub struct RedisRegistry {
    conn: redis::aio::ConnectionManager,
}

impl RedisRegistry {
    /// Build from a shared connection manager.
    #[must_use]
    pub fn new(conn: redis::aio::ConnectionManager) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl Registry for RedisRegistry {
    async fn announce(&self, key: &str, value: Vec<u8>, ttl: Duration) -> BusResult<()> {
        let opts = SetOptions::default().with_expiration(SetExpiry::PX(ttl.as_millis() as u64));
        let mut conn = self.conn.clone();
        conn.set_options(key, value, opts).await?;
        Ok(())
    }

    async fn lookup(&self, key: &str) -> BusResult<Option<Vec<u8>>> {
        let mut conn = self.conn.clone();
        // Typed GET returns Option<String>; announcements are small JSON, but go
        // through the untyped query to keep arbitrary bytes intact.
        let value: Option<Vec<u8>> = redis::cmd("GET").arg(key).query_async(&mut conn).await?;
        Ok(value)
    }

    async fn withdraw(&self, key: &str) -> BusResult<()> {
        let mut conn = self.conn.clone();
        conn.del(key).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn announce_lookup_withdraw_roundtrip() {
        let reg = InProcessRegistry::new();
        reg.announce(
            "cat:pod:a",
            b"10.0.0.1:8787".to_vec(),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        assert_eq!(
            reg.lookup("cat:pod:a").await.unwrap().unwrap(),
            b"10.0.0.1:8787"
        );
        reg.withdraw("cat:pod:a").await.unwrap();
        assert!(reg.lookup("cat:pod:a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn lookup_misses_after_ttl() {
        let reg = InProcessRegistry::new();
        reg.announce("cat:pod:a", b"x".to_vec(), Duration::from_millis(10))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(reg.lookup("cat:pod:a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn reannounce_overwrites_and_extends() {
        let reg = InProcessRegistry::new();
        reg.announce("k", b"old".to_vec(), Duration::from_millis(10))
            .await
            .unwrap();
        reg.announce("k", b"new".to_vec(), Duration::from_secs(30))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(reg.lookup("k").await.unwrap().unwrap(), b"new");
    }

    #[tokio::test]
    async fn unknown_key_misses() {
        let reg = InProcessRegistry::new();
        assert!(reg.lookup("nope").await.unwrap().is_none());
        // Withdrawing something never announced is a quiet no-op.
        reg.withdraw("nope").await.unwrap();
    }
}
