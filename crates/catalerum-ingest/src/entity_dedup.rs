//! The entity-projection dedup seam (SOUL §5/§6.3/§29): one place every entity a
//! note/event/extraction references passes through so the *same* thing is never
//! projected as two graph nodes.
//!
//! This is the **graph-entity half** of the §29 dedup work — the twin of the
//! memory seam [`crate::dedup::store_memory_deduped`], mirroring its philosophy:
//! heuristic first, conservative, favour false-negatives, never a destructive
//! merge we can't defend.
//!
//! # What an entity is here
//! Entities are a **Neo4j-only, derived** projection today (SOUL §6.3) — there is
//! no `entities` table in Postgres, and entities are **not** embedded in Qdrant.
//! An entity is `(workspace, kind, display_name, aliases)` with a stable
//! [`EntityId`]. The only path that mints entities today is note **tags** and
//! event **labels** projected as `:Topic` nodes (see [`crate::graph`]); richer
//! `:Person`/`:Org`/… extraction from bodies is the anticipated escalation (§10).
//!
//! # The heuristic (the only layer that fires today)
//! Normalize the raw name — trim, collapse internal whitespace, casefold — into a
//! **dedup key**, then derive the entity's id deterministically from
//! `(workspace, kind, key)` ([`entity_id`]). Because the id *is* a function of the
//! normalized name **scoped to the kind**, two references that normalize to the
//! same `(kind, key)` resolve to the *same* [`EntityId`], and the idempotent Neo4j
//! `MERGE` folds them into one node — so "Machine Learning", "machine  learning",
//! and " MACHINE LEARNING " are one `:Topic`, while a Person "Mercury" and a Place
//! "Mercury" stay distinct (same normalized name, different kind → different id).
//! Duplicate resolution is therefore **structural**: a caller that resolves a raw
//! name always gets the surviving entity, and any edge it then draws attaches to
//! that survivor. [`resolve_entity`] does this for one name; [`resolve_entities`]
//! deduplicates a whole batch (a note's tags) in one pass, keeping the first-seen
//! display form.
//!
//! # Observability
//! [`project_entity_deduped`] is the write-path entry point that reports whether a
//! resolved entity was newly **created** or **deduplicated** against an existing
//! node ([`EntityStoreStatus`]), the entity twin of the memory seam's `status`.
//!
//! # Conservative by design — the recorded deferrals (§29)
//! - **No fuzzy matching in v1.** Only exact normalized-key equality; no edit
//!   distance, so "Bob" and "Rob" never collapse. A missed dup (two nodes for one
//!   thing) is recoverable; a wrong merge is not.
//! - **Unicode NFC folding is applied** (since 2026-07-02): composed and
//!   decomposed spellings of the same accented name ("café" vs "cafe\u{301}")
//!   resolve to one key. Keys that were already NFC-composed are unchanged, so
//!   pre-existing ids for ordinary keyboard input are stable.
//! - **Embedding similarity is not applicable yet.** Entities are not embedded, so
//!   there is no vector layer to run (unlike memories); if entity embeddings ever
//!   land, a conservative same-kind ANN check (à la the memory seam's 0.95 cutoff)
//!   is the place to add it — this seam deliberately introduces no new embedding
//!   pipeline for dedup.
//! - **Retroactive merge is out of scope.** This seam only prevents *new*
//!   duplicates; collapsing two entities that already exist as distinct nodes (and
//!   re-homing their edges) is a destructive operation reserved for the
//!   LLM-assisted escalation path (§29), not done here.

use std::collections::HashSet;

use uuid::Uuid;

use catalerum_core::{Entity, EntityId, EntityKind, WorkspaceId};
use catalerum_graph::GraphStore;

use crate::error::Result;

/// What [`project_entity_deduped`] did with a resolved entity — the observability
/// signal callers surface (`created` / `deduplicated`), the entity twin of the
/// memory seam's [`MemoryStoreStatus`](crate::dedup::MemoryStoreStatus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityStoreStatus {
    /// A genuinely new entity — its node was created.
    Created,
    /// A duplicate of an existing entity — no new node; the surviving node was
    /// upserted in place and returned.
    Deduplicated,
}

impl EntityStoreStatus {
    /// The stable wire token for this status.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EntityStoreStatus::Created => "created",
            EntityStoreStatus::Deduplicated => "deduplicated",
        }
    }
}

