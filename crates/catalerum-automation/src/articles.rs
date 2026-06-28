//! **Internal articles** (SOUL §11): curated, embeddable how-to guides that walk an
//! author end-to-end through building a real automation — the worked-example layer
//! above the per-node-type [`catalog`](crate::catalog).
//!
//! Where a [`NodeDoc`](crate::NodeDoc) documents *one* trigger/action in isolation,
//! an [`Article`] is a full recipe: the goal, the graph shape, the node-by-node
//! wiring, and the gotchas — for a whole task like "ingest my email" or "index a
//! GitHub wiki and expose it over MCP". They are the content an agent (over the
//! `search_articles` tool) or the visual editor (over `/articles/search`) surfaces
//! when a user asks *how do I build X*, not *which node does Y*.
//!
//! Like the catalog this is **pure data** — a curated Markdown corpus compiled in via
//! [`include_str!`] and parsed once, with no I/O or engine state. Each article's
//! [`Article::embed_text`] is what the in-memory semantic index in `catalerum-api`
//! (`article_index`) embeds; this crate only owns the corpus. Every `related_node`
//! must be a real [`catalog`](crate::catalog) id, and every article's `body_md` is
//! rendered through the shared `catalerum-markdown` engine, so both stay honest
//! against the node types they teach.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// One internal how-to article: a worked, end-to-end automation recipe. Serialized
/// verbatim to the REST surface and embedded (via [`embed_text`](Article::embed_text))
/// for semantic search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Article {
    /// Stable slug the article is addressed by (e.g. `"email-ingestion"`,
    /// `"github-wiki-mcp"`). Unique across the corpus.
    pub id: String,
    /// Human-readable title (e.g. "Ingest your email into notes").
    pub title: String,
    /// One-line summary — what the recipe accomplishes.
    pub summary: String,
    /// Coarse grouping for listing/filtering (e.g. `"automation-example"`).
    pub category: String,
    /// Display tags shown in the UI (short, user-facing labels).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Intent/synonym words that broaden semantic-search recall (not shown; they
    /// bias the embedding toward the phrasings a searcher is likely to use).
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The [`catalog`](crate::catalog) node-type ids this recipe wires together
    /// (e.g. `"trigger.collect_email"`, `"action.label_email"`) — the cross-link back
    /// to the per-node docs. Every entry must resolve via [`catalog::get`](crate::catalog::get).
    #[serde(default)]
    pub related_nodes: Vec<String>,
    /// The full article body in Markdown (CommonMark + GFM), rendered by
    /// `catalerum-markdown`.
    pub body_md: String,
}

/// The most `body_md` characters folded into [`embed_text`](Article::embed_text). Keeps
/// the embed input comfortably under a typical model's context window even for a long
/// article; the title/summary/keywords carry the short-query signal regardless.
const EMBED_BODY_CHARS: usize = 6000;

impl Article {
    /// The text embedded for semantic search: title, id, summary, tags, and keywords
    /// (strong signal for short intent queries) followed by the article body
    /// (truncated to [`EMBED_BODY_CHARS`], for natural-language "how do I…" queries).
    #[must_use]
    pub fn embed_text(&self) -> String {
        let body: String = self.body_md.chars().take(EMBED_BODY_CHARS).collect();
        format!(
            "{title} ({id}): {summary} Tags: {tags} Keywords: {keywords}\n\n{body}",
            title = self.title,
            id = self.id,
            summary = self.summary,
            tags = self.tags.join(", "),
            keywords = self.keywords.join(", "),
        )
    }
}

/// The curated article corpus, parsed once from the compiled-in JSON. A parse failure
/// is a build-time authoring bug in `articles.json`, so it panics on first access (the
/// embedded document is not user input).
static ARTICLES: LazyLock<Vec<Article>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("articles.json"))
        .expect("embedded internal articles (articles.json) must be valid JSON")
});

/// Every internal article, in a stable display order (as authored in `articles.json`).
#[must_use]
pub fn articles() -> &'static [Article] {
    &ARTICLES
}

