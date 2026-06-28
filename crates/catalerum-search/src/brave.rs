//! Brave Search backend (SOUL §27).
//!
//! [Brave Search](https://brave.com/search/api/) serves an **independent** web
//! index (not a Google/Bing reseller) over a plain authenticated GET. Auth is the
//! `X-Subscription-Token` header; results come back as
//! `{ web: { results: [ { title, url, description, age } ] } }`.

use async_trait::async_trait;
use serde_json::Value as Json;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// Brave Search API base.
pub const BRAVE_API: &str = "https://api.search.brave.com/res/v1/web/search";

/// A Brave-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct BraveSearcher {
    http: reqwest::Client,
    api_key: String,
}

impl BraveSearcher {
    /// Build a client from a Brave subscription token.
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

/// Map our generic freshness hint onto Brave's `freshness` codes (`pd`/`pw`/
/// `pm`/`py`). An unrecognized value passes through (Brave also accepts a
/// `YYYY-MM-DDtoYYYY-MM-DD` range).
fn brave_freshness(f: &str) -> String {
    match f.trim().to_ascii_lowercase().as_str() {
        "day" | "d" | "pd" => "pd".to_string(),
        "week" | "w" | "pw" => "pw".to_string(),
        "month" | "m" | "pm" => "pm".to_string(),
        "year" | "y" | "py" => "py".to_string(),
        other => other.to_string(),
    }
}

#[async_trait]
impl WebSearcher for BraveSearcher {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        let count = request.limit.clamp(1, 20).to_string();
        let mut query: Vec<(&str, String)> = vec![("q", request.query.clone()), ("count", count)];
        if let Some(f) = request.freshness.as_deref() {
            query.push(("freshness", brave_freshness(f)));
        }

        let resp = self
            .http
            .get(BRAVE_API)
            .header("Accept", "application/json")
            .header("Accept-Encoding", "gzip")
            .header("X-Subscription-Token", &self.api_key)
            .query(&query)
            .send()
            .await
            .map_err(|e| Error::provider(format!("brave request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "brave returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding brave response: {e}")))?;
        Ok(parse_brave(&request.query, &value))
    }
}

/// Parse Brave's `{ web: { results: [...] } }` body into [`SearchResults`].
fn parse_brave(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(brave_hit).collect())
        .unwrap_or_default();
    SearchResults {
        query: query.to_string(),
        provider: "brave".to_string(),
        results,
        answer: None,
    }
}

fn brave_hit(item: &Json) -> Option<SearchHit> {
    let url = item.get("url").and_then(Json::as_str)?.to_string();
    let title = item
        .get("title")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    let snippet = item
        .get("description")
        .and_then(Json::as_str)
        .map(str::to_string);
    let published = item
        .get("age")
        .or_else(|| item.get("page_age"))
        .and_then(Json::as_str)
        .map(str::to_string);
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
            "web": {
                "results": [
                    {
                        "title": "Rust",
                        "url": "https://www.rust-lang.org",
                        "description": "A language empowering everyone.",
                        "age": "January 1, 2024"
                    },
                    {
                        "title": "No URL dropped",
                        "description": "missing url -> skipped"
                    }
                ]
            }
        })
    }

    #[test]
    fn parses_results_and_skips_urlless() {
        let res = parse_brave("rust", &sample());
        assert_eq!(res.provider, "brave");
        assert_eq!(res.query, "rust");
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.title, "Rust");
        assert_eq!(
            hit.snippet.as_deref(),
            Some("A language empowering everyone.")
        );
        assert_eq!(hit.published.as_deref(), Some("January 1, 2024"));
    }

    #[test]
    fn empty_body_is_empty_results() {
        let res = parse_brave("x", &json!({}));
        assert!(res.results.is_empty());
        assert!(res.answer.is_none());
    }

    #[test]
    fn freshness_maps_to_brave_codes() {
        assert_eq!(brave_freshness("week"), "pw");
        assert_eq!(brave_freshness("MONTH"), "pm");
        assert_eq!(
            brave_freshness("2023-01-01to2023-02-01"),
            "2023-01-01to2023-02-01"
        );
    }
}
