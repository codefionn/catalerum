//! Point, payload, and query types for the derived Qdrant index (SOUL §6.4).
//!
//! Qdrant is a *derived* index: every point is rebuildable from Postgres truth
//! (`chunks` / `memories` / `notes`), so each point carries a [`SourceRef`] back
//! to the row it came from. Payloads carry **source ref, kind, entity ids, and a
//! timestamp** so semantic search is *filtered* — the workspace filter is always
//! applied (defense-in-depth on top of per-workspace collections, §18), and
//! callers can further narrow by artifact kind, mentioned entities, or time
//! window.

use catalerum_core::{EntityId, SourceRef, WorkspaceId};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::error::{Result, VectorError};

/// The distance metric a collection scores with. Cosine is the default and the
/// right choice for normalized text embeddings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Distance {
    /// Cosine similarity (the default for text embeddings).
    #[default]
    Cosine,
    /// Dot product.
    Dot,
    /// Euclidean (L2) distance.
    Euclid,
}

impl Distance {
    /// The string Qdrant expects in a collection's `distance` field.
    #[must_use]
    pub fn as_qdrant(self) -> &'static str {
        match self {
            Distance::Cosine => "Cosine",
            Distance::Dot => "Dot",
            Distance::Euclid => "Euclid",
        }
    }
}

/// The stable, filterable discriminant for a [`SourceRef`] — what kind of source
/// row a point was derived from. Stored as the `kind` payload field so a search
/// can be scoped to (say) only notes and memories.
#[must_use]
pub fn source_kind(source: &SourceRef) -> &'static str {
    match source {
        SourceRef::Event { .. } => "event",
        SourceRef::Object { .. } => "object",
        SourceRef::Note { .. } => "note",
        SourceRef::Memory { .. } => "memory",
        SourceRef::Email { .. } => "email",
        SourceRef::Message { .. } => "message",
        SourceRef::Document { .. } => "document",
        SourceRef::External { .. } => "external",
    }
}

/// The flat string id of the source row (the uuid for first-class rows, the uri
/// for an external resource). Stored as the `source_id` payload field so all
/// points derived from one source can be deleted/re-projected in a single
/// filtered call (`delete_by_source`).
#[must_use]
pub fn source_id_string(source: &SourceRef) -> String {
    match source {
        SourceRef::Event { id } => id.to_string(),
        SourceRef::Object { id } => id.to_string(),
        SourceRef::Note { id } => id.to_string(),
        SourceRef::Memory { id } => id.to_string(),
        SourceRef::Email { id } => id.to_string(),
        SourceRef::Message { id } => id.to_string(),
        SourceRef::Document { id } => id.to_string(),
        SourceRef::External { uri } => uri.clone(),
    }
}

/// Normalize a caller-supplied key prefix to a directory boundary (trailing
/// `/`), so scoping to "subdir `acme`" and "subdir `acme/`" mean the same thing.
/// An empty/`"/"` prefix normalizes to empty (no narrowing).
#[must_use]
pub fn normalize_key_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_start_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.ends_with('/') {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/")
    }
}

/// Every directory-boundary ancestor prefix of a storage `key`, each with a
/// trailing `/`. Emitted as the filterable `key_prefixes` array so a prefix
/// search is a server-side **exact** `match` (Qdrant has no prefix operator) —
/// e.g. `acme/docs/page.md` → `["acme/", "acme/docs/"]`, so a query for `acme/`
/// or `acme/docs/` matches, but `ac/` does not.
#[must_use]
pub fn key_ancestor_prefixes(key: &str) -> Vec<String> {
    let mut prefixes = Vec::new();
    let mut acc = String::new();
    // Split on '/', keep every prefix up to (but not including) the last segment
    // (the filename): each directory boundary is a valid subdir scope.
    let segments: Vec<&str> = key.split('/').collect();
    for seg in segments.iter().take(segments.len().saturating_sub(1)) {
        if seg.is_empty() {
            continue;
        }
        acc.push_str(seg);
        acc.push('/');
        prefixes.push(acc.clone());
    }
    prefixes
}

