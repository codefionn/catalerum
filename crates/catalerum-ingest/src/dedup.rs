//! The memory-store dedup seam (SOUL §22/§29): one place every memory write
//! passes through so the same fact is never stored twice.
//!
//! The auto-store path (a chat that proactively `remember`s durable facts) plus
//! the explicit `remember` tool and the `POST /memories` route all re-state the
//! *same* facts across conversations — that is exactly where duplicates breed.
//! [`store_memory_deduped`] is the shared entry point, in two escalating layers:
//!
//! 1. **Heuristic** (always, no embedding): normalize the candidate (trim,
//!    casefold, collapse whitespace) and compare it against the workspace's
//!    existing memories in the *same visibility class*. An exact match — or a
//!    match where either text wholly contains the other as a run of whole words —
//!    is resolved without ever calling the embedder:
//!    - the candidate adds nothing → **skip the insert, touch** the existing row's
//!      `updated_at` (recency reaffirm) and return it as [`MemoryStoreStatus::Deduplicated`];
//!    - the candidate *strictly extends* an existing memory → **update** that row's
//!      text (a refinement, not a dup) → [`MemoryStoreStatus::Refined`].
//! 2. **Similarity** (only when a [`MemoryDedupIndex`] is supplied, i.e. a vector
//!    backend is configured): embed the candidate and search its near-neighbours
//!    among the workspace's embedded memories. A neighbour at or above
//!    [`MEMORY_DEDUP_SIMILARITY_THRESHOLD`] is treated as a duplicate (same
//!    skip+touch), unless the candidate is a strict superset of it (then refine).
//!    **Best-effort**: if this layer errors (embedder or vector backend down),
//!    the write proceeds heuristic-only rather than failing — see below.
//!
//! When neither layer fires the candidate is a genuinely new fact →
//! [`MemoryStoreStatus::Stored`] (created + enqueued for embedding).
//!
//! **Conservative by design.** A dropped *new* fact is worse than a stored
//! near-duplicate, so the threshold is deliberately high and dedup is confined to
//! the candidate's own scope+owner class — a user re-remembering a shared fact
//! privately is allowed to keep its private copy rather than risk swallowing it or
//! silently editing shared state. LLM-assisted judging of borderline pairs is the
//! recorded escalation path (§29), not done here.

use std::collections::HashMap;

use catalerum_core::embed::EmbeddingRequest;
use catalerum_core::model::{Memory, MemoryScope};
use catalerum_core::provider::Embedder;
use catalerum_core::{MemoryId, SourceRef, UserId, WorkspaceId};
use catalerum_store::Store;
use catalerum_vector::{SearchFilter, SearchQuery, VectorStore};

use crate::embed::enqueue_ingest_memory;
use crate::error::Result;

/// Cosine-similarity cutoff at/above which an embedded near-neighbour memory is
/// treated as a duplicate of the candidate.
///
/// The vector index scores with cosine similarity (Qdrant `Cosine`), so this is a
/// value in `[-1, 1]` where `1.0` is an identical direction. **0.95** is
/// intentionally high: at that similarity two short fact strings are
/// near-paraphrases ("prefers black tea" vs "likes black tea"), not merely related
/// ("prefers tea" vs "prefers coffee" sits far below). The task is explicit that a
/// *dropped new fact* is worse than a *stored near-duplicate*, so we favour
/// false-negatives — a slightly-too-low threshold would swallow genuinely new
/// facts, which we never want; a slightly-too-high one just stores an occasional
/// near-dup that the heuristic exact/superset layer already catches for the common
/// re-remember case. It is a named constant (not config) because the memory domain
/// exposes no tuning surface today; wiring it to config is deferred to when one
/// exists (§29).
pub const MEMORY_DEDUP_SIMILARITY_THRESHOLD: f32 = 0.95;

/// How many existing memories to scan for the heuristic exact/superset pass — the
/// same bound the auto-curator already used for its exact-dedup prefetch.
const DEDUP_SCAN_LIMIT: i64 = 500;

/// How many near-neighbours to fetch for the similarity pass. We only need the
/// single best *same-class, still-present* hit; the small over-fetch is headroom
/// for visibility/class filtering (a hit whose row is another user's or is gone).
const DEDUP_NEIGHBOUR_SCAN: u64 = 10;

/// The similarity-layer dependencies, borrowed for one [`store_memory_deduped`]
/// call: an [`Embedder`] to vectorise the candidate and the [`VectorStore`] to
/// search. Built from the caller's already-configured search backend (the API's
/// `SemanticSearch` or a worker's [`EmbedContext`](crate::EmbedContext)); its
/// presence *is* the "a vector backend is configured" signal, so it also gates
/// whether a stored/refined memory is enqueued for (re-)embedding.
pub struct MemoryDedupIndex<'a> {
    /// Embeds the candidate text for the near-neighbour search.
    pub embedder: &'a dyn Embedder,
    /// The derived vector index searched for near-duplicate memories.
    pub vector: &'a VectorStore,
    /// The embedding model to vectorise with (must match the index's).
    pub embed_model: &'a str,
}

