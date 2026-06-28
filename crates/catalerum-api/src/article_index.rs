//! Semantic index over the **internal articles** corpus
//! ([`catalerum_automation::articles`], SOUL §11) — so an author (an LLM agent over
//! `search_articles`, or the visual editor over `/articles/search`) can find the
//! worked how-to recipe they need by intent ("how do I ingest my email", "expose a
//! wiki over MCP") instead of paging through every article.
//!
//! It mirrors [`NodeDocIndex`](crate::node_index::NodeDocIndex): each article's
//! [`embed_text`](catalerum_automation::Article::embed_text) is embedded with the
//! workspace's configured llmleaf embedder, and `search` ranks the corpus against an
//! embedded query by cosine similarity. The corpus is **static** (it ships in the
//! binary and never changes at runtime), so the index is in-memory and embeds each
//! article exactly once — [`reconcile`](ArticleIndex::reconcile) embeds only what
//! isn't embedded yet and is a no-op thereafter. It is called on each search (so the
//! first search warms it) and pre-warmed at boot. Like the node-type index there is no
//! capability filtering: the articles are global documentation, identical for everyone.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use catalerum_automation::Article;
use catalerum_core::embed::EmbeddingRequest;
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::Embedder;

/// One ranked article match: the full [`Article`] plus its similarity score.
#[derive(Clone, Debug, PartialEq)]
pub struct ArticleHit {
    pub article: Article,
    /// Cosine similarity to the query in `[-1, 1]` (higher is closer).
    pub score: f32,
}

/// In-memory semantic index of the static internal-articles corpus.
#[derive(Clone)]
pub struct ArticleIndex {
    embedder: Arc<dyn Embedder>,
    model: String,
    /// `article id → embedding vector`, filled lazily/at boot.
    vectors: Arc<RwLock<HashMap<String, Vec<f32>>>>,
}

impl ArticleIndex {
    /// A new, empty index embedding with `embedder` + `model` (the workspace's
    /// configured embedding model, `[llm].embedding_model`).
    #[must_use]
    pub fn new(embedder: Arc<dyn Embedder>, model: impl Into<String>) -> Self {
        Self {
            embedder,
            model: model.into(),
            vectors: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Read guard over the vectors (recovers from a poisoned lock).
    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Vec<f32>>> {
        self.vectors.read().unwrap_or_else(|e| e.into_inner())
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

    /// Embed any article not yet embedded (the whole corpus on first call, then
    /// nothing). Returns how many articles were embedded. Idempotent.
    ///
    /// # Errors
    /// If the embedder fails; the existing vectors are left untouched so a transient
    /// outage degrades to stale-but-usable rather than empty.
    pub async fn reconcile(&self) -> Result<usize> {
        let articles = catalerum_automation::articles();
        let to_embed: Vec<(String, String)> = {
            let vectors = self.read();
            articles
                .iter()
                .filter(|a| !vectors.contains_key(&a.id))
                .map(|a| (a.id.clone(), a.embed_text()))
                .collect()
        };
        if to_embed.is_empty() {
            return Ok(0);
        }
        let embedded = self
            .embed(to_embed.iter().map(|(_, t)| t.clone()).collect())
            .await?;
        let mut vectors = self.vectors.write().unwrap_or_else(|e| e.into_inner());
        for ((id, _), vector) in to_embed.iter().zip(embedded) {
            vectors.insert(id.clone(), vector);
        }
        Ok(to_embed.len())
    }

    /// Search the article corpus for the recipes matching `query` by meaning, ranked
    /// by cosine similarity, top `limit` first. Warms the index first (a reconcile
    /// failure is non-fatal — the search proceeds against whatever is already
    /// embedded).
    ///
    /// # Errors
    /// If embedding the query fails.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<ArticleHit>> {
        let _ = self.reconcile().await;

        let query_vec = self
            .embed(vec![query.to_string()])
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| Error::provider("embedder returned no query vector"))?;

        let vectors = self.read();
        let mut hits: Vec<ArticleHit> = catalerum_automation::articles()
            .iter()
            .filter_map(|article| {
                vectors.get(&article.id).map(|v| ArticleHit {
                    score: cosine(&query_vec, v),
                    article: article.clone(),
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
    use catalerum_core::embed::{Embedding, EmbeddingResponse};

    /// A deterministic offline embedder: a tiny keyword-presence vector, so
    /// "similar text → similar vector" holds and ranking is testable without a
    /// network call.
    struct KeywordEmbedder;

    const VOCAB: &[&str] = &[
        "email",
        "inbox",
        "label",
        "webhook",
        "http",
        "wiki",
        "mcp",
        "embedding",
    ];

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

    fn index() -> ArticleIndex {
        ArticleIndex::new(Arc::new(KeywordEmbedder), "test-model")
    }

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero-norm → 0");
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0, "length mismatch → 0");
    }

    #[tokio::test]
    async fn reconcile_embeds_the_whole_corpus_once() {
        let idx = index();
        let n = idx.reconcile().await.unwrap();
        assert_eq!(
            n,
            catalerum_automation::articles().len(),
            "embeds every article"
        );
        assert_eq!(
            idx.reconcile().await.unwrap(),
            0,
            "second reconcile is a no-op"
        );
    }

    #[tokio::test]
    async fn search_ranks_relevant_articles_first() {
        let idx = index();
        // An email-intent query surfaces an email article near the top.
        let hits = idx
            .search("how do I import my inbox email", 4)
            .await
            .unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter()
                .take(2)
                .any(|h| h.article.id == "email-ingestion" || h.article.id == "email-tagging"),
            "an email article ranks for an email query, got {:?}",
            hits.iter().map(|h| &h.article.id).collect::<Vec<_>>()
        );

        // A wiki/MCP query surfaces the wiki article.
        let hits = idx
            .search("expose a wiki over mcp with embedding", 4)
            .await
            .unwrap();
        assert!(
            hits.iter()
                .take(2)
                .any(|h| h.article.id == "github-wiki-mcp"),
            "wiki article ranks for a wiki/mcp query, got {:?}",
            hits.iter().map(|h| &h.article.id).collect::<Vec<_>>()
        );

        // limit is respected.
        assert_eq!(idx.search("webhook", 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_lazily_reconciles() {
        let idx = index();
        // No explicit reconcile — search must warm the index itself.
        let hits = idx.search("webhook http", 3).await.unwrap();
        assert!(!hits.is_empty());
    }
}