/// The payload stored alongside a vector — the filterable metadata plus the
/// chunk text (so a search result is self-describing without a second DB hop).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointPayload {
    /// The workspace this point belongs to (always filtered on, §18).
    pub workspace_id: WorkspaceId,
    /// The Postgres-truth row this point was derived from (§3.1).
    pub source: SourceRef,
    /// The text that was embedded.
    pub text: String,
    /// Entities mentioned in the text, for entity-filtered retrieval (§6.4).
    pub entity_ids: Vec<EntityId>,
    /// The storage bucket this point's source object lives in, denormalized for
    /// cheap filtering. Empty for non-object sources (note/memory/…). Lets a
    /// search (or a custom MCP endpoint) scope to one bucket.
    pub bucket_name: String,
    /// The storage key (path) of this point's source object, denormalized so a
    /// search can `prefix`-match it — e.g. narrow to one wiki subdir. Empty for
    /// non-object sources.
    pub key: String,
    /// When the source was created, for time-windowed retrieval. Stored as a
    /// unix-second integer so Qdrant range filters work.
    pub created_at: Option<DateTime<Utc>>,
}

impl PointPayload {
    /// A minimal payload: a workspace, a source, and the embedded text.
    #[must_use]
    pub fn new(workspace_id: WorkspaceId, source: SourceRef, text: impl Into<String>) -> Self {
        Self {
            workspace_id,
            source,
            text: text.into(),
            entity_ids: Vec::new(),
            bucket_name: String::new(),
            key: String::new(),
            created_at: None,
        }
    }

    /// Attach mentioned entities.
    #[must_use]
    pub fn with_entities(mut self, entity_ids: Vec<EntityId>) -> Self {
        self.entity_ids = entity_ids;
        self
    }

    /// Attach the source object's storage bucket + key so search can scope to a
    /// bucket / key-prefix (a subdir). No-op-friendly: pass empty strings for
    /// non-object sources.
    #[must_use]
    pub fn with_storage(mut self, bucket_name: impl Into<String>, key: impl Into<String>) -> Self {
        self.bucket_name = bucket_name.into();
        self.key = key.into();
        self
    }

    /// Attach a source timestamp (enables time-window filtering).
    #[must_use]
    pub fn with_created_at(mut self, created_at: DateTime<Utc>) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// The denormalized `kind` discriminant for this payload's source.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        source_kind(&self.source)
    }

    /// Render to the JSON object Qdrant stores. `kind` and `source_id` are
    /// denormalized flat fields for cheap filtering; `source` keeps the full
    /// typed pointer for reconstruction.
    #[must_use]
    pub fn to_qdrant(&self) -> Value {
        let mut map = Map::new();
        map.insert("workspace_id".into(), json!(self.workspace_id.to_string()));
        map.insert("kind".into(), json!(source_kind(&self.source)));
        map.insert("source_id".into(), json!(source_id_string(&self.source)));
        map.insert(
            "source".into(),
            serde_json::to_value(&self.source).unwrap_or(Value::Null),
        );
        map.insert("text".into(), json!(self.text));
        map.insert(
            "entity_ids".into(),
            json!(self
                .entity_ids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()),
        );
        // Only emit the storage fields when present, so note/memory points stay
        // as compact as before and never carry empty-string clutter.
        if !self.bucket_name.is_empty() {
            map.insert("bucket_name".into(), json!(self.bucket_name));
        }
        if !self.key.is_empty() {
            map.insert("key".into(), json!(self.key));
            // Denormalized ancestor prefixes so a subdir search is an exact,
            // index-friendly `match` rather than an (unsupported) prefix scan.
            map.insert(
                "key_prefixes".into(),
                json!(key_ancestor_prefixes(&self.key)),
            );
        }
        if let Some(ts) = self.created_at {
            map.insert("created_at".into(), json!(ts.timestamp()));
        }
        Value::Object(map)
    }

    /// Reconstruct a payload from the JSON object Qdrant returns.
    pub fn from_qdrant(value: &Value) -> Result<Self> {
        let obj = value
            .as_object()
            .ok_or_else(|| VectorError::Malformed("payload is not a JSON object".into()))?;

        let workspace_id = obj
            .get("workspace_id")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| VectorError::Malformed("payload missing workspace_id".into()))?;

        let source: SourceRef = obj
            .get("source")
            .ok_or_else(|| VectorError::Malformed("payload missing source".into()))
            .and_then(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|e| VectorError::Malformed(format!("bad source: {e}")))
            })?;

        let text = obj
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let entity_ids = obj
            .get("entity_ids")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .filter_map(|s| s.parse().ok())
                    .collect()
            })
            .unwrap_or_default();

        let bucket_name = obj
            .get("bucket_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let key = obj
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let created_at = obj
            .get("created_at")
            .and_then(Value::as_i64)
            .and_then(|secs| DateTime::from_timestamp(secs, 0));

        Ok(Self {
            workspace_id,
            source,
            text,
            entity_ids,
            bucket_name,
            key,
            created_at,
        })
    }
}