/// What [`store_memory_deduped`] did with a candidate — the observability signal
/// callers surface (`stored` / `deduplicated` / `refined`) so the model (and
/// tests) can tell a new fact from an already-known one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryStoreStatus {
    /// A genuinely new fact — a row was created (and enqueued for embedding).
    Stored,
    /// A duplicate of an existing memory — no row created; the existing row was
    /// touched (recency reaffirm) and returned.
    Deduplicated,
    /// A strict extension of an existing memory — that row's text was updated in
    /// place (and re-embedded); no new row.
    Refined,
}

impl MemoryStoreStatus {
    /// The stable wire token for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryStoreStatus::Stored => "stored",
            MemoryStoreStatus::Deduplicated => "deduplicated",
            MemoryStoreStatus::Refined => "refined",
        }
    }
}

/// The result of a dedup-aware store: the resolved memory (newly created, the
/// touched duplicate, or the refined row) plus what happened to it.
#[derive(Clone, Debug)]
pub struct MemoryStoreOutcome {
    /// The memory the caller should treat as the outcome of its write.
    pub memory: Memory,
    /// Whether it was newly stored, deduplicated, or refined.
    pub status: MemoryStoreStatus,
}

/// Normalize a memory's text for dedup comparison: trim, collapse internal runs of
/// whitespace to single spaces, and casefold. Pure — the heuristic layer's basis
/// and the superset check's input.
#[must_use]
pub fn normalize_memory_text(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// How a normalized candidate relates to a normalized existing memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DedupRelation {
    /// Unrelated — neither contains the other.
    Distinct,
    /// The candidate adds nothing: identical, or the existing memory already
    /// wholly contains the candidate. Skip the insert.
    Duplicate,
    /// The candidate strictly extends the existing memory (contains it as a run of
    /// whole words and is longer). Refine the existing memory.
    Refinement,
}

/// Classify a normalized candidate against a normalized existing memory (pure).
fn dedup_relation(candidate_norm: &str, existing_norm: &str) -> DedupRelation {
    if candidate_norm == existing_norm {
        return DedupRelation::Duplicate;
    }
    // The candidate is longer and wholly contains the existing fact → a refinement.
    if is_whole_word_superset(candidate_norm, existing_norm) {
        return DedupRelation::Refinement;
    }
    // The existing fact already wholly contains the candidate → nothing new.
    if is_whole_word_superset(existing_norm, candidate_norm) {
        return DedupRelation::Duplicate;
    }
    DedupRelation::Distinct
}

/// True when `longer` strictly contains `shorter` as a run of **whole words**
/// (both normalized: single-spaced, casefolded). Whole-word so "cat" is not a
/// superset of "category"; strict so identical texts are handled as `Duplicate`,
/// not here. Pure.
fn is_whole_word_superset(longer: &str, shorter: &str) -> bool {
    if shorter.is_empty() || longer.len() <= shorter.len() {
        return false;
    }
    // Pad both with spaces so containment matches only at word boundaries.
    let hay = format!(" {longer} ");
    let needle = format!(" {shorter} ");
    hay.contains(&needle)
}

/// Whether an existing memory falls in the same dedup class as a candidate — the
/// candidate's own scope, and (for `User` scope) the same owning member. Dedup and
/// refine only ever touch same-class rows, so a private `remember` never edits or
/// is swallowed by shared workspace state (and vice versa).
fn same_class(scope: MemoryScope, user_id: Option<UserId>, existing: &Memory) -> bool {
    match scope {
        MemoryScope::Workspace => matches!(existing.scope, MemoryScope::Workspace),
        MemoryScope::User => {
            matches!(existing.scope, MemoryScope::User) && existing.user_id == user_id
        }
    }
}

