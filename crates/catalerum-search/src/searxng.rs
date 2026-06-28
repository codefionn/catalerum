//! SearXNG backend (SOUL §27).
//!
//! [SearXNG](https://docs.searxng.org) is a self-hosted, privacy-respecting
//! metasearch engine: it aggregates other engines and exposes no API key — you
//! point at your own deployment via `base_url` (same self-host story as
//! Firecrawl). Asking for `format=json` over a plain GET returns
//! `{ results: [ { title, url, content, score, publishedDate } ], answers: [...] }`.

use async_trait::async_trait;
use serde_json::Value as Json;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// A SearXNG-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct SearxngSearcher {
    http: reqwest::Client,
    base_url: String,
}

impl SearxngSearcher {
    /// Build a client pointed at a SearXNG instance. A trailing `/` is trimmed
    /// so the `{base_url}/search` join is stable.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        Ok(Self { http, base_url })
    }
}

/// Map our generic freshness hint onto SearXNG's `time_range`
/// (`day`/`week`/`month`/`year`); anything else is dropped (SearXNG ignores
/// unknown values).
fn searxng_time_range(f: &str) -> Option<&'static str> {
    match f.trim().to_ascii_lowercase().as_str() {
        "day" | "d" => Some("day"),
        "week" | "w" => Some("week"),
        "month" | "m" => Some("month"),
        "year" | "y" => Some("year"),
        _ => None,
    }
}

#[async_trait]
impl WebSearcher for SearxngSearcher {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        let url = format!("{}/search", self.base_url);
        let mut query: Vec<(&str, String)> =
            vec![("q", request.query.clone()), ("format", "json".to_string())];
        if let Some(tr) = request.freshness.as_deref().and_then(searxng_time_range) {
            query.push(("time_range", tr.to_string()));
        }

        let resp = self
            .http
            .get(&url)
            .query(&query)
            .send()
            .await
            .map_err(|e| Error::provider(format!("searxng request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "searxng returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding searxng response: {e}")))?;
        // SearXNG returns a whole page of aggregated hits with no count knob, so
        // truncate locally to honor the request limit.
        let mut results = parse_searxng(&request.query, &value);
        results
            .results
            .truncate(request.limit.clamp(1, 20) as usize);
        Ok(results)
    }
}

/// Parse SearXNG's `{ results: [...], answers: [...] }` body into
/// [`SearchResults`].
fn parse_searxng(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("results")
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(searxng_hit).collect())
        .unwrap_or_default();
    let answer = value
        .get("answers")
        .and_then(Json::as_array)
        .and_then(|arr| {
            arr.iter()
                .filter_map(Json::as_str)
                .find(|s| !s.trim().is_empty())
        })
        .map(str::to_string);
    SearchResults {
        query: query.to_string(),
        provider: "searxng".to_string(),
        results,
        answer,
    }
}

fn searxng_hit(item: &Json) -> Option<SearchHit> {
    let url = item.get("url").and_then(Json::as_str)?.to_string();
    let title = item
        .get("title")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let snippet = item
        .get("content")
        .and_then(Json::as_str)
        .map(str::to_string);
    let score = item.get("score").and_then(Json::as_f64);
    let published = item
        .get("publishedDate")
        .and_then(Json::as_str)
        .map(str::to_string);
    Some(SearchHit {
        title,
        url,
        snippet,
        raw_content: None,
        score,
        published,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Json {
        json!({
            "query": "rust",
            "results": [
                {
                    "title": "Rust",
                    "url": "https://www.rust-lang.org",
                    "content": "A language empowering everyone.",
                    "score": 1.2,
                    "publishedDate": "2024-01-01",
                    "engine": "google"
                },
                { "title": "no url", "content": "skipped" }
            ],
            "answers": ["", "Rust is a systems language."],
            "suggestions": ["rustup"]
        })
    }

    #[test]
    fn parses_results_and_answer() {
        let res = parse_searxng("rust", &sample());
        assert_eq!(res.provider, "searxng");
        assert_eq!(res.query, "rust");
        assert_eq!(res.answer.as_deref(), Some("Rust is a systems language."));
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.title, "Rust");
        assert_eq!(
            hit.snippet.as_deref(),
            Some("A language empowering everyone.")
        );
        assert_eq!(hit.score, Some(1.2));
        assert_eq!(hit.published.as_deref(), Some("2024-01-01"));
        assert!(hit.raw_content.is_none());
    }

    #[test]
    fn empty_body_is_empty_results() {
        let res = parse_searxng("x", &json!({}));
        assert!(res.results.is_empty());
        assert!(res.answer.is_none());
    }

    #[test]
    fn time_range_maps_known_values_only() {
        assert_eq!(searxng_time_range("week"), Some("week"));
        assert_eq!(searxng_time_range("m"), Some("month"));
        assert_eq!(searxng_time_range("decade"), None);
    }

    #[test]
    fn new_trims_trailing_slash() {
        let s = SearxngSearcher::new("https://searx.example.org/").unwrap();
        assert_eq!(s.base_url, "https://searx.example.org");
    }
}