/// The article with the given [`Article::id`] (e.g. `"email-ingestion"`), if it exists.
#[must_use]
pub fn get(id: &str) -> Option<&'static Article> {
    ARTICLES.iter().find(|a| a.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn articles_parse_and_ids_are_unique() {
        let all = articles();
        assert!(!all.is_empty(), "article corpus must not be empty");
        let ids: HashSet<&str> = all.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids.len(), all.len(), "article ids must be unique");
    }

    #[test]
    fn every_article_is_well_formed() {
        for a in articles() {
            assert!(!a.id.is_empty(), "article id is non-empty");
            assert!(!a.title.is_empty(), "{}: title", a.id);
            assert!(!a.summary.is_empty(), "{}: summary", a.id);
            assert!(!a.category.is_empty(), "{}: category", a.id);
            assert!(!a.body_md.is_empty(), "{}: body", a.id);
            // The embed input always carries the identifying signal.
            let text = a.embed_text();
            assert!(text.contains(&a.id), "{}: embed_text carries id", a.id);
            assert!(
                text.contains(&a.title),
                "{}: embed_text carries title",
                a.id
            );
        }
    }

    #[test]
    fn related_nodes_reference_real_catalog_entries() {
        // Every cross-linked node id must resolve to a real node-type doc, so an
        // article can't teach a node that doesn't exist (or drift when one is renamed).
        for a in articles() {
            for node in &a.related_nodes {
                assert!(
                    crate::catalog::get(node).is_some(),
                    "article `{}` references unknown node type `{node}`",
                    a.id,
                );
            }
        }
    }

    #[test]
    fn get_resolves_and_rejects() {
        let first = &articles()[0];
        assert!(get(&first.id).is_some());
        assert!(get("nope-nope-nope").is_none());
    }

    /// The bodies of every ` ```json ` fenced code block in `md`.
    fn json_blocks(md: &str) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut current: Option<String> = None;
        for line in md.lines() {
            match &mut current {
                None => {
                    if line.trim_start().starts_with("```json") {
                        current = Some(String::new());
                    }
                }
                Some(buf) => {
                    if line.trim_start().starts_with("```") {
                        blocks.push(std::mem::take(buf));
                        current = None;
                    } else {
                        buf.push_str(line);
                        buf.push('\n');
                    }
                }
            }
        }
        blocks
    }

    #[test]
    fn embedded_graph_examples_are_valid_and_pasteable() {
        // Every ```json block an article ships as a "paste-ready" example must be real:
        // a `{ "graph": … }` block deserializes into a `Graph` that passes the engine's
        // authoring validator, and a bare node block deserializes into a typed `Node`.
        // Guards the recipes against drifting out of sync with the graph model.
        use crate::graph::{Graph, Node};
        let mut checked = 0;
        for a in articles() {
            for block in json_blocks(&a.body_md) {
                let value: serde_json::Value = serde_json::from_str(&block).unwrap_or_else(|e| {
                    panic!(
                        "article `{}`: a ```json block is not valid JSON: {e}\n{block}",
                        a.id
                    )
                });
                if let Some(graph_val) = value.get("graph") {
                    let graph: Graph =
                        serde_json::from_value(graph_val.clone()).unwrap_or_else(|e| {
                            panic!(
                                "article `{}`: graph example is not a valid Graph: {e}",
                                a.id
                            )
                        });
                    graph.validate().unwrap_or_else(|e| {
                        panic!("article `{}`: graph example fails validation: {e}", a.id)
                    });
                } else if value.get("kind").is_some() {
                    // A bare graph node shown on its own (e.g. an action example).
                    serde_json::from_value::<Node>(value.clone()).unwrap_or_else(|e| {
                        panic!("article `{}`: node example is not a valid Node: {e}", a.id)
                    });
                } else {
                    panic!(
                        "article `{}`: a ```json block is neither a graph nor a node",
                        a.id
                    );
                }
                checked += 1;
            }
        }
        assert!(
            checked >= articles().len(),
            "every article ships at least one validated JSON example (checked {checked})",
        );
    }
}
