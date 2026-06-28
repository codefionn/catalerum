//! Semantic index over the tool registry (SOUL §6.4/§7) — so an agent can
//! **search** for the tool it needs by intent instead of being shown every spec.
//!
//! With the static tools plus runtime MCP tools (a single Playwright server adds
//! ~25), advertising the whole registry every turn is costly and noisy. The
//! [`ToolIndex`] embeds each tool's `name: description` with the same llmleaf
//! embedder chat uses, and `search` ranks them against an embedded query by
//! cosine similarity — returning only the tools the **caller is allowed to call**
//! (its `required_capability` covered by the context's capabilities, deny-by-default
//! §19). The `search_tools` tool is the thin wrapper.
//!
//! The corpus is tiny (tens–hundreds), global, and changes rarely, so the index is
//! **in memory** (no Qdrant collection, works whenever the embedder does) and
//! **self-syncing**: [`reconcile`](ToolIndex::reconcile) embeds tools newly present
//! (e.g. just-connected MCP servers) and drops tools gone from the registry. It is
//! called on each search (incremental — embeds only what changed) and pre-warmed at
//! boot.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use catalerum_core::embed::EmbeddingRequest;
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::Embedder;
use catalerum_core::tool::{ToolContext, ToolRegistry};

/// One ranked tool match.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolHit {
    pub name: String,
    pub description: String,
    /// Cosine similarity to the query in `[-1, 1]` (higher is closer).
    pub score: f32,
}

/// An embedded tool entry: its description (to detect changes) and vector.
struct Indexed {
    description: String,
    vector: Vec<f32>,
}

/// In-memory semantic index of the tool registry, kept in sync with it.
#[derive(Clone)]
pub struct ToolIndex {
    embedder: Arc<dyn Embedder>,
    model: String,
    entries: Arc<RwLock<HashMap<String, Indexed>>>,
}