/// Store `text` as a memory, deduplicating against the workspace's existing
/// memories (SOUL §22/§29). The single seam shared by the `remember` tool, the
/// `POST /memories` route, and the background auto-curator — see the module docs
/// for the layered heuristic + similarity design.
///
/// `index` enables the embedding-similarity layer and gates embed-enqueue: pass
/// `Some` when a vector backend is configured, `None` for heuristic-only. `scope`
/// / `user_id` set the new memory's visibility (and the dedup class); `source`
/// records provenance on a freshly stored memory.
pub async fn store_memory_deduped(
    store: &Store,
    index: Option<&MemoryDedupIndex<'_>>,
    workspace_id: WorkspaceId,
    scope: MemoryScope,
    user_id: Option<UserId>,
    text: &str,
    source: Option<&SourceRef>,
) -> Result<MemoryStoreOutcome> {
    let candidate = text.trim();
    let cand_norm = normalize_memory_text(candidate);

    // The candidate's dedup class: its own scope, and for `User` scope its owner.
    // `list_visible(_, None, _)` returns only workspace memories; with a user it
    // also returns that user's private ones, which `same_class` then narrows to.
    let visible_user = match scope {
        MemoryScope::User => user_id,
        MemoryScope::Workspace => None,
    };
    let existing = store
        .memories()
        .list_visible(workspace_id, visible_user, DEDUP_SCAN_LIMIT)
        .await?;

    // --- Layer 1: heuristic (no embedding) -------------------------------------
    // A `Duplicate` (exact / already-covered) wins over a `Refinement`, so scan for
    // one first and only refine if none is found.
    let mut refine_target: Option<MemoryId> = None;
    for m in &existing {
        if !same_class(scope, user_id, m) {
            continue;
        }
        match dedup_relation(&cand_norm, &normalize_memory_text(&m.text)) {
            DedupRelation::Duplicate => {
                return deduplicate(store, workspace_id, m.id).await;
            }
            DedupRelation::Refinement if refine_target.is_none() => {
                refine_target = Some(m.id);
            }
            _ => {}
        }
    }
    if let Some(id) = refine_target {
        return refine(store, index, workspace_id, id, candidate).await;
    }

    // --- Layer 2: embedding similarity (best-effort) ----------------------------
    // A similarity-layer failure (embedder or vector backend down/misconfigured)
    // must never block the write: a dropped new fact is worse than a stored
    // near-duplicate (module docs), so degrade to heuristic-only and store.
    if let Some(idx) = index {
        match nearest_same_class_memory(store, idx, workspace_id, scope, user_id, candidate).await {
            Ok(Some(hit)) => {
                // A near-neighbour above the threshold: refine if the candidate strictly
                // extends it, else it is a duplicate (skip + touch).
                return match dedup_relation(&cand_norm, &normalize_memory_text(&hit.text)) {
                    DedupRelation::Refinement => {
                        refine(store, index, workspace_id, hit.id, candidate).await
                    }
                    _ => deduplicate(store, workspace_id, hit.id).await,
                };
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "memory dedup similarity layer failed; storing without it"
                );
            }
        }
    }

    // --- Neither layer fired: a genuinely new fact -----------------------------
    let memory = store
        .memories()
        .create(workspace_id, scope, user_id, candidate, source)
        .await?;
    // Embed it so semantic recall can surface it (best-effort; only when a vector
    // backend is configured — else the job would have no worker to serve it).
    if index.is_some() {
        enqueue_ingest_memory(store, workspace_id, memory.id).await?;
    }
    Ok(MemoryStoreOutcome {
        memory,
        status: MemoryStoreStatus::Stored,
    })
}

/// Skip an insert that duplicates `id`: touch the row (recency reaffirm) and
/// return it as [`MemoryStoreStatus::Deduplicated`].
async fn deduplicate(
    store: &Store,
    workspace_id: WorkspaceId,
    id: MemoryId,
) -> Result<MemoryStoreOutcome> {
    let memory = store.memories().touch(workspace_id, id).await?;
    Ok(MemoryStoreOutcome {
        memory,
        status: MemoryStoreStatus::Deduplicated,
    })
}

/// Replace `id`'s text with the (strictly extending) candidate and re-embed, so
/// Postgres and the vector index stay in sync. Returns [`MemoryStoreStatus::Refined`].
async fn refine(
    store: &Store,
    index: Option<&MemoryDedupIndex<'_>>,
    workspace_id: WorkspaceId,
    id: MemoryId,
    text: &str,
) -> Result<MemoryStoreOutcome> {
    let memory = store.memories().update_text(workspace_id, id, text).await?;
    if index.is_some() {
        enqueue_ingest_memory(store, workspace_id, id).await?;
    }
    Ok(MemoryStoreOutcome {
        memory,
        status: MemoryStoreStatus::Refined,
    })
}

