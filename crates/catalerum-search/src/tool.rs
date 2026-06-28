//! The `web_search` LLM tool (SOUL §7, §27).
//!
//! A thin [`Tool`] wrapper over a [`WebSearcher`] (usually the
//! [`MultiSearcher`](crate::MultiSearcher) router): the model passes a query and
//! gets back ranked results — title, url, snippet — in one schema regardless of
//! which engine served them. Like every tool it is a client of a scoped
//! capability — `web:search` — enforced at the API choke point, never by the
//! model (SOUL §3.3, §19). Results are deliberately compact (snippets, not full
//! pages) to keep context cheap; the model fetches a promising URL with the
//! separate `fetch_url` tool (§27).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::error::{Error, Result};
use catalerum_core::id::{UserId, WorkspaceId};
use catalerum_core::provider::{SearchRequest, WebSearcher};
use catalerum_core::tool::{Tool, ToolContext};

const MAX_QUERIES: usize = 4;

/// Resolves a caller's preferred default search provider (SOUL §7/§13).
///
/// The per-user override the settings UI writes. `catalerum-api` implements this
/// over the `search_settings` store; [`WebSearchTool`] consults it **only** when
/// the model didn't pin a `provider`, so an unset/absent override transparently
/// falls back to the router's configured default. Kept a trait (injected at
/// construction) so `catalerum-search` stays free of any store dependency.
#[async_trait]
pub trait SearchDefaults: Send + Sync {
    /// The caller's preferred default provider, or `None` to use the router's
    /// configured default. `user_id` is absent for non-user (agent) callers.
    async fn default_provider(
        &self,
        workspace_id: WorkspaceId,
        user_id: Option<UserId>,
    ) -> Option<String>;
}

/// The `web_search` tool — search the web and return ranked results.
pub struct WebSearchTool {
    searcher: Arc<dyn WebSearcher>,
    defaults: Option<Arc<dyn SearchDefaults>>,
}

impl WebSearchTool {
    /// Wrap a searcher as the `web_search` tool.
    #[must_use]
    pub fn new(searcher: Arc<dyn WebSearcher>) -> Self {
        Self {
            searcher,
            defaults: None,
        }
    }

