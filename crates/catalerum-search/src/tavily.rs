//! Tavily backend (SOUL §27).
//!
//! [Tavily](https://tavily.com) is a search API built for LLM/RAG use: a single
//! POST returns ranked results with content excerpts and, optionally, a
//! synthesized `answer` — the cleanest agent payload of the bunch. Auth is an
//! `Authorization: Bearer tvly-…` header; the request and the recency
//! (`time_range`) / full-text (`include_raw_content`) knobs map straight onto our
//! [`SearchRequest`].

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// Tavily search endpoint.
pub const TAVILY_API: &str = "https://api.tavily.com/search";

/// A Tavily-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct TavilySearcher {
    http: reqwest::Client,
    api_key: String,
}

impl TavilySearcher {
    /// Build a client from a Tavily API key (`tvly-…`).
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

/// Map our generic freshness hint onto Tavily's `time_range`
/// (`day`/`week`/`month`/`year`); anything else is dropped (Tavily rejects
/// unknown values).
fn tavily_time_range(f: &str) -> Option<&'static str> {
    match f.trim().to_ascii_lowercase().as_str() {
        "day" | "d" => Some("day"),
        "week" | "w" => Some("week"),
        "month" | "m" => Some("month"),
        "year" | "y" => Some("year"),
        _ => None,
    }
}

#[async_trait]
impl WebSearcher for TavilySearcher {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        let mut body = json!({
            "query": request.query,
            "max_results": request.limit.clamp(1, 20),
            "include_answer": true,
            "include_raw_content": request.include_raw_content,
        });
        if let Some(tr) = request.freshness.as_deref().and_then(tavily_time_range) {
            body["time_range"] = json!(tr);
        }

        let resp = self
            .http
            .post(TAVILY_API)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("tavily request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "tavily returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding tavily response: {e}")))?;
        Ok(parse_tavily(&request.query, &value))
    }
}

/// Parse Tavily's `{ answer, results: [...] }` body into [`SearchResults`].
fn parse_tavily(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("results")
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(tavily_hit).collect())
        .unwrap_or_default();
    let answer = value
        .get("answer")
        .and_then(Json::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    SearchResults {
        query: query.to_string(),
        provider: "tavily".to_string(),
        results,
        answer,
    }
}

fn tavily_hit(item: &Json) -> Option<SearchHit> {
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
    let raw_content = item
        .get("raw_content")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let score = item.get("score").and_then(Json::as_f64);
    let published = item
        .get("published_date")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Json {
        json!({
            "query": "rust",
            "answer": "Rust is a systems language.",
            "results": [
                {
                    "title": "Rust",
                    "url": "https://www.rust-lang.org",
                    "content": "A language empowering everyone.",
                    "score": 0.97,
                    "published_date": "2024-01-01"
                },
                { "title": "no url", "content": "skipped" }
            ]
        })
    }

    #[test]
    fn parses_results_and_answer() {
        let res = parse_tavily("rust", &sample());
        assert_eq!(res.provider, "tavily");
        assert_eq!(res.answer.as_deref(), Some("Rust is a systems language."));
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.score, Some(0.97));
        assert_eq!(hit.published.as_deref(), Some("2024-01-01"));
    }

    #[test]
    fn blank_answer_becomes_none() {
        let res = parse_tavily("x", &json!({ "answer": "  ", "results": [] }));
        assert!(res.answer.is_none());
        assert!(res.results.is_empty());
    }

    #[test]
    fn time_range_maps_known_values_only() {
        assert_eq!(tavily_time_range("week"), Some("week"));
        assert_eq!(tavily_time_range("decade"), None);
    }
}
