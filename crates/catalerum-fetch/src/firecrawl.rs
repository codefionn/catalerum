//! Firecrawl scrape backend (SOUL §27).
//!
//! [Firecrawl](https://firecrawl.dev) renders JavaScript and returns clean
//! Markdown for a URL. It is the one non-open-source option here, but it is also
//! self-hostable, so this client points at either the cloud
//! (`https://api.firecrawl.dev`) or a private deployment via `base_url` — same
//! request shape, credentials referenced per workspace (SOUL §13).
//!
//! Firecrawl already does HTML→Markdown server-side, so for a Markdown request we
//! pass its output straight through; for plain text we strip the Markdown locally
//! (`markdown.rs`).

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{FetchFormat, FetchRequest, FetchedPage, WebFetcher};

use crate::markdown;
use crate::policy::FetchPolicy;

/// Firecrawl cloud base URL.
pub const FIRECRAWL_CLOUD: &str = "https://api.firecrawl.dev";

/// A Firecrawl-backed [`WebFetcher`] (SOUL §27).
#[derive(Clone)]
pub struct FirecrawlFetcher {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    policy: FetchPolicy,
}

impl FirecrawlFetcher {
    /// Build a client. `base_url` defaults to the cloud when empty; `api_key` is
    /// the Firecrawl key (or any value a self-hosted deployment accepts).
    pub fn new(
        base_url: Option<&str>,
        api_key: impl Into<String>,
        policy: FetchPolicy,
    ) -> Result<Self> {
        let base_url = base_url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(FIRECRAWL_CLOUD)
            .trim_end_matches('/')
            .to_string();
        let http = reqwest::Client::builder()
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        Ok(Self {
            http,
            base_url,
            api_key: api_key.into(),
            policy,
        })
    }
}

#[async_trait]
impl WebFetcher for FirecrawlFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage> {
        // Validate the target's scheme/host even though Firecrawl does the
        // fetching — keeps the egress posture consistent (SOUL §19) — and
        // re-check after DNS resolution so a public name that resolves to an
        // internal address is refused (matches the HTTP/browser backends).
        let url = self.policy.validate(&request.url)?;
        self.policy.guard_resolved(&url).await?;

        // Ask Firecrawl for the formats we need; `markdown` doubles as the source
        // for a `Text` request.
        let want_html = request.format == FetchFormat::Html;
        let formats: Vec<&str> = if want_html {
            vec!["html"]
        } else {
            vec!["markdown"]
        };
        let body = json!({
            "url": request.url,
            "formats": formats,
            "onlyMainContent": request.main_content_only,
        });

        let url = format!("{}/v1/scrape", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::provider(format!("firecrawl request failed: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::provider(format!(
                "firecrawl returned {status}: {}",
                text.trim()
            )));
        }

        let value: Json = resp
            .json()
            .await
            .map_err(|e| Error::provider(format!("decoding firecrawl response: {e}")))?;
        parse_scrape(&request, value, self.policy.max_bytes)
    }
}

/// Parse Firecrawl's `{ success, data: { markdown, html, metadata } }` body into
/// a [`FetchedPage`]. The returned content is capped at `max_bytes` so a large
/// (or hostile self-hosted) response can't blow the LLM's context (SOUL §27).
fn parse_scrape(request: &FetchRequest, value: Json, max_bytes: u64) -> Result<FetchedPage> {
    if value.get("success") == Some(&Json::Bool(false)) {
        let msg = value
            .get("error")
            .and_then(Json::as_str)
            .unwrap_or("unknown error");
        return Err(Error::provider(format!("firecrawl: {msg}")));
    }
    let data = value
        .get("data")
        .ok_or_else(|| Error::provider("firecrawl response missing `data`"))?;

    let meta = data.get("metadata");
    let title = meta
        .and_then(|m| m.get("title"))
        .and_then(Json::as_str)
        .map(str::to_string);
    let status = meta
        .and_then(|m| m.get("statusCode"))
        .and_then(Json::as_u64)
        .unwrap_or(200) as u16;
    let final_url = meta
        .and_then(|m| m.get("sourceURL").or_else(|| m.get("url")))
        .and_then(Json::as_str)
        .unwrap_or(&request.url)
        .to_string();
    let content_type = meta
        .and_then(|m| m.get("contentType"))
        .and_then(Json::as_str)
        .map(str::to_string);

    // Cap the received content at the policy byte budget (char-boundary safe) so
    // a large or hostile self-hosted response can't blow the LLM's context.
    let html = crate::policy::cap_str(
        data.get("html").and_then(Json::as_str).unwrap_or_default(),
        max_bytes,
    );
    let md = crate::policy::cap_str(
        data.get("markdown")
            .and_then(Json::as_str)
            .unwrap_or_default(),
        max_bytes,
    );

    // `raw_bytes` is the original HTML size when known; Firecrawl returns Markdown
    // directly, so for a Markdown/Text request it stays 0 (context ratio = None).
    let (content, raw_bytes) = match request.format {
        FetchFormat::Html => (html.to_string(), html.len() as u64),
        FetchFormat::Markdown => (md.to_string(), 0),
        FetchFormat::Text => (markdown::markdown_to_text(md), 0),
    };
    let content_bytes = content.len() as u64;

    Ok(FetchedPage {
        url: final_url,
        status,
        title,
        content_type,
        content,
        format: request.format,
        raw_bytes,
        content_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc_response() -> Json {
        json!({
            "success": true,
            "data": {
                "markdown": "# Title\n\nBody text.",
                "metadata": {
                    "title": "Title",
                    "statusCode": 200,
                    "sourceURL": "https://example.com/article"
                }
            }
        })
    }

    const CAP: u64 = 5 * 1024 * 1024;

    #[test]
    fn parses_markdown_scrape() {
        let req = FetchRequest::new("https://example.com/article");
        let page = parse_scrape(&req, fc_response(), CAP).unwrap();
        assert_eq!(page.content, "# Title\n\nBody text.");
        assert_eq!(page.title.as_deref(), Some("Title"));
        assert_eq!(page.status, 200);
        assert_eq!(page.url, "https://example.com/article");
        assert_eq!(page.format, FetchFormat::Markdown);
    }

    #[test]
    fn text_format_strips_markdown() {
        let req = FetchRequest::new("https://example.com/article").format(FetchFormat::Text);
        let page = parse_scrape(&req, fc_response(), CAP).unwrap();
        assert_eq!(page.content, "Title\nBody text.");
    }

    #[test]
    fn surfaces_firecrawl_failure() {
        let req = FetchRequest::new("https://example.com/");
        let v = json!({ "success": false, "error": "rate limited" });
        let err = parse_scrape(&req, v, CAP).unwrap_err();
        assert!(err.to_string().contains("rate limited"));
    }

    #[test]
    fn content_is_capped_at_max_bytes() {
        let big = "x".repeat(10_000);
        let v = json!({ "success": true, "data": { "markdown": big } });
        let req = FetchRequest::new("https://example.com/");
        let page = parse_scrape(&req, v, 100).unwrap();
        assert_eq!(page.content.len(), 100);
    }
}
