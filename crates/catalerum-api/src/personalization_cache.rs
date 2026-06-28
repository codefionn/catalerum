//! Sole-user personalization cache (SOUL §18/§29).
//!
//! Resolves the §29 "sole-user caches" open question. Investigation of the chat
//! turn's per-turn personalization (SOUL §22, built in `routes/ws.rs`) found that
//! of the inputs injected into the system prompt only the **user profile
//! snapshot** is message-independent and therefore safe to cache across
//! consecutive turns:
//!
//! - **profile** — `store.profiles().get(ws, user)`, a single indexed Postgres
//!   row (`PRIMARY KEY (workspace_id, user_id)`); identical turn to turn until the
//!   user edits it. **Cacheable.**
//! - **recall** — `recall_memory_texts(query = user_msg.content)` *embeds the
//!   current message* (an embedding-API round-trip + a Qdrant vector search), so
//!   it is **inherently per-message** and can never be cached across turns. This
//!   is the only expensive personalization input; caching cannot help it.
//!
//! So the cacheable surface is the profile alone, and the read it saves is one
//! indexed row — the win is modest. This primitive exists to *resolve* the open
//! question with a correct, mode-gated cache, not because the read is a
//! bottleneck (see the §29 annotation for the honest measurement).
//!
//! **Invalidation is a generation bump, not TTL-only.** A workspace-scoped,
//! in-memory generation counter is bumped by every profile write. A cache entry
//! records the generation it was built under; a mismatch forces a rebuild. A
//! modest TTL backstop ([`DEFAULT_TTL`]) covers any write path that forgot to
//! bump — belt and braces.
//!
//! Memory writes deliberately do **not** bump: nothing memory-derived is cached
//! (recall is per-message and always freshly computed), so a `remember` / dedup
//! `touch` / `refine` cannot stale any cache entry here.
//!
//! **Mode gate + flip.** The cache is consulted only when the boot-time
//! deployment mode is `single_user` (one human; process-local, so there is no
//! cross-pod coherence concern by definition — the noted assumption). Flipping to
//! `multi_user` is a config edit + restart (SOUL §18): the restarted process
//! never consults the cache (and its in-memory map starts empty) — that *is* the
//! flip-invalidation story.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use catalerum_core::model::Profile;
use catalerum_core::{UserId, WorkspaceId};

/// TTL backstop for a cached profile snapshot (SOUL §29): even absent a
/// generation bump, an entry is rebuilt after this long.
pub(crate) const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// One cached profile snapshot plus the metadata that decides its freshness.
struct Entry {
    profile: Profile,
    /// The workspace generation this snapshot was built under.
    generation: u64,
    /// When it was stored (for the TTL backstop).
    stored_at: Instant,
}

/// The sole-user personalization cache: a per-workspace generation counter plus
/// per-`(workspace, user)` profile snapshots. In-memory, process-local.
///
/// All methods are cheap and synchronous (short critical sections); the store
/// read that fills the cache happens in [`crate::state::AppState::cached_profile`]
/// *outside* any lock. Lock order is never nested: [`Self::get`] reads (and
/// releases) the generation counter *before* locking the entry map, and
/// [`Self::put`] takes a caller-captured generation so it locks only the entry
/// map — so `bump` (generations only), `get`, and `put` cannot deadlock.
pub(crate) struct PersonalizationCache {
    /// Per-workspace generation counter; bumped by every profile write. Absent =
    /// generation 0.
    generations: Mutex<HashMap<WorkspaceId, u64>>,
    /// Cached profile snapshots keyed by `(workspace, user)`.
    entries: Mutex<HashMap<(WorkspaceId, UserId), Entry>>,
    ttl: Duration,
}