/// A vector point ready to upsert: a stable id, its embedding, and its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorPoint {
    /// Stable point id (use the `chunks.qdrant_point_id` / `memories.point_id`).
    pub id: uuid::Uuid,
    /// The embedding vector (its width must match the collection).
    pub vector: Vec<f32>,
    /// Filterable metadata + the embedded text.
    pub payload: PointPayload,
}

impl VectorPoint {
    /// A point with a freshly generated id.
    #[must_use]
    pub fn new(vector: Vec<f32>, payload: PointPayload) -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            vector,
            payload,
        }
    }

    /// A point with a caller-supplied id (idempotent re-upsert of one chunk).
    #[must_use]
    pub fn with_id(id: uuid::Uuid, vector: Vec<f32>, payload: PointPayload) -> Self {
        Self {
            id,
            vector,
            payload,
        }
    }

    pub(crate) fn to_qdrant(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "vector": self.vector,
            "payload": self.payload.to_qdrant(),
        })
    }
}

/// One hit from a search: the point id, its similarity score, and its payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredPoint {
    /// The matched point's id.
    pub id: uuid::Uuid,
    /// Similarity score under the collection's distance metric.
    pub score: f32,
    /// The matched point's payload.
    pub payload: PointPayload,
}

/// Optional narrowing applied on top of the always-present workspace filter.
/// An empty filter (the default) means "all points in the workspace".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchFilter {
    /// Restrict to these source kinds (`note`, `memory`, … — match *any*).
    pub kinds: Vec<String>,
    /// Restrict to points mentioning *any* of these entities.
    pub entity_ids: Vec<EntityId>,
    /// Restrict to points whose source object lives in this exact bucket.
    pub bucket_name: Option<String>,
    /// Restrict to points whose source object `key` starts with this prefix —
    /// the "one subdir" narrowing (e.g. `acme/` for a wiki's files).
    pub key_prefix: Option<String>,
    /// Only points whose source `created_at` is at or after this instant.
    pub created_after: Option<DateTime<Utc>>,
    /// Only points whose source `created_at` is at or before this instant.
    pub created_before: Option<DateTime<Utc>>,
}

impl SearchFilter {
    /// True when no narrowing beyond the workspace is requested.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty()
            && self.entity_ids.is_empty()
            && self.bucket_name.is_none()
            && self.key_prefix.is_none()
            && self.created_after.is_none()
            && self.created_before.is_none()
    }

    /// Build the Qdrant `filter` object for `workspace_id` + this narrowing.
    /// The workspace match is **always** present, so a search can never reach
    /// another tenant's points even if collections were ever shared (§18).
    #[must_use]
    pub fn to_qdrant(&self, workspace_id: WorkspaceId) -> Value {
        let mut must = vec![json!({
            "key": "workspace_id",
            "match": { "value": workspace_id.to_string() },
        })];

        if !self.kinds.is_empty() {
            must.push(json!({ "key": "kind", "match": { "any": self.kinds } }));
        }
        if !self.entity_ids.is_empty() {
            let ids: Vec<String> = self.entity_ids.iter().map(ToString::to_string).collect();
            must.push(json!({ "key": "entity_ids", "match": { "any": ids } }));
        }
        if let Some(bucket) = &self.bucket_name {
            must.push(json!({ "key": "bucket_name", "match": { "value": bucket } }));
        }
        if let Some(prefix) = &self.key_prefix {
            // Exact match against the denormalized ancestor-prefix array — the
            // point is "under this subdir" iff its key_prefixes contains it.
            let norm = normalize_key_prefix(prefix);
            if !norm.is_empty() {
                must.push(json!({ "key": "key_prefixes", "match": { "value": norm } }));
            }
        }
        if self.created_after.is_some() || self.created_before.is_some() {
            let mut range = Map::new();
            if let Some(after) = self.created_after {
                range.insert("gte".into(), json!(after.timestamp()));
            }
            if let Some(before) = self.created_before {
                range.insert("lte".into(), json!(before.timestamp()));
            }
            must.push(json!({ "key": "created_at", "range": Value::Object(range) }));
        }

        json!({ "must": must })
    }
}

