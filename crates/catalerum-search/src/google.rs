//! Google Programmable Search Engine backend (SOUL §27).
//!
//! [Google PSE / Custom Search](https://developers.google.com/custom-search/v1/overview)
//! exposes a curated [Programmable Search Engine](https://programmablesearchengine.google.com)
//! over a plain authenticated GET: an API `key` plus the engine id `cx`. The free
//! tier allows 100 queries/day. Results come back as
//! `{ items: [ { title, link, snippet, displayLink } ] }` — note the hit URL field
//! is `link`, and `items` is absent entirely when there are zero results.

use async_trait::async_trait;
use serde_json::Value as Json;

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{SearchHit, SearchRequest, SearchResults, WebSearcher};

/// Google Custom Search JSON API base.
pub const GOOGLE_API: &str = "https://www.googleapis.com/customsearch/v1";

/// A Google PSE-backed [`WebSearcher`] (SOUL §27).
#[derive(Clone)]
pub struct GoogleSearcher {
    http: reqwest::Client,
    api_key: String,
    cx: String,
}

impl GoogleSearcher {
    /// Build a client from an API `key` and a Programmable Search Engine id `cx`.
    pub fn new(api_key: impl Into<String>, cx: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            cx: cx.into(),
        })
    }
}

/// Map our generic freshness hint onto Google's `dateRestrict` codes (`d1`/`w1`/
/// `m1`/`y1`); anything else is dropped (Google ignores unknown values, but we
/// keep the request clean).
fn google_date_restrict(f: &str) -> Option<&'static str> {
    match f.trim().to_ascii_lowercase().as_str() {
        "day" | "d" => Some("d1"),
        "week" | "w" => Some("w1"),
        "month" | "m" => Some("m1"),
        "year" | "y" => Some("y1"),
        _ => None,
    }
}

#[async_trait]
impl WebSearcher for GoogleSearcher {
    fn name(&self) -> &str {
        "google"
    }

    async fn search(&self, request: SearchRequest) -> Result<SearchResults> {
        // Google CSE caps `num` at 10 results per request.
        let num = request.limit.clamp(1, 10).to_string();
        let mut query: Vec<(&str, String)> = vec![
            ("key", self.api_key.clone()),
            ("cx", self.cx.clone()),
            ("q", request.query.clone()),
            ("num", num),
        ];
        if let Some(dr) = request.freshness.as_deref().and_then(google_date_restrict) {
            query.push(("dateRestrict", dr.to_string()));
        }

        let resp = self
            .http
            .get(GOOGLE_API)
            .query(&query)
            .send()
            .await
            .map_err(|e| Error::provider(format!("google request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "google returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding google response: {e}")))?;
        Ok(parse_google(&request.query, &value))
    }
}

/// Parse Google's `{ items: [...] }` body into [`SearchResults`].
fn parse_google(query: &str, value: &Json) -> SearchResults {
    let results = value
        .get("items")
        .and_then(Json::as_array)
        .map(|arr| arr.iter().filter_map(google_hit).collect())
        .unwrap_or_default();
    SearchResults {
        query: query.to_string(),
        provider: "google".to_string(),
        results,
        answer: None,
    }
}

fn google_hit(item: &Json) -> Option<SearchHit> {
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
    Some(SearchHit {
        title,
        url,
        snippet,
        raw_content: None,
        score: None,
        published: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Json {
        json!({
            "items": [
                {
                    "title": "Rust",
                    "link": "https://www.rust-lang.org",
                    "snippet": "A language empowering everyone.",
                    "displayLink": "www.rust-lang.org"
                },
                {
                    "title": "No link dropped",
                    "snippet": "missing link -> skipped"
                }
            ],
            "searchInformation": { "totalResults": "123" }
        })
    }

    #[test]
    fn parses_results_and_skips_urlless() {
        let res = parse_google("rust", &sample());
        assert_eq!(res.provider, "google");
        assert_eq!(res.query, "rust");
        assert_eq!(res.results.len(), 1);
        let hit = &res.results[0];
        assert_eq!(hit.url, "https://www.rust-lang.org");
        assert_eq!(hit.title, "Rust");
        assert_eq!(
            hit.snippet.as_deref(),
            Some("A language empowering everyone.")
        );
        assert_eq!(hit.score, None);
        assert_eq!(hit.published, None);
    }

    #[test]
    fn empty_body_is_empty_results() {
        let res = parse_google("x", &json!({}));
        assert!(res.results.is_empty());
        assert!(res.answer.is_none());
    }

    #[test]
    fn date_restrict_maps_known_values_only() {
        assert_eq!(google_date_restrict("week"), Some("w1"));
        assert_eq!(google_date_restrict("year"), Some("y1"));
        assert_eq!(google_date_restrict("decade"), None);
    }
}
