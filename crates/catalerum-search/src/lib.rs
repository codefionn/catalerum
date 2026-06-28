//! catalerum-search — web search behind the core [`WebSearcher`] trait, exposed
//! to the LLM as one capability-gated `web_search` tool (SOUL §27, §19).
//!
//! # Backends
//! Each is a thin HTTP+JSON client behind its own cargo feature (all on by
//! default), mirroring how [`catalerum_fetch`](https://docs.rs) layers fetch
//! backends:
//! - **Brave** ([`BraveSearcher`], `brave`) — independent index, `X-Subscription-Token` header, GET.
//! - **Tavily** ([`TavilySearcher`], `tavily`) — LLM/RAG-tuned; key in the JSON body, POST; returns excerpts + a synthesized `answer`.
//! - **Exa** ([`ExaSearcher`], `exa`) — neural + keyword, `x-api-key` header, POST.
//! - **SearXNG** ([`SearxngSearcher`], `searxng`) — self-hosted metasearch, no key, `format=json`, GET.
//! - **Google PSE** ([`GoogleSearcher`], `google`) — Programmable Search Engine, `key` + `cx`, GET.
//! - **SerpAPI** ([`SerpApiSearcher`], `serpapi`) — real Google/Bing scrape, `api_key` query param, GET.
//!
//! [`MultiSearcher`] routes each [`SearchRequest`] to a backend by
//! [`SearchRequest::provider`], falling back to a configured default. Every
//! backend returns the common [`SearchResults`] shape so the model — and the
//! `web_search` tool — sees one schema regardless of engine.
//!
//! Safety: search hits trusted vendor endpoints, so the egress SSRF guard that
//! `catalerum-fetch` applies to arbitrary URLs is not needed here; the model
//! reaches result URLs only through the separate `fetch_url` tool (SOUL §19).

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchRequest, SearchResults, WebSearcher};

pub mod tool;

#[cfg(feature = "brave")]
pub mod brave;
#[cfg(feature = "exa")]
pub mod exa;
#[cfg(feature = "google")]
pub mod google;
#[cfg(feature = "searxng")]
pub mod searxng;
#[cfg(feature = "serpapi")]
pub mod serpapi;
#[cfg(feature = "tavily")]
pub mod tavily;

pub use tool::{SearchDefaults, WebSearchTool};

#[cfg(feature = "brave")]
pub use brave::BraveSearcher;
#[cfg(feature = "exa")]
pub use exa::ExaSearcher;
#[cfg(feature = "google")]
pub use google::GoogleSearcher;
#[cfg(feature = "searxng")]
pub use searxng::SearxngSearcher;
#[cfg(feature = "serpapi")]
pub use serpapi::SerpApiSearcher;
#[cfg(feature = "tavily")]
pub use tavily::TavilySearcher;

// Re-export the core search surface for ergonomic `use catalerum_search::…`.
pub use catalerum_core::provider::{SearchHit, SearchRequest as Request, SearchResults as Results};

/// The canonical provider ids, in display order. Mirrors the cargo features and
/// the `[search]` config sub-blocks — the single source of truth a settings UI
/// iterates to show "which engines exist".
pub const PROVIDER_IDS: &[&str] = &["brave", "tavily", "exa", "searxng", "google", "serpapi"];

/// Routes a [`SearchRequest`] to one of several [`WebSearcher`] backends by
/// provider id (SOUL §27). A request with no `provider` resolves to `default`;
/// an unknown/disabled provider is a clear `Invalid` error rather than a silent
/// fallback (a search against the wrong engine is worse than an error).
pub struct MultiSearcher {
    /// Wired backends, in registration order (preserved for listing).
    backends: Vec<Arc<dyn WebSearcher>>,
    /// Provider id a no-`provider` request resolves to.
    default: String,
}

impl MultiSearcher {
    /// Build a router over `backends`, resolving bare requests to `default`.
    #[must_use]
    pub fn new(backends: Vec<Arc<dyn WebSearcher>>, default: impl Into<String>) -> Self {
        Self {
            backends,
            default: default.into(),
        }
    }

    /// The provider ids that are actually wired, in registration order.
    #[must_use]
    pub fn provider_names(&self) -> Vec<&str> {
        self.backends.iter().map(|b| b.name()).collect()
    }

    /// The provider a no-`provider` request resolves to.
    #[must_use]
    pub fn default_provider(&self) -> &str {
        &self.default
    }

    /// Whether any backend is wired.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    fn resolve(&self, name: &str) -> Option<&Arc<dyn WebSearcher>> {
        self.backends.iter().find(|b| b.name() == name)
    }
}

#[async_trait]
impl WebSearcher for MultiSearcher {
    fn name(&self) -> &str {
        "multi"
    }

    async fn search(&self, mut request: SearchRequest) -> Result<SearchResults> {
        let name = request
            .provider
            .take()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| self.default.clone());
        let backend = self.resolve(&name).ok_or_else(|| {
            Error::invalid(format!(
                "unknown or disabled search provider `{name}` (available: {})",
                self.provider_names().join(", ")
            ))
        })?;
        backend.search(request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubSearcher(&'static str);

    #[async_trait]
    impl WebSearcher for StubSearcher {
        fn name(&self) -> &str {
            self.0
        }
        async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
            Ok(SearchResults {
                query: request.query,
                provider: self.0.to_string(),
                results: vec![],
                answer: None,
            })
        }
    }

    fn router() -> MultiSearcher {
        MultiSearcher::new(
            vec![
                Arc::new(StubSearcher("brave")),
                Arc::new(StubSearcher("tavily")),
            ],
            "brave",
        )
    }

    #[tokio::test]
    async fn bare_request_uses_default() {
        let out = router().search(SearchRequest::new("q")).await.unwrap();
        assert_eq!(out.provider, "brave");
    }

    #[tokio::test]
    async fn provider_override_routes() {
        let out = router()
            .search(SearchRequest::new("q").provider("tavily"))
            .await
            .unwrap();
        assert_eq!(out.provider, "tavily");
    }

    #[tokio::test]
    async fn unknown_provider_errors() {
        let err = router()
            .search(SearchRequest::new("q").provider("bing"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        assert!(err.to_string().contains("brave, tavily"));
    }

    #[test]
    fn lists_wired_providers() {
        let r = router();
        assert_eq!(r.provider_names(), vec!["brave", "tavily"]);
        assert_eq!(r.default_provider(), "brave");
        assert!(!r.is_empty());
    }
}