/// The result of a dedup-aware entity projection: the resolved entity (freshly
/// created or the surviving duplicate) plus what happened to it.
#[derive(Clone, Debug)]
pub struct EntityStoreOutcome {
    /// The entity the caller should treat as the outcome of its write — always the
    /// *surviving* node, so edges drawn to it can never orphan.
    pub entity: Entity,
    /// Whether it was newly created or deduplicated.
    pub status: EntityStoreStatus,
}

/// Normalize a raw entity name into its **dedup key**: trim, collapse internal
/// whitespace runs to single spaces, NFC-fold, and casefold. Pure — the basis of
/// both the deterministic id ([`entity_id`]) and duplicate detection.
///
/// NFC folding means composed/decomposed spellings of the same accented name
/// (U+00E9 vs `e`+U+0301) resolve to the same key — without it they minted two
/// entities. Pre-NFC ids for names that were *already* NFC-composed (the common
/// case for keyboard input) are unchanged.
#[must_use]
pub fn normalize_entity_name(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    entity_display_name(name)
        .nfc()
        .collect::<String>()
        .to_lowercase()
}

/// The **display form** of a raw entity name: trim and collapse internal
/// whitespace runs to single spaces, preserving case. Pure — the first-seen
/// display form kept on the node (its lowercase is [`normalize_entity_name`]).
#[must_use]
pub fn entity_display_name(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The stable id-prefix for an [`EntityKind`] — the kind scope of the dedup key.
///
/// **Stable by contract:** these strings feed the UUIDv5 in [`entity_id`], so an
/// entity's id depends on them; changing a token would re-home every entity of
/// that kind (a graph reprojection). The `Topic` token is `"topic"` so ids match
/// the tag/label `:Topic` projection that predates this seam byte-for-byte.
#[must_use]
const fn kind_prefix(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Person => "person",
        EntityKind::Org => "org",
        EntityKind::Topic => "topic",
        EntityKind::Project => "project",
        EntityKind::Place => "place",
    }
}

/// The stable, **kind-scoped** id for a normalized entity key within a workspace: a
/// UUIDv5 over `(workspace, "<kind>:<key>")`. The same `(kind, key)` is the same
/// id on every projection and across notes/events (idempotent §3.4), so the
/// idempotent Neo4j `MERGE` never duplicates it; a different kind — or a different
/// workspace — yields a different id. `key` must already be normalized
/// ([`normalize_entity_name`]).
#[must_use]
pub fn entity_id(workspace_id: WorkspaceId, kind: EntityKind, key: &str) -> EntityId {
    let name = format!("{}:{key}", kind_prefix(kind));
    EntityId::from_uuid(Uuid::new_v5(&workspace_id.as_uuid(), name.as_bytes()))
}

/// Resolve one raw `(kind, name)` reference to its canonical [`Entity`] — the
/// heuristic dedup applied to a single name. Returns `None` when the name is blank
/// after normalization (an empty tag/label references nothing). The returned
/// entity's id is deterministic ([`entity_id`]), so *every* reference that
/// normalizes to the same `(kind, key)` resolves to the **same** entity — the
/// survivor a caller links to. `aliases` start empty (this seam mints no aliases).
#[must_use]
pub fn resolve_entity(workspace_id: WorkspaceId, kind: EntityKind, name: &str) -> Option<Entity> {
    let display = entity_display_name(name);
    if display.is_empty() {
        return None;
    }
    let key = display.to_lowercase();
    Some(Entity {
        id: entity_id(workspace_id, kind, &key),
        workspace_id,
        kind,
        display_name: display,
        aliases: Vec::new(),
    })
}