    /// Attach a per-user default-provider resolver (SOUL §7/§13). When the model
    /// omits `provider`, the tool asks the resolver for the caller's preferred
    /// default before falling back to the router's configured default.
    #[must_use]
    pub fn with_defaults(mut self, defaults: Arc<dyn SearchDefaults>) -> Self {
        self.defaults = Some(defaults);
        self
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn required_capability(&self) -> Option<Capability> {
        // A scoped read of the web (SOUL §19/§27): its own verb on the `web`
        // domain, mirroring `vector:search`, so a grant can deny search while
        // still allowing `fetch_url` (`web:read`) — or vice versa.
        Some(Capability::new(Action::Search, Resource::domain("web")))
    }

    fn description(&self) -> &str {
        "Search the web and return ranked results (title, url, snippet). Pass `queries` \
         (an array); the result is a `searches` map keyed by each query. \
         Start with ONE focused query and a small `limit` (usually 3–5); batch only \
         genuinely independent questions, not overlapping variants, because every \
         query multiplies the returned context. Use results to choose the few pages \
         worth reading with `fetch_url`. \
         Set `provider` to pick a specific engine; omit it for the configured default. \
         `limit`, `provider`, `include_raw_content`, and `freshness` apply to every \
         query. `include_raw_content` returns full page text per hit \
         (slower/pricier)."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "queries": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUERIES,
                    "items": { "type": "string" },
                    "description": "One focused query is preferred. Batch up to 4 only when the questions are independent; avoid overlapping variants. Returns a `searches` map keyed by query."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 20,
                    "default": 5,
                    "description": "Max results PER query. Prefer 3–5 initially; increase only when needed because snippets can be long."
                },
                "provider": {
                    "type": "string",
                    "description": "Optional engine override, e.g. brave/tavily/exa. Omit for the default."
                },
                "include_raw_content": {
                    "type": "boolean",
                    "default": false,
                    "description": "Return full page text per hit (slower/pricier; not every engine supports it)."
                },
                "freshness": {
                    "type": "string",
                    "description": "Optional recency hint, e.g. day/week/month/year. Best-effort per engine."
                }
            },
            "required": ["queries"]
        })
    }

    async fn invoke(&self, args: Json, ctx: &ToolContext) -> Result<Json> {
        let mut queries = args
            .get("queries")
            .and_then(Json::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Json::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Be liberal at the execution boundary for older clients and nested
        // `catalerum.callTool` scripts. `queries` remains the only advertised
        // shape, but accepting a lone `query` avoids wasting an agent round when
        // a model recalls the common singular web-search convention. If both
        // forms are present, treat the singular value as one more query.
        if let Some(query) = args
            .get("query")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|query| !query.is_empty())
        {
            queries.push(query.to_string());
        }

        // Dedup in first-seen order so repeated strings never cause another
        // metered engine call or collide in the result map.
        {
            let mut seen = std::collections::HashSet::new();
            queries.retain(|q| seen.insert(q.clone()));
        }
        if queries.is_empty() {
            return Err(Error::invalid(
                "`queries` must contain at least one non-empty string",
            ));
        }
        if queries.len() > MAX_QUERIES {
            return Err(Error::invalid(format!(
                "`queries` accepts at most {MAX_QUERIES} unique queries; start with one focused query and split independent follow-ups into another call"
            )));
        }

        // The shared knobs (`limit`/`provider`/`include_raw_content`/`freshness`)
        // apply to every query, so resolve the provider default once up front rather
        // than per query (SOUL §13).
        let mut provider = args
            .get("provider")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        if provider.is_none() {
            if let (Some(defaults), Some(ws)) = (&self.defaults, ctx.workspace_id) {
                provider = defaults.default_provider(ws, ctx.user_id).await;
            }
        }

        // A request template carrying the shared knobs; each query clones it.
        let mut template = SearchRequest::new(String::new());
        template.provider = provider;
        if let Some(n) = args.get("limit").and_then(Json::as_u64) {
            template.limit = n.clamp(1, 20) as u32;
        }
        if let Some(b) = args.get("include_raw_content").and_then(Json::as_bool) {
            template.include_raw_content = b;
        }
        if let Some(f) = args.get("freshness").and_then(Json::as_str) {
            let f = f.trim();
            if !f.is_empty() {
                template.freshness = Some(f.to_string());
            }
        }
        let request_for = |query: &str| {
            let mut r = template.clone();
            r.query = query.to_string();
            r
        };

        // Run every search concurrently, then assemble a dictionary keyed by
        // the query string. A single engine error degrades to a per-query `error`
        // entry instead of failing the whole batch, so one bad query can't sink the
        // rest.
        let outcomes =
            futures::future::join_all(queries.iter().map(|q| self.searcher.search(request_for(q))))
                .await;

        let mut searches = serde_json::Map::with_capacity(queries.len());
        for (query, outcome) in queries.iter().zip(outcomes) {
            let value = match outcome {
                Ok(mut results) => {
                    results.results.truncate(template.limit as usize);
                    serde_json::to_value(results)
                        .map_err(|e| Error::invalid(format!("encoding results: {e}")))?
                }
                Err(e) => json!({ "query": query, "error": e.to_string() }),
            };
            searches.insert(query.clone(), value);
        }
        Ok(json!({ "searches": Json::Object(searches) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::provider::{SearchHit, SearchResults};

    struct StubSearcher;

    #[async_trait]
    impl WebSearcher for StubSearcher {
        fn name(&self) -> &str {
            "stub"
        }
        async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
            Ok(SearchResults {
                query: request.query,
                provider: format!("limit={}", request.limit),
                results: vec![SearchHit {
                    title: "Example".into(),
                    url: "https://example.com".into(),
                    snippet: Some("hi".into()),
                    raw_content: None,
                    score: None,
                    published: None,
                }],
                answer: None,
            })
        }
    }

    fn tool() -> WebSearchTool {
        WebSearchTool::new(Arc::new(StubSearcher))
    }

    #[test]
    fn advertises_capability_and_schema() {
        let t = tool();
        assert_eq!(t.name(), "web_search");
        assert_eq!(
            t.required_capability(),
            Some(Capability::new(Action::Search, Resource::domain("web")))
        );
        let schema = t.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("anyOf").is_none());
        assert!(schema["properties"].get("query").is_none());
        assert_eq!(schema["required"], json!(["queries"]));
        assert_eq!(schema["properties"]["queries"]["type"], "array");
        assert_eq!(schema["properties"]["queries"]["minItems"], 1);
        assert_eq!(schema["properties"]["queries"]["maxItems"], 4);
        assert!(t.description().contains("ONE focused query"));
    }

    #[tokio::test]
    async fn invokes_and_clamps_limit() {
        let out = tool()
            .invoke(
                json!({ "queries": ["rust"], "limit": 99 }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        // limit clamped to 20.
        assert_eq!(out["searches"]["rust"]["provider"], "limit=20");
        assert_eq!(out["searches"]["rust"]["query"], "rust");
        assert_eq!(
            out["searches"]["rust"]["results"][0]["url"],
            "https://example.com"
        );
    }

    struct CountingSearcher(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl WebSearcher for CountingSearcher {
        fn name(&self) -> &str {
            "counting"
        }
        async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SearchResults {
                query: request.query,
                provider: "counting".into(),
                results: vec![],
                answer: None,
            })
        }
    }

    #[tokio::test]
    async fn duplicate_queries_are_searched_once() {
        // Repeated values collapse to exactly two metered engine calls.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = WebSearchTool::new(Arc::new(CountingSearcher(calls.clone())));
        let out = tool
            .invoke(
                json!({ "queries": ["rust", "rust", "go", "go"] }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "deduped to 2 searches"
        );
        let searches = out["searches"].as_object().expect("searches map");
        assert_eq!(searches.len(), 2);
        assert!(searches.contains_key("rust") && searches.contains_key("go"));
    }

    #[tokio::test]
    async fn rejects_more_queries_than_the_advertised_context_bound() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = WebSearchTool::new(Arc::new(CountingSearcher(calls.clone())));
        let err = tool
            .invoke(
                json!({ "queries": ["one", "two", "three", "four", "five"] }),
                &ToolContext::default(),
            )
            .await
            .expect_err("too many queries");
        assert!(err.to_string().contains("at most 4 unique queries"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    struct OverReturningSearcher(usize);

    #[async_trait]
    impl WebSearcher for OverReturningSearcher {
        fn name(&self) -> &str {
            "over"
        }
        async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
            let results = (0..self.0)
                .map(|i| SearchHit {
                    title: format!("r{i}"),
                    url: format!("https://e/{i}"),
                    snippet: None,
                    raw_content: None,
                    score: None,
                    published: None,
                })
                .collect();
            Ok(SearchResults {
                query: request.query,
                provider: "over".into(),
                results,
                answer: None,
            })
        }
    }

    #[tokio::test]
    async fn output_is_capped_to_limit_even_if_the_provider_over_returns() {
        let tool = WebSearchTool::new(Arc::new(OverReturningSearcher(10)));
        // Engine returns 10 hits, `limit` 2 → capped to 2 per query.
        let out = tool
            .invoke(
                json!({ "queries": ["a"], "limit": 2 }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            out["searches"]["a"]["results"].as_array().unwrap().len(),
            2,
            "{out}"
        );
    }

    #[tokio::test]
    async fn missing_queries_are_invalid_but_legacy_singular_query_works() {
        let err = tool()
            .invoke(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));

        let out = tool()
            .invoke(json!({ "query": "old form" }), &ToolContext::default())
            .await
            .unwrap();
        assert_eq!(out["searches"]["old form"]["query"], "old form");
    }

    #[tokio::test]
    async fn legacy_singular_query_combines_with_and_deduplicates_queries() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let tool = WebSearchTool::new(Arc::new(CountingSearcher(calls.clone())));
        let out = tool
            .invoke(
                json!({ "queries": ["rust"], "query": "rust" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(out["searches"].as_object().unwrap().len(), 1);
    }

    struct FixedDefaults(Option<&'static str>);

    #[async_trait]
    impl SearchDefaults for FixedDefaults {
        async fn default_provider(
            &self,
            _ws: WorkspaceId,
            _user: Option<UserId>,
        ) -> Option<String> {
            self.0.map(str::to_string)
        }
    }

    /// A searcher that echoes back the provider the request carried (or "none").
    struct EchoProvider;

    #[async_trait]
    impl WebSearcher for EchoProvider {
        fn name(&self) -> &str {
            "echo"
        }
        async fn search(
            &self,
            request: SearchRequest,
        ) -> Result<catalerum_core::provider::SearchResults> {
            Ok(catalerum_core::provider::SearchResults {
                query: request.query,
                provider: request.provider.unwrap_or_else(|| "none".into()),
                results: vec![],
                answer: None,
            })
        }
    }

    #[tokio::test]
    async fn per_user_default_applies_only_when_provider_omitted() {
        let ctx = ToolContext::for_workspace(WorkspaceId::new());
        // No provider in args → resolver's preference is used.
        let t = WebSearchTool::new(Arc::new(EchoProvider))
            .with_defaults(Arc::new(FixedDefaults(Some("tavily"))));
        let out = t.invoke(json!({ "queries": ["q"] }), &ctx).await.unwrap();
        assert_eq!(out["searches"]["q"]["provider"], "tavily");

        // Explicit provider in args → resolver is NOT consulted.
        let out = t
            .invoke(json!({ "queries": ["q"], "provider": "brave" }), &ctx)
            .await
            .unwrap();
        assert_eq!(out["searches"]["q"]["provider"], "brave");

        // Resolver returns None → request stays bare (router default applies).
        let t =
            WebSearchTool::new(Arc::new(EchoProvider)).with_defaults(Arc::new(FixedDefaults(None)));
        let out = t.invoke(json!({ "queries": ["q"] }), &ctx).await.unwrap();
        assert_eq!(out["searches"]["q"]["provider"], "none");
    }

    #[tokio::test]
    async fn batch_returns_a_dictionary_keyed_by_query() {
        let out = tool()
            .invoke(
                json!({ "queries": ["rust", "tokio"], "limit": 3 }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        // Top-level `searches` map, one entry per query, each a full result object
        // (the stub echoes the query and `limit=N` into provider).
        let searches = out["searches"].as_object().expect("searches map");
        assert_eq!(searches.len(), 2);
        assert_eq!(out["searches"]["rust"]["query"], "rust");
        assert_eq!(out["searches"]["tokio"]["query"], "tokio");
        assert_eq!(out["searches"]["rust"]["provider"], "limit=3");
        assert_eq!(
            out["searches"]["rust"]["results"][0]["url"],
            "https://example.com"
        );
        // No single-search keys leak to the top level in batch mode.
        assert!(out.get("results").is_none());
    }

    /// A searcher that errors for the query `boom`, else echoes the query back.
    struct BoomSearcher;

    #[async_trait]
    impl WebSearcher for BoomSearcher {
        fn name(&self) -> &str {
            "boom"
        }
        async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
            if request.query == "boom" {
                return Err(Error::invalid("engine exploded"));
            }
            Ok(SearchResults {
                query: request.query,
                provider: "boom".into(),
                results: vec![],
                answer: None,
            })
        }
    }

    #[tokio::test]
    async fn batch_degrades_a_failing_query_to_an_error_entry() {
        let t = WebSearchTool::new(Arc::new(BoomSearcher));
        let out = t
            .invoke(
                json!({ "queries": ["ok", "boom"] }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        // The good query still returns results; the bad one carries an `error`
        // instead of sinking the whole batch.
        assert_eq!(out["searches"]["ok"]["provider"], "boom");
        assert!(out["searches"]["boom"]["error"].is_string());
        assert_eq!(out["searches"]["boom"]["query"], "boom");
    }
}