/// A filtered ANN query: the query vector, how many neighbours to return, the
/// optional narrowing, and an optional minimum score.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchQuery {
    /// The query embedding (its width must match the collection).
    pub vector: Vec<f32>,
    /// Max neighbours to return.
    pub limit: u64,
    /// Narrowing applied on top of the workspace filter.
    pub filter: SearchFilter,
    /// Drop hits scoring below this threshold (metric-dependent).
    pub score_threshold: Option<f32>,
}

impl SearchQuery {
    /// A top-`limit` query for `vector` with no extra narrowing.
    #[must_use]
    pub fn new(vector: Vec<f32>, limit: u64) -> Self {
        Self {
            vector,
            limit,
            filter: SearchFilter::default(),
            score_threshold: None,
        }
    }

    /// Apply a narrowing filter.
    #[must_use]
    pub fn with_filter(mut self, filter: SearchFilter) -> Self {
        self.filter = filter;
        self
    }

    /// Set a minimum score cutoff.
    #[must_use]
    pub fn with_score_threshold(mut self, threshold: f32) -> Self {
        self.score_threshold = Some(threshold);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §18 tenant-isolation invariant for the **empty** filter — the
    /// "all points in the workspace" path (`is_empty()` true). It must still emit
    /// *exactly* the `workspace_id` must-clause, never an unfiltered query that
    /// would reach every tenant. (The full-filter case — workspace always first
    /// alongside the other clauses — is covered by
    /// `full_filter_builds_all_clauses_with_workspace_always_first` in `lib.rs`.)
    #[test]
    fn to_qdrant_empty_filter_is_workspace_scoped_not_unfiltered() {
        let ws = WorkspaceId::new();
        assert!(SearchFilter::default().is_empty());
        let q = SearchFilter::default().to_qdrant(ws);
        let must = q["must"].as_array().expect("must array");
        assert_eq!(
            must.len(),
            1,
            "empty filter must be workspace-only: {must:?}"
        );
        assert_eq!(
            must[0],
            json!({ "key": "workspace_id", "match": { "value": ws.to_string() } })
        );
    }

    #[test]
    fn key_ancestor_prefixes_are_directory_boundaries() {
        assert_eq!(
            key_ancestor_prefixes("acme/docs/page.md"),
            vec!["acme/".to_string(), "acme/docs/".to_string()]
        );
        // A top-level file has no directory ancestor.
        assert_eq!(key_ancestor_prefixes("readme.md"), Vec::<String>::new());
        // Leading slashes and empty segments are ignored.
        assert_eq!(
            key_ancestor_prefixes("/a//b/c.txt"),
            vec!["a/".to_string(), "a/b/".to_string()]
        );
    }

    #[test]
    fn normalize_key_prefix_forces_directory_boundary() {
        assert_eq!(normalize_key_prefix("acme"), "acme/");
        assert_eq!(normalize_key_prefix("acme/docs"), "acme/docs/");
        assert_eq!(normalize_key_prefix("/acme/"), "acme/");
        assert_eq!(normalize_key_prefix(""), "");
        assert_eq!(normalize_key_prefix("/"), "");
    }

    #[test]
    fn payload_round_trips_storage_fields_and_emits_prefixes() {
        let ws = WorkspaceId::new();
        let src = SourceRef::Object {
            id: catalerum_core::ObjectId::new(),
        };
        let payload = PointPayload::new(ws, src, "hi").with_storage("wiki", "acme/docs/page.md");
        let json = payload.to_qdrant();
        assert_eq!(json["bucket_name"], "wiki");
        assert_eq!(json["key"], "acme/docs/page.md");
        assert_eq!(json["key_prefixes"], json!(["acme/", "acme/docs/"]));
        // bucket_name/key survive the round-trip (key_prefixes is derived, not stored back).
        assert_eq!(PointPayload::from_qdrant(&json).unwrap(), payload);
    }

    #[test]
    fn note_payload_omits_storage_fields() {
        let ws = WorkspaceId::new();
        let src = SourceRef::Note {
            id: catalerum_core::NoteId::new(),
        };
        let json = PointPayload::new(ws, src, "n").to_qdrant();
        assert!(json.get("bucket_name").is_none());
        assert!(json.get("key").is_none());
        assert!(json.get("key_prefixes").is_none());
    }
}
