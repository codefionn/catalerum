//! SerpAPI backend (SOUL §27).
//!
//! [SerpAPI](https://serpapi.com) scrapes the *real* Google/Bing SERP and hands
//! it back as structured JSON, so results match what a human sees in the browser
//! (`organic_results`, plus an `answer_box` for featured snippets). Auth is the
//! `api_key` query parameter on a plain GET; `engine` selects which SERP to
//! scrape (`google`, `bing`, …) and recency rides Google's `tbs=qdr:…` filter.

use async_trait::async_trait;
use serde_json::Value as Json;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// SerpAPI search endpoint (JSON response).
pub const SERPAPI_API: &str = "https://serpapi.com/search.json";

/// A SerpAPI-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct SerpApiSearcher {
    http: reqwest::Client,
    api_key: String,
    engine: String,
}

impl SerpApiSearcher {
    /// Build a client from a SerpAPI key and SERP `engine`. A blank `engine`
    /// defaults to `google` (config supplies the default, but guard here too).
    pub fn new(api_key: impl Into<String>, engine: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        let engine = engine.into();
        let engine = if engine.trim().is_empty() {
            "google".to_string()
        } else {
            engine
        };
        Ok(Self {
            http,
            api_key: api_key.into(),
            engine,
        })
    }
}

/// Map our generic freshness hint onto Google's `tbs=qdr:…` recency codes
/// (`qdr:d`/`qdr:w`/`qdr:m`/`qdr:y`); anything else is dropped (an unknown
/// `tbs` would silently break the query).
fn serpapi_tbs(f: &str) -> Option<String> {
    match f.trim().to_ascii_lowercase().as_str() {
        "day" | "d" => Some("qdr:d".to_string()),
        "week" | "w" => Some("qdr:w".to_string()),
        "month" | "m" => Some("qdr:m".to_string()),
        "year" | "y" => Some("qdr:y".to_string()),
        _ => None,
    }
}

#[async_trait]
impl WebSearcher for SerpApiSearcher {
    fn name(&self) -> &str {
        "serpapi"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        let num = request.limit.clamp(1, 20).to_string();
        let mut query: Vec<(&str, String)> = vec![
            ("engine", self.engine.clone()),
            ("q", request.query.clone()),
            ("api_key", self.api_key.clone()),
            ("num", num),
        ];
        if let Some(tbs) = request.freshness.as_deref().and_then(serpapi_tbs) {
            query.push(("tbs", tbs));
        }

        let resp = self
            .http
            .get(SERPAPI_API)
            .query(&query)
            .send()
            .await
            .map_err(|e| Error::provider(format!("serpapi request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "serpapi returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding serpapi response: {e}")))?;
        Ok(parse_serpapi(&request.query, &value))
    }
}

/// Parse SerpAPI's `{ organic_results: [...], answer_box: {...} }` body into
/// [`SearchResults`].
fn parse_serpapi(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("organic_results")
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(serpapi_hit).collect())
        .unwrap_or_default();
    // Featured-snippet box: prefer the direct `answer`, fall back to its
    // `snippet`, taking the first non-empty of the two.
    let answer = value.get("answer_box").and_then(|b| {
        ["answer", "snippet"]
            .into_iter()
            .filter_map(|k| b.get(k).and_then(Json::as_str))
            .find(|s| !s.trim().is_empty())
            .map(str::to_string)
    });
    SearchResults {
        query: query.to_string(),
        provider: "serpapi".to_string(),
        results,
        answer,
    }
}

fn serpapi_hit(item: &Json) -> Option<SearchHit> {
    let url = item.get("link").and_then(Json::as_str)?.to_string();
    let title = item
        .get("title")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let snippet = item
        .get("snippet")
        .and_then(Json::as_str)
        .map(str::to_string);
    let published = item.get("date").and_then(Json::as_str).map(str::to_string);
    Some(SearchHit {
        title,
        url,
        snippet,
        raw_content: None,
        score: None,
        published,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Json {
        json!({
            "organic_results": [
                {
                    "position": 1,
                    "title": "Rust",
                    "link": "https://www.rust-lang.org",
                    "snippet": "A language empowering everyone.",
                    "date": "Jan 1, 2024",
                    "source": "rust-lang.org"
                },
                {
                    "position": 2,
                    "title": "no link dropped",
                    "snippet": "missing link -> skipped"
                }
            ],
            "answer_box": {
                "answer": "Rust is a systems programming language.",
                "snippet": "fallback snippet"
            }
        })
    }

    #[test]
    fn parses_results_and_skips_linkless() {
        let res = parse_serpapi("rust", &sample());
        assert_eq!(res.provider, "serpapi");
        assert_eq!(res.query, "rust");
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.title, "Rust");
        assert_eq!(
            hit.snippet.as_deref(),
            Some("A language empowering everyone.")
        );
        assert_eq!(hit.published.as_deref(), Some("Jan 1, 2024"));
        assert_eq!(hit.score, None);
        assert!(hit.raw_content.is_none());
    }

    #[test]
    fn answer_prefers_answer_then_snippet() {
        let res = parse_serpapi("rust", &sample());
        assert_eq!(
            res.answer.as_deref(),
            Some("Rust is a systems programming language.")
        );

        let snippet_only = json!({ "answer_box": { "snippet": "just a snippet" } });
        assert_eq!(
            parse_serpapi("x", &snippet_only).answer.as_deref(),
            Some("just a snippet")
        );

        let blank_answer = json!({ "answer_box": { "answer": "  ", "snippet": "use me" } });
        assert_eq!(
            parse_serpapi("x", &blank_answer).answer.as_deref(),
            Some("use me")
        );
    }

    #[test]
    fn empty_body_is_empty_results() {
        let res = parse_serpapi("x", &json!({}));
        assert!(res.results.is_empty());
        assert!(res.answer.is_none());
    }

    #[test]
    fn freshness_maps_to_tbs_codes() {
        assert_eq!(serpapi_tbs("day"), Some("qdr:d".to_string()));
        assert_eq!(serpapi_tbs("WEEK"), Some("qdr:w".to_string()));
        assert_eq!(serpapi_tbs("month"), Some("qdr:m".to_string()));
        assert_eq!(serpapi_tbs("year"), Some("qdr:y".to_string()));
        assert_eq!(serpapi_tbs("decade"), None);
    }

    #[test]
    fn new_defaults_blank_engine_to_google() {
        let s = SerpApiSearcher::new("k", "   ").unwrap();
        assert_eq!(s.engine, "google");
        let s = SerpApiSearcher::new("k", "bing").unwrap();
        assert_eq!(s.engine, "bing");
    }
}