impl PersonalizationCache {
    /// A cache with the given TTL backstop.
    #[must_use]
    pub(crate) fn new(ttl: Duration) -> Self {
        Self {
            generations: Mutex::new(HashMap::new()),
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    /// The current generation for a workspace (0 if never bumped).
    #[must_use]
    pub(crate) fn generation(&self, ws: WorkspaceId) -> u64 {
        *self.generations.lock().unwrap().get(&ws).unwrap_or(&0)
    }

    /// Bump a workspace's generation — every cached profile in it is now stale and
    /// will be rebuilt on next read. Cheap, so callers bump liberally on any write
    /// that could change a profile.
    pub(crate) fn bump(&self, ws: WorkspaceId) {
        let mut g = self.generations.lock().unwrap();
        *g.entry(ws).or_insert(0) += 1;
    }

    /// Return the cached profile if it is still fresh at `now`: present, built
    /// under the *current* generation, and within the TTL. Otherwise `None` — the
    /// caller reads through and [`Self::put`]s the result.
    #[must_use]
    pub(crate) fn get(&self, ws: WorkspaceId, user: UserId, now: Instant) -> Option<Profile> {
        // Read (and release) the generation counter *before* locking entries so
        // the two locks are never held nested.
        let current = self.generation(ws);
        let entries = self.entries.lock().unwrap();
        let e = entries.get(&(ws, user))?;
        if e.generation != current {
            return None;
        }
        if now.saturating_duration_since(e.stored_at) >= self.ttl {
            return None;
        }
        Some(e.profile.clone())
    }

    /// Store a freshly-read profile under `generation` — which the caller captures
    /// **before** the read, so a bump that races the read stamps the entry with the
    /// now-stale generation and the next [`Self::get`] rebuilds (no stale serve).
    pub(crate) fn put(
        &self,
        ws: WorkspaceId,
        user: UserId,
        profile: Profile,
        generation: u64,
        now: Instant,
    ) {
        self.entries.lock().unwrap().insert(
            (ws, user),
            Entry {
                profile,
                generation,
                stored_at: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::model::Map;
    use uuid::Uuid;

    fn ws(n: u128) -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(n))
    }
    fn user(n: u128) -> UserId {
        UserId::from_uuid(Uuid::from_u128(n))
    }
    fn profile(w: WorkspaceId, u: UserId, tag: &str) -> Profile {
        let mut fields = Map::new();
        fields.insert(
            "tag".to_string(),
            serde_json::Value::String(tag.to_string()),
        );
        Profile {
            workspace_id: w,
            user_id: u,
            fields,
        }
    }

    #[test]
    fn generation_starts_at_zero_and_bumps_per_workspace() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        let (a, b) = (ws(1), ws(2));
        assert_eq!(c.generation(a), 0);
        c.bump(a);
        assert_eq!(c.generation(a), 1);
        // A bump is workspace-scoped: b is untouched.
        assert_eq!(c.generation(b), 0);
        c.bump(a);
        assert_eq!(c.generation(a), 2);
    }

    #[test]
    fn hit_returns_the_exact_bytes_that_were_put() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        let (w, u) = (ws(1), user(1));
        let now = Instant::now();
        let p = profile(w, u, "hello");
        c.put(w, u, p.clone(), c.generation(w), now);
        // Same generation, within TTL → hit, and byte-identical to the source
        // (no behavioral change to the injected content).
        assert_eq!(c.get(w, u, now), Some(p));
    }

    #[test]
    fn miss_when_absent() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        assert_eq!(c.get(ws(1), user(1), Instant::now()), None);
    }

    #[test]
    fn miss_on_generation_mismatch_after_a_write() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        let (w, u) = (ws(1), user(1));
        let now = Instant::now();
        c.put(w, u, profile(w, u, "old"), c.generation(w), now);
        assert!(c.get(w, u, now).is_some());
        // A profile write bumps the workspace generation → the entry is stale.
        c.bump(w);
        assert_eq!(c.get(w, u, now), None);
    }

    #[test]
    fn miss_on_ttl_expiry() {
        let ttl = Duration::from_secs(60);
        let c = PersonalizationCache::new(ttl);
        let (w, u) = (ws(1), user(1));
        let t0 = Instant::now();
        c.put(w, u, profile(w, u, "x"), c.generation(w), t0);
        // Fresh at t0, and just under the TTL.
        assert!(c.get(w, u, t0).is_some());
        assert!(c.get(w, u, t0 + Duration::from_secs(59)).is_some());
        // At/after the TTL → rebuild.
        assert_eq!(c.get(w, u, t0 + ttl), None);
        assert_eq!(c.get(w, u, t0 + ttl + Duration::from_secs(1)), None);
    }

    #[test]
    fn put_stamps_the_captured_generation_so_a_racing_bump_invalidates_the_fill() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        let (w, u) = (ws(1), user(1));
        let now = Instant::now();
        // Simulate: capture generation (0) before the read, a write bumps it to 1
        // mid-read, then we store the (now-stale) snapshot under the captured 0.
        let captured = c.generation(w);
        c.bump(w);
        c.put(w, u, profile(w, u, "stale"), captured, now);
        // The next read sees generation 1 != stored 0 → rebuild, never a stale serve.
        assert_eq!(c.get(w, u, now), None);
    }

    #[test]
    fn per_user_isolation_within_a_workspace() {
        let c = PersonalizationCache::new(DEFAULT_TTL);
        let w = ws(1);
        let (u1, u2) = (user(1), user(2));
        let now = Instant::now();
        c.put(w, u1, profile(w, u1, "one"), c.generation(w), now);
        assert!(c.get(w, u1, now).is_some());
        // A different user in the same workspace has no entry yet.
        assert_eq!(c.get(w, u2, now), None);
    }
}