/// The nearest embedded memory to `text` that is in the candidate's dedup class
/// and scores at/above [`MEMORY_DEDUP_SIMILARITY_THRESHOLD`], or `None`. The
/// index's score threshold does the cutoff; we then resolve rows (visibility is
/// not encoded in the vector, §22) and keep the best same-class one.
async fn nearest_same_class_memory(
    store: &Store,
    idx: &MemoryDedupIndex<'_>,
    workspace_id: WorkspaceId,
    scope: MemoryScope,
    user_id: Option<UserId>,
    text: &str,
) -> Result<Option<Memory>> {
    let resp = idx
        .embedder
        .embed(EmbeddingRequest::single(idx.embed_model, text))
        .await?;
    let Some(vector) = resp.embeddings.into_iter().next().map(|e| e.vector) else {
        return Ok(None);
    };
    let query = SearchQuery::new(vector, DEDUP_NEIGHBOUR_SCAN)
        .with_filter(SearchFilter {
            kinds: vec!["memory".to_string()],
            ..Default::default()
        })
        .with_score_threshold(MEMORY_DEDUP_SIMILARITY_THRESHOLD);
    let hits = idx.vector.search(workspace_id, &query).await?;

    // Resolve every hit's memory row in one batch (the vector carries no
    // visibility, §22), then take the highest-scored same-class survivor. Qdrant
    // returns hits in descending score, so the first same-class match is the best.
    let ids: Vec<MemoryId> = hits
        .iter()
        .filter_map(|h| match &h.payload.source {
            SourceRef::Memory { id } => Some(*id),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return Ok(None);
    }
    let by_id: HashMap<MemoryId, Memory> = store
        .memories()
        .get_many(workspace_id, &ids)
        .await?
        .into_iter()
        .map(|m| (m.id, m))
        .collect();
    for h in &hits {
        if let SourceRef::Memory { id } = &h.payload.source {
            if let Some(m) = by_id.get(id) {
                if same_class(scope, user_id, m) {
                    return Ok(Some(m.clone()));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace_and_casefolds() {
        assert_eq!(normalize_memory_text("  Prefers   TEA  "), "prefers tea");
        assert_eq!(normalize_memory_text("a b"), normalize_memory_text("A   B"));
        assert_eq!(
            normalize_memory_text("Works\tin\nBerlin"),
            "works in berlin"
        );
        assert_eq!(normalize_memory_text(""), "");
    }

    #[test]
    fn whole_word_superset_is_strict_and_boundary_aware() {
        // Strict superset at word boundaries.
        assert!(is_whole_word_superset(
            "works in berlin as an engineer",
            "works in berlin"
        ));
        assert!(is_whole_word_superset("i really like tea", "like tea"));
        // Equal → not a superset (handled as Duplicate elsewhere).
        assert!(!is_whole_word_superset("like tea", "like tea"));
        // Not a whole-word containment: "cat" inside "category".
        assert!(!is_whole_word_superset("category theory", "cat"));
        // Shorter can't be a superset of longer.
        assert!(!is_whole_word_superset("like tea", "i really like tea"));
        // Empty needle is never a superset.
        assert!(!is_whole_word_superset("anything", ""));
    }

    #[test]
    fn dedup_relation_classifies_exact_superset_subset_and_distinct() {
        // Exact (post-normalize) → Duplicate.
        assert_eq!(
            dedup_relation("prefers tea", "prefers tea"),
            DedupRelation::Duplicate
        );
        // Candidate strictly extends existing → Refinement.
        assert_eq!(
            dedup_relation("works in berlin as an engineer", "works in berlin"),
            DedupRelation::Refinement
        );
        // Existing already covers the candidate → Duplicate (nothing new).
        assert_eq!(
            dedup_relation("works in berlin", "works in berlin as an engineer"),
            DedupRelation::Duplicate
        );
        // Unrelated facts → Distinct.
        assert_eq!(
            dedup_relation("prefers tea", "works in berlin"),
            DedupRelation::Distinct
        );
        // Overlapping words but neither contains the other → Distinct.
        assert_eq!(
            dedup_relation("likes green tea", "prefers black tea"),
            DedupRelation::Distinct
        );
    }

    #[test]
    fn same_class_confines_dedup_to_scope_and_owner() {
        let alice = UserId::new();
        let bob = UserId::new();
        let ws = WorkspaceId::new();
        let now = chrono::Utc::now();
        let mk = |scope, uid| Memory {
            id: MemoryId::new(),
            workspace_id: ws,
            scope,
            user_id: uid,
            text: "x".to_string(),
            source: None,
            point_id: None,
            created_at: now,
        };
        // Workspace candidate matches only workspace memories.
        let ws_mem = mk(MemoryScope::Workspace, None);
        let alice_mem = mk(MemoryScope::User, Some(alice));
        assert!(same_class(MemoryScope::Workspace, None, &ws_mem));
        assert!(!same_class(MemoryScope::Workspace, None, &alice_mem));
        // A user candidate matches only that user's private memories.
        assert!(same_class(MemoryScope::User, Some(alice), &alice_mem));
        assert!(!same_class(MemoryScope::User, Some(alice), &ws_mem));
        assert!(!same_class(MemoryScope::User, Some(bob), &alice_mem));
    }
}