/// Resolve a batch of raw `(kind, name)` references to their **distinct** canonical
/// entities, in first-seen order, deduplicating within the batch (SOUL §6.3/§29).
/// Two candidates that normalize to the same `(kind, key)` fold to one entity
/// keeping the first-seen display form; blanks are dropped. This is the
/// note-tags / event-labels projection's dedup pass — the single normalizer the
/// tag/label `:Topic` synthesis routes through.
#[must_use]
pub fn resolve_entities<'a, I>(workspace_id: WorkspaceId, candidates: I) -> Vec<Entity>
where
    I: IntoIterator<Item = (EntityKind, &'a str)>,
{
    let mut seen: HashSet<(EntityKind, String)> = HashSet::new();
    candidates
        .into_iter()
        .filter_map(|(kind, name)| {
            let entity = resolve_entity(workspace_id, kind, name)?;
            // Key the dedup on (kind, normalized) — same as the id scope — so a
            // repeated tag folds but a same-name different-kind reference does not.
            if seen.insert((kind, entity.display_name.to_lowercase())) {
                Some(entity)
            } else {
                None
            }
        })
        .collect()
}

/// Project a single entity through the dedup seam, reporting created-vs-
/// deduplicated (SOUL §29) — the entity twin of
/// [`store_memory_deduped`](crate::dedup::store_memory_deduped) and the entry point
/// the anticipated body-extraction path (§10) should write through so a re-stated
/// reference never mints a second node.
///
/// Resolves `name` to its canonical [`Entity`] ([`resolve_entity`]); a blank name
/// yields `Ok(None)` (nothing to project). Otherwise it upserts the entity via
/// [`GraphStore::project_entity_reporting`] — the heuristic match *is* the
/// deterministic-id hit against the existing graph node — and returns the survivor
/// tagged [`EntityStoreStatus::Deduplicated`] when a node already existed, else
/// [`EntityStoreStatus::Created`]. Idempotent: the underlying `MERGE` means a
/// duplicate never creates a second node regardless of the reported status.
pub async fn project_entity_deduped(
    graph: &GraphStore,
    workspace_id: WorkspaceId,
    kind: EntityKind,
    name: &str,
) -> Result<Option<EntityStoreOutcome>> {
    let Some(entity) = resolve_entity(workspace_id, kind, name) else {
        return Ok(None);
    };
    let existed = graph.project_entity_reporting(&entity).await?;
    let status = if existed {
        EntityStoreStatus::Deduplicated
    } else {
        EntityStoreStatus::Created
    };
    Ok(Some(EntityStoreOutcome { entity, status }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace_and_casefolds() {
        assert_eq!(
            normalize_entity_name("  Machine   Learning  "),
            "machine learning"
        );
        assert_eq!(
            normalize_entity_name("Ada\tLovelace"),
            normalize_entity_name("ada  lovelace")
        );
        assert_eq!(
            normalize_entity_name("Works\nin\nBerlin"),
            "works in berlin"
        );
        assert_eq!(normalize_entity_name("   "), "");
        assert_eq!(normalize_entity_name(""), "");
    }

    #[test]
    fn normalize_nfc_folds_composed_and_decomposed_accents() {
        // U+00E9 (é composed) vs "e" + U+0301 (combining acute) — same key.
        assert_eq!(
            normalize_entity_name("Caf\u{e9}"),
            normalize_entity_name("Cafe\u{301}")
        );
        // And therefore the same deterministic entity id.
        let ws = WorkspaceId::new();
        assert_eq!(
            entity_id(ws, EntityKind::Place, &normalize_entity_name("Caf\u{e9}")),
            entity_id(ws, EntityKind::Place, &normalize_entity_name("Cafe\u{301}"))
        );
        // Already-composed input keys are unchanged by the fold.
        assert_eq!(normalize_entity_name("Caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn display_form_preserves_case_but_collapses_whitespace() {
        assert_eq!(
            entity_display_name("  Machine   Learning  "),
            "Machine Learning"
        );
        assert_eq!(entity_display_name("\tAda\nLovelace "), "Ada Lovelace");
        assert_eq!(entity_display_name("   "), "");
    }

    #[test]
    fn entity_id_is_workspace_and_kind_scoped_and_deterministic() {
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        // Stable for the same (workspace, kind, key).
        assert_eq!(
            entity_id(a, EntityKind::Person, "ada"),
            entity_id(a, EntityKind::Person, "ada")
        );
        // Per-workspace.
        assert_ne!(
            entity_id(a, EntityKind::Person, "ada"),
            entity_id(b, EntityKind::Person, "ada")
        );
        // Per-kind: same normalized name, different kind → distinct entity (§29).
        assert_ne!(
            entity_id(a, EntityKind::Person, "mercury"),
            entity_id(a, EntityKind::Place, "mercury")
        );
        // Per-key.
        assert_ne!(
            entity_id(a, EntityKind::Topic, "x"),
            entity_id(a, EntityKind::Topic, "y")
        );
    }

    #[test]
    fn topic_id_stays_byte_compatible_with_the_pre_seam_projection() {
        // The tag/label `:Topic` projection minted ids as UUIDv5 over
        // "topic:<normalized>"; this must not change or existing nodes re-home.
        let ws = WorkspaceId::new();
        let want = EntityId::from_uuid(Uuid::new_v5(&ws.as_uuid(), b"topic:machine learning"));
        assert_eq!(entity_id(ws, EntityKind::Topic, "machine learning"), want);
    }

    #[test]
    fn resolve_entity_normalizes_and_drops_blanks() {
        let ws = WorkspaceId::new();
        assert!(resolve_entity(ws, EntityKind::Topic, "   ").is_none());
        let e = resolve_entity(ws, EntityKind::Person, "  Ada   Lovelace ").unwrap();
        // Display keeps case, whitespace collapsed; id is the normalized-key hash.
        assert_eq!(e.display_name, "Ada Lovelace");
        assert_eq!(e.kind, EntityKind::Person);
        assert_eq!(e.id, entity_id(ws, EntityKind::Person, "ada lovelace"));
        assert!(e.aliases.is_empty());
        // Case/whitespace variants resolve to the *same* entity id (the survivor).
        let e2 = resolve_entity(ws, EntityKind::Person, "ADA  LOVELACE").unwrap();
        assert_eq!(e.id, e2.id);
    }

    #[test]
    fn resolve_entities_dedups_batch_case_insensitively_keeping_first_seen_display() {
        let ws = WorkspaceId::new();
        let out = resolve_entities(
            ws,
            [
                (EntityKind::Topic, "Work"),
                (EntityKind::Topic, "work"),
                (EntityKind::Topic, "  "),
                (EntityKind::Topic, "Ideas"),
            ],
        );
        // "Work"/"work" collapse; blank dropped → 2 distinct topics, order kept.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].display_name, "Work"); // first-seen casing
        assert_eq!(out[0].id, entity_id(ws, EntityKind::Topic, "work"));
        assert_eq!(out[1].display_name, "Ideas");
    }

    #[test]
    fn resolve_entities_keeps_same_name_across_kinds_distinct() {
        let ws = WorkspaceId::new();
        let out = resolve_entities(
            ws,
            [
                (EntityKind::Person, "Mercury"),
                (EntityKind::Place, "Mercury"),
                (EntityKind::Person, "mercury"), // dup of the Person
            ],
        );
        // Person "Mercury" + Place "Mercury" are distinct; the third folds.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, EntityKind::Person);
        assert_eq!(out[1].kind, EntityKind::Place);
        assert_ne!(out[0].id, out[1].id);
    }

    #[test]
    fn status_wire_tokens_are_stable() {
        assert_eq!(EntityStoreStatus::Created.as_str(), "created");
        assert_eq!(EntityStoreStatus::Deduplicated.as_str(), "deduplicated");
    }
}

/// Live integration tests for the write-path seam against a real Neo4j — gated on
/// `NEO4J_URL` (e.g. `http://localhost:7474`) so the suite **skips and passes**
/// with no server, the same pattern as `catalerum-graph`'s `live` tests.
/// `NEO4J_USER` / `NEO4J_PASSWORD` default to `neo4j` / `catalerum`.
#[cfg(test)]
mod live {
    use super::*;

    fn graph() -> Option<GraphStore> {
        let url = std::env::var("NEO4J_URL").ok()?;
        let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into());
        let password = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "catalerum".into());
        Some(
            GraphStore::new(&url)
                .expect("valid NEO4J_URL")
                .with_auth(user, password),
        )
    }

    #[tokio::test]
    async fn project_entity_deduped_creates_then_dedups() {
        let Some(graph) = graph() else {
            eprintln!("NEO4J_URL unset; skipping live entity dedup seam test");
            return;
        };
        graph.ensure_indexes().await.expect("indexes");
        let ws = WorkspaceId::new();
        graph.delete_workspace(ws).await.unwrap();

        // First reference → created.
        let a = project_entity_deduped(&graph, ws, EntityKind::Person, "Ada Lovelace")
            .await
            .unwrap()
            .expect("non-blank name resolves");
        assert_eq!(a.status, EntityStoreStatus::Created);

        // A case/whitespace variant of the same person → deduplicated onto it (same
        // surviving id), no second node.
        let b = project_entity_deduped(&graph, ws, EntityKind::Person, "  ada   LOVELACE ")
            .await
            .unwrap()
            .expect("non-blank name resolves");
        assert_eq!(b.status, EntityStoreStatus::Deduplicated);
        assert_eq!(a.entity.id, b.entity.id);
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 1);

        // Same normalized name, different kind → its own creation (§29).
        let place = project_entity_deduped(&graph, ws, EntityKind::Place, "Ada Lovelace")
            .await
            .unwrap()
            .expect("non-blank name resolves");
        assert_eq!(place.status, EntityStoreStatus::Created);
        assert_ne!(a.entity.id, place.entity.id);
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 2);

        // A blank name references nothing → no outcome, no node.
        assert!(project_entity_deduped(&graph, ws, EntityKind::Topic, "   ")
            .await
            .unwrap()
            .is_none());
        assert_eq!(graph.count_nodes(ws).await.unwrap(), 2);

        graph.delete_workspace(ws).await.unwrap();
    }
}