impl ToolIndex {
    /// A new, empty index embedding with `embedder` + `model` (the workspace's
    /// configured embedding model, `[llm].embedding_model`).
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder>, model: impl Into<String>) -> Self {
        Self {
            embedder,
            model: model.into(),
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Read guard over the entries (recovers from a poisoned lock).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Indexed>> {
        self.entries.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Embed `texts` in one batch, returning one vector per input in order.
    async fn embed(&self, texts: Vec<String>) -> Result<Vec<Vec<f32>>> {
        let resp = self
            .embedder
            .embed(EmbeddingRequest::new(&self.model, texts))
            .await?;
        // Order is preserved end to end, but sort by index to be defensive.
        let mut pairs: Vec<_> = resp
            .embeddings
            .into_iter()
            .map(|e| (e.index, e.vector))
            .collect();
        pairs.sort_by_key(|(i, _)| *i);
        Ok(pairs.into_iter().map(|(_, v)| v).collect())
    }

    /// Bring the index in line with `registry`: embed tools newly present (or whose
    /// description changed) and drop tools no longer registered. Incremental — only
    /// the delta is embedded. Returns how many tools were (re-)embedded.
    ///
    /// # Errors
    /// If the embedder fails; the existing index is left untouched so a transient
    /// embedder outage degrades to stale-but-usable rather than empty.
    pub async fn reconcile(&self, registry: &ToolRegistry) -> Result<usize> {
        let specs = registry.specs(None);
        let current: HashSet<String> = specs.iter().map(|s| s.name.clone()).collect();

        // Snapshot what needs embedding without holding the lock across the await.
        let to_embed: Vec<(String, String)> = {
            let entries = self.read();
            specs
                .iter()
                .filter(|s| {
                    entries
                        .get(&s.name)
                        .is_none_or(|e| e.description != s.description)
                })
                .map(|s| (s.name.clone(), embed_text(&s.name, &s.description)))
                .collect()
        };

        if !to_embed.is_empty() {
            let vectors = self
                .embed(to_embed.iter().map(|(_, t)| t.clone()).collect())
                .await?;
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            for ((name, _), vector) in to_embed.iter().zip(vectors) {
                let description = specs
                    .iter()
                    .find(|s| &s.name == name)
                    .map(|s| s.description.clone())
                    .unwrap_or_default();
                entries.insert(
                    name.clone(),
                    Indexed {
                        description,
                        vector,
                    },
                );
            }
            // Drop entries for tools that have since left the registry.
            entries.retain(|name, _| current.contains(name));
        } else {
            // Nothing to embed, but tools may have been removed — prune.
            let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
            if entries.len() != current.len() {
                entries.retain(|name, _| current.contains(name));
            }
        }
        Ok(to_embed.len())
    }

    /// Search the registry for tools matching `query` by meaning, restricted to the
    /// tools `ctx` is **allowed** to call (its capabilities cover the tool's
    /// `required_capability`, §19). Reconciles first so a just-added MCP server's
    /// tools are searchable, then ranks the allowed set by cosine similarity and
    /// returns the top `limit`.
    ///
    /// # Errors
    /// If embedding the query fails. A reconcile failure is non-fatal — the search
    /// proceeds against whatever is already indexed.
    pub async fn search(
        &self,
        registry: &ToolRegistry,
        ctx: &ToolContext,
        query: &str,
        limit: usize,
    ) -> Result<Vec<ToolHit>> {
        // Keep the index fresh (best-effort: an outage here shouldn't sink a search
        // that the existing entries can still serve).
        let _ = self.reconcile(registry).await;

        let query_vec = self
            .embed(vec![query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::provider("embedder returned no query vector"))?;

        let entries = self.read();
        let mut hits: Vec<ToolHit> = registry
            .specs(None)
            .into_iter()
            // Never offer the discovery tools themselves (they're always
            // advertised), and only tools the caller may call.
            .filter(|s| {
                s.name != SEARCH_TOOLS_NAME
                    && s.name != LIST_TOOLS_NAME
                    && tool_allowed(registry, ctx, &s.name)
            })
            .filter_map(|s| {
                entries.get(&s.name).map(|e| ToolHit {
                    score: cosine(&query_vec, &e.vector),
                    name: s.name,
                    description: s.description,
                })
            })
            .collect();
        // Highest score first; NaN scores (degenerate vectors) sort last.
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }
}

/// The name of the wrapper tool ([`crate::tools`]), excluded from its own results.
pub(crate) const SEARCH_TOOLS_NAME: &str = "search_tools";

/// The catalog-listing sibling of `search_tools` ([`crate::tools`]) — likewise
/// excluded from discovery results (it is always advertised alongside them).
pub(crate) const LIST_TOOLS_NAME: &str = "list_tools";

/// The text embedded for a tool: its name carries signal alongside the description.
fn embed_text(name: &str, description: &str) -> String {
    if description.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {description}")
    }
}

/// Whether `ctx` may dispatch `name` — the exact gate
/// [`ToolRegistry::dispatch`](catalerum_core::tool::ToolRegistry::dispatch) applies:
/// when the caller's capabilities are known, the tool's `required_capability` must
/// be covered; an unknown context (trusted) or an ungated tool always passes.
/// Shared with `list_tools` ([`crate::tools`]) so both discovery surfaces filter
/// identically.
pub(crate) fn tool_allowed(registry: &ToolRegistry, ctx: &ToolContext, name: &str) -> bool {
    let Some(tool) = registry.get(name) else {
        return false;
    };
    match (&ctx.capabilities, tool.required_capability()) {
        (Some(caps), Some(required)) => caps.iter().any(|held| held.covers(&required)),
        _ => true,
    }
}

/// Cosine similarity of two equal-length vectors; `0.0` for a length mismatch or a
/// zero-norm vector (no direction to compare).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use catalerum_core::capability::{Action, Capability, Resource};
    use catalerum_core::embed::{Embedding, EmbeddingResponse};
    use catalerum_core::tool::Tool;
    use serde_json::{json, Value};

    /// A deterministic offline embedder: a tiny keyword-presence vector, so
    /// "similar text → similar vector" holds and ranking is testable without a
    /// network call.
    struct KeywordEmbedder;

    const VOCAB: &[&str] = &["browser", "note", "email", "calendar"];

    #[async_trait]
    impl Embedder for KeywordEmbedder {
        async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse> {
            let embeddings = request
                .input
                .iter()
                .enumerate()
                .map(|(i, text)| {
                    let lower = text.to_lowercase();
                    let vector = VOCAB
                        .iter()
                        .map(|w| if lower.contains(w) { 1.0 } else { 0.0 })
                        .collect();
                    Embedding {
                        index: i as u32,
                        vector,
                    }
                })
                .collect();
            Ok(EmbeddingResponse {
                model: request.model,
                embeddings,
                usage: None,
            })
        }
    }

    /// A no-op tool with a chosen name/description and optional required capability.
    struct StubTool {
        name: String,
        description: String,
        capability: Option<Capability>,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            &self.description
        }
        fn required_capability(&self) -> Option<Capability> {
            self.capability.clone()
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn invoke(&self, _args: Value, _ctx: &ToolContext) -> Result<Value> {
            Ok(json!({}))
        }
    }

    fn stub(name: &str, description: &str, cap: Option<Capability>) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name: name.to_string(),
            description: description.to_string(),
            capability: cap,
        })
    }

    fn index() -> ToolIndex {
        ToolIndex::new(Arc::new(KeywordEmbedder), "test-model")
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero-norm → 0");
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0, "length mismatch → 0");
    }

    #[tokio::test]
    async fn ranks_relevant_tools_first_and_respects_capabilities() {
        let mut reg = ToolRegistry::new();
        reg.register(stub(
            "browse",
            "drive a web browser to navigate pages",
            None,
        ));
        reg.register(stub("take_note", "write a note", None));
        reg.register(stub(
            "read_calendar",
            "list calendar events",
            Some(Capability::new(Action::Read, Resource::domain("calendar"))),
        ));
        let idx = index();

        // Owner-ish context (no capability restriction): a browser query ranks the
        // browser tool first.
        let ctx = ToolContext::default();
        let hits = idx.search(&reg, &ctx, "open a browser", 10).await.unwrap();
        assert_eq!(hits[0].name, "browse");
        assert!(hits[0].score > 0.9);
        assert_eq!(hits.len(), 3, "all tools indexed and returned, ranked");

        // A restricted context lacking `calendar:read` must NOT see the calendar
        // tool — search returns only what it could actually call (§19).
        let restricted = ToolContext {
            capabilities: Some(vec![Capability::new(
                Action::Write,
                Resource::domain("notes"),
            )]),
            ..Default::default()
        };
        let names: Vec<_> = idx
            .search(&reg, &restricted, "calendar events", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|h| h.name)
            .collect();
        assert!(
            !names.contains(&"read_calendar".to_string()),
            "gated tool filtered out"
        );
        assert!(names.contains(&"browse".to_string()) && names.contains(&"take_note".to_string()));
    }

    #[tokio::test]
    async fn reconcile_adds_new_tools_and_prunes_removed_ones() {
        let idx = index();
        let mut reg = ToolRegistry::new();
        reg.register(stub("browse", "web browser", None));
        assert_eq!(idx.reconcile(&reg).await.unwrap(), 1, "embeds the one tool");
        assert_eq!(idx.read().len(), 1);

        // A hot-added (overlay) tool is embedded on the next reconcile…
        reg.register_dynamic(stub("send_email", "send an email", None));
        assert_eq!(
            idx.reconcile(&reg).await.unwrap(),
            1,
            "embeds only the new tool"
        );
        assert_eq!(idx.read().len(), 2);
        // …and removing it prunes the stale entry (no re-embed).
        reg.unregister_dynamic("send_email");
        assert_eq!(
            idx.reconcile(&reg).await.unwrap(),
            0,
            "nothing new to embed"
        );
        assert_eq!(idx.read().len(), 1, "the removed tool is pruned");
        assert!(idx.read().contains_key("browse"));
    }

    #[tokio::test]
    async fn search_lazily_reconciles_a_newly_added_tool() {
        let idx = index();
        let reg = ToolRegistry::new();
        reg.register_dynamic(stub("browse", "drive a web browser", None));
        // No explicit reconcile — search must pick the new overlay tool up itself.
        let hits = idx
            .search(&reg, &ToolContext::default(), "browser", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "browse");
    }
}
