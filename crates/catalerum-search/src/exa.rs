//! Exa backend (SOUL §27).
//!
//! [Exa](https://exa.ai) runs a **neural** (embeddings) index alongside a
//! classic keyword one — `type: "auto"` lets Exa pick per query — over a single
//! POST. Auth is the `x-api-key` header; results come back as
//! `{ results: [ { title, url, score, publishedDate, text, highlights } ] }`.
//! Unlike Tavily there is no synthesized `answer` on `/search`, and per-result
//! excerpts arrive as a `highlights` array we stitch into one snippet.

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// Exa search endpoint.
pub const EXA_API: &str = "https://api.exa.ai/search";

/// An Exa-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct ExaSearcher {
    http: reqwest::Client,
    api_key: String,
}

impl ExaSearcher {
    /// Build a client from an Exa API key.
    pub fn new(api_key: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        Ok(Self {
            http,
            api_key: api_key.into(),
        })
    }
}

#[async_trait]
impl WebSearcher for ExaSearcher {
    fn name(&self) -> &str {
        "exa"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        // Freshness is dropped: Exa filters recency via absolute ISO
        // `startPublishedDate` params, which need date math our generic hint
        // (`day`/`week`/…) doesn't carry.
        let body = json!({
            "query": request.query,
            "numResults": request.limit.clamp(1, 20),
            "type": "auto",
            "contents": {
                "text": request.include_raw_content,
                "highlights": true,
            },
        });

        let resp = self
            .http
            .post(EXA_API)
            .header("x-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("exa request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "exa returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding exa response: {e}")))?;
        Ok(parse_exa(&request.query, &value))
    }
}

/// Parse Exa's `{ results: [...] }` body into [`SearchResults`]. Exa has no
/// synthesized `answer` on `/search`, so it is always `None`.
fn parse_exa(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("results")
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(exa_hit).collect())
        .unwrap_or_default();
    SearchResults {
        query: query.to_string(),
        provider: "exa".to_string(),
        results,
        answer: None,
    }
}

fn exa_hit(item: &Json) -> Option<SearchHit> {
    let url = item.get("url").and_then(Json::as_str)?.to_string();
    let title = item
        .get("title")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let snippet = exa_snippet(item);
    let raw_content = item
        .get("text")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
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
        raw_content,
        score,
        published,
    })
}

/// Stitch Exa's `highlights` (the most relevant sentences) into one snippet,
/// dropping blanks; `None` when there are no usable highlights.
fn exa_snippet(item: &Json) -> Option<String> {
    let joined = item
        .get("highlights")
        .and_then(Json::as_array)?
        .iter()
        .filter_map(Json::as_str)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" … ");
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Json {
        json!({
            "results": [
                {
                    "title": "Rust",
                    "url": "https://www.rust-lang.org",
                    "score": 0.83,
                    "publishedDate": "2024-01-01",
                    "text": "Rust is a systems programming language.",
                    "highlights": ["A language empowering everyone.", "Fearless concurrency."]
                },
                { "title": "no url", "highlights": ["skipped"] }
            ],
            "autopromptString": "rust programming"
        })
    }

    #[test]
    fn parses_results_and_skips_urlless() {
        let res = parse_exa("rust", &sample());
        assert_eq!(res.provider, "exa");
        assert_eq!(res.query, "rust");
        assert!(res.answer.is_none());
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.title, "Rust");
        assert_eq!(
            hit.snippet.as_deref(),
            Some("A language empowering everyone. … Fearless concurrency.")
        );
        assert_eq!(
            hit.raw_content.as_deref(),
            Some("Rust is a systems programming language.")
        );
        assert_eq!(hit.score, Some(0.83));
        assert_eq!(hit.published.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn missing_highlights_yield_no_snippet() {
        let res = parse_exa(
            "x",
            &json!({ "results": [{ "url": "https://example.com" }] }),
        );
        assert_eq!(res.results.len(), 1);
        assert!(res.results[0].snippet.is_none());
        assert!(res.results[0].raw_content.is_none());
    }

    #[test]
    fn empty_body_is_empty_results() {
        let res = parse_exa("x", &json!({}));
        assert!(res.results.is_empty());
        assert!(res.answer.is_none());
    }
}
