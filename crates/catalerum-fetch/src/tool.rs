//! The `fetch_url` LLM tool (SOUL §7, §27).
//!
//! A thin [`Tool`] wrapper over a [`WebFetcher`]: the model passes a URL (and
//! optionally a format/mode) and gets back clean Markdown plus a little
//! metadata. Like every tool it is a client of a scoped capability — `web:read`
//! — enforced at the API choke point, never by the model (SOUL §3.3, §19). The
//! Markdown default is deliberate: it keeps the result cheap in context (§27).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value as Json};

use catalerum_core::capability::{Action, Capability, Resource};
use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{
    FetchFormat, FetchMode, FetchRequest, WebFetcher, WebhookBody, WebhookDelivery, WebhookMethod,
    WebhookSender,
};
use catalerum_core::tool::{Tool, ToolContext};

use crate::extract::{extract_html, ExtractField};
use crate::markdown::{extract_title, html_to_markdown, html_to_text, MarkdownOptions};

/// The `fetch_url` tool — fetch a web page as AI-friendly Markdown.
pub struct FetchUrlTool {
    fetcher: Arc<dyn WebFetcher>,
}

impl FetchUrlTool {
    /// Wrap a fetcher as the `fetch_url` tool.
    #[must_use]
    pub fn new(fetcher: Arc<dyn WebFetcher>) -> Self {
        Self { fetcher }
    }
}

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "fetch_url"
    }

    fn required_capability(&self) -> Option<Capability> {
        // Web egress is a scoped read (SOUL §19/§27): the API choke point must
        // hold `web:read` to dispatch this, so a narrower grant can deny it.
        Some(Capability::new(Action::Read, Resource::domain("web")))
    }

    fn description(&self) -> &str {
        "Fetch ONE selected web page and return its main content as clean Markdown. \
         Full pages can be long: first narrow candidates with web_search, then fetch \
         only pages you will actually use. Use `format` for html/text and \
         `mode=browser` for JavaScript-heavy pages."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to fetch."
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "html", "text"],
                    "default": "markdown",
                    "description": "Representation to return. Markdown is cheapest in context."
                },
                "mode": {
                    "type": "string",
                    "enum": ["auto", "http", "browser"],
                    "default": "auto",
                    "description": "auto = plain GET; browser renders JavaScript first."
                },
                "main_content_only": {
                    "type": "boolean",
                    "default": true,
                    "description": "Drop nav/header/footer/boilerplate before converting."
                }
            },
            "required": ["url"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let url = args
            .get("url")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`url` is required"))?
            .to_string();

        let mut request = FetchRequest::new(url);
        if let Some(f) = args.get("format").and_then(Json::as_str) {
            request.format = parse_format(f)?;
        }
        if let Some(m) = args.get("mode").and_then(Json::as_str) {
            request.mode = parse_mode(m)?;
        }
        if let Some(b) = args.get("main_content_only").and_then(Json::as_bool) {
            request.main_content_only = b;
        }

        let page = self.fetcher.fetch(request).await?;
        // Hand the model the content plus light provenance; keep it compact.
        Ok(json!({
            "url": page.url,
            "status": page.status,
            "title": page.title,
            "format": page.format,
            "content": page.content,
            "raw_bytes": page.raw_bytes,
            "content_bytes": page.content_bytes,
        }))
    }
}

fn parse_format(s: &str) -> Result<FetchFormat> {
    match s {
        "markdown" | "md" => Ok(FetchFormat::Markdown),
        "html" => Ok(FetchFormat::Html),
        "text" | "plain" => Ok(FetchFormat::Text),
        other => Err(Error::invalid(format!("unknown format `{other}`"))),
    }
}

fn parse_mode(s: &str) -> Result<FetchMode> {
    match s {
        "auto" => Ok(FetchMode::Auto),
        "http" => Ok(FetchMode::Http),
        "browser" => Ok(FetchMode::Browser),
        other => Err(Error::invalid(format!("unknown mode `{other}`"))),
    }
}

/// The `send_webhook` tool — deliver a payload to an external webhook URL
/// (SOUL §11/§27).
///
/// A thin [`Tool`] wrapper over a [`WebhookSender`]: the model (or a `Webhook`
/// automation action) names a URL, a JSON `payload` (or raw `body` +
/// `content_type`), optional `headers` (e.g. an Authorization bearer or a
/// signature), and a method. Egress-**write**, so it gates on `web:write` —
/// the counterpart to `fetch_url`'s `web:read` — and the sender enforces the
/// same SSRF guard as fetching (redirects are never followed). A non-2xx
/// receiver status is a tool **error** (a delivery that wasn't acknowledged
/// must fail the step, not silently advance the run).
pub struct SendWebhookTool {
    sender: Arc<dyn WebhookSender>,
}

impl SendWebhookTool {
    /// Wrap a sender as the `send_webhook` tool.
    #[must_use]
    pub fn new(sender: Arc<dyn WebhookSender>) -> Self {
        Self { sender }
    }
}

#[async_trait]
impl Tool for SendWebhookTool {
    fn name(&self) -> &str {
        "send_webhook"
    }

    fn required_capability(&self) -> Option<Capability> {
        // Outbound delivery is a scoped egress WRITE (SOUL §19/§27): the API
        // choke point must hold `web:write` to dispatch this, so a read-only
        // (or narrower) grant can deny it while still allowing `fetch_url`.
        Some(Capability::new(Action::Write, Resource::domain("web")))
    }

    fn description(&self) -> &str {
        "Deliver a payload to an external webhook URL (HTTP POST by default). \
         Pass a JSON `payload` (sent as application/json), or a raw string \
         `body` plus `content_type` for non-JSON receivers. Add `headers` for \
         auth/signatures. Fails on a non-2xx response; redirects are not \
         followed."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "Absolute http(s) URL to deliver to."
                },
                "payload": {
                    "description": "JSON payload, sent as application/json (any JSON value). Mutually exclusive with `body`; omitting both sends an empty JSON object."
                },
                "body": {
                    "type": "string",
                    "description": "Raw string body for non-JSON receivers (form-encoded, XML, plain text). Mutually exclusive with `payload`."
                },
                "content_type": {
                    "type": "string",
                    "default": "text/plain",
                    "description": "Content-Type for a raw `body`. Ignored with `payload` (always application/json)."
                },
                "headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "Extra request headers, e.g. {\"Authorization\": \"Bearer …\", \"X-Signature\": \"…\"}. Transport headers (Host, Content-Length, Content-Type, …) are refused."
                },
                "method": {
                    "type": "string",
                    "enum": ["post", "put", "patch"],
                    "default": "post",
                    "description": "HTTP method. Deliveries are writes; to READ a URL use fetch_url."
                },
                "timeout_secs": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Per-delivery timeout override, in seconds (capped at 300)."
                }
            },
            "required": ["url"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let url = args
            .get("url")
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::invalid("`url` is required"))?
            .to_string();

        let method = match args.get("method").and_then(Json::as_str) {
            None => WebhookMethod::Post,
            Some(m) => WebhookMethod::parse(m)
                .ok_or_else(|| Error::invalid(format!("unknown method `{m}` (post/put/patch)")))?,
        };

        let body = match (args.get("payload"), args.get("body")) {
            (Some(_), Some(_)) => {
                return Err(Error::invalid(
                    "`payload` and `body` are mutually exclusive — JSON goes in `payload`, \
                     a raw string in `body` (+ `content_type`)",
                ))
            }
            (Some(payload), None) => WebhookBody::Json(payload.clone()),
            (None, Some(raw)) => {
                let raw = raw.as_str().ok_or_else(|| {
                    Error::invalid("`body` must be a string (JSON goes in `payload`)")
                })?;
                let content_type = args
                    .get("content_type")
                    .and_then(Json::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("text/plain");
                WebhookBody::Raw {
                    body: raw.to_string(),
                    content_type: content_type.to_string(),
                }
            }
            // The webhook norm: an event ping with no payload is `{}`.
            (None, None) => WebhookBody::Json(json!({})),
        };

        let headers = match args.get("headers") {
            None => Vec::new(),
            Some(Json::Object(map)) => map
                .iter()
                .map(|(k, v)| {
                    let v = v.as_str().ok_or_else(|| {
                        Error::invalid(format!("header `{k}` must have a string value"))
                    })?;
                    Ok((k.clone(), v.to_string()))
                })
                .collect::<Result<Vec<_>>>()?,
            Some(_) => {
                return Err(Error::invalid(
                    "`headers` must be an object of string values",
                ))
            }
        };

        let delivery = WebhookDelivery {
            url,
            method,
            headers,
            body,
            timeout_secs: args.get("timeout_secs").and_then(Json::as_u64),
        };
        let resp = self.sender.deliver(delivery).await?;
        // A refusal must fail the step: an automation graph should stop (and a
        // collect cursor not advance) when the receiver didn't take delivery.
        if !resp.is_success() {
            let excerpt: String = resp.body.chars().take(500).collect();
            return Err(Error::provider(format!(
                "webhook delivery to `{}` refused: HTTP {}{}",
                resp.url,
                resp.status,
                if excerpt.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — {excerpt}")
                }
            )));
        }
        Ok(json!({
            "url": resp.url,
            "status": resp.status,
            "delivered": true,
            "content_type": resp.content_type,
            "body": resp.body,
            "body_bytes": resp.body_bytes,
        }))
    }
}

/// The `html_to_markdown` tool — convert an HTML string to clean Markdown (or
/// plain text).
///
/// A **pure, deterministic** transform: no network and no capability (it touches
/// no resource — the HTML is already in hand), so it is always available. It is the
/// companion to [`FetchUrlTool`] for graphs that obtain raw HTML elsewhere (an
/// upstream `fetch_url` with `format=html`, or an [`ExtractHtmlTool`] fragment) and
/// want it turned into cheap, readable content as a discrete step (SOUL §27).
pub struct HtmlToMarkdownTool;

#[async_trait]
impl Tool for HtmlToMarkdownTool {
    fn name(&self) -> &str {
        "html_to_markdown"
    }

    // No `required_capability`: a pure transform on data already in hand performs no
    // egress and reads no resource, so the default (`None`) is correct.

    fn description(&self) -> &str {
        "Convert an HTML string to clean Markdown (or plain text). A pure transform \
         (no network): pass `html` plus optional knobs. Use after fetching raw HTML \
         (or extracting a fragment) to make it cheap to read."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "html": {
                    "type": "string",
                    "description": "The HTML document or fragment to convert."
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text"],
                    "default": "markdown",
                    "description": "markdown (default) or text (the Markdown with syntax stripped)."
                },
                "main_content_only": {
                    "type": "boolean",
                    "default": true,
                    "description": "Drop nav/header/footer/boilerplate before converting."
                },
                "include_links": {
                    "type": "boolean",
                    "default": true,
                    "description": "Keep [text](href) links. Off keeps link text, drops the URL."
                },
                "include_images": {
                    "type": "boolean",
                    "default": true,
                    "description": "Keep ![alt](src) image references."
                },
                "base_url": {
                    "type": "string",
                    "description": "Optional base URL to resolve relative href/src against."
                }
            },
            "required": ["html"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let html = args
            .get("html")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`html` is required"))?;

        let mut opts = MarkdownOptions::default();
        if let Some(b) = args.get("main_content_only").and_then(Json::as_bool) {
            opts.main_content_only = b;
        }
        if let Some(b) = args.get("include_links").and_then(Json::as_bool) {
            opts.include_links = b;
        }
        if let Some(b) = args.get("include_images").and_then(Json::as_bool) {
            opts.include_images = b;
        }
        if let Some(u) = args.get("base_url").and_then(Json::as_str) {
            opts.base_url = Some(u.to_string());
        }

        let as_text = matches!(
            args.get("format").and_then(Json::as_str),
            Some("text" | "plain")
        );
        let content = if as_text {
            html_to_text(html, &opts)
        } else {
            html_to_markdown(html, &opts)
        };

        Ok(json!({
            "format": if as_text { "text" } else { "markdown" },
            "title": extract_title(html),
            "content": content,
            "source_bytes": html.len(),
            "content_bytes": content.len(),
        }))
    }
}

/// The `extract_html` tool — pull parts of an HTML document by CSS selector.
///
/// A **pure, deterministic** transform: no network and no capability. Given `html`
/// and a CSS `selector`, it returns each matched element's text (default), inner /
/// outer HTML, or a named attribute — so a graph can scrape a specific field out of
/// a fetched page (SOUL §27). A selector matching nothing yields an empty result; an
/// invalid selector is a clear error.
pub struct ExtractHtmlTool;

#[async_trait]
impl Tool for ExtractHtmlTool {
    fn name(&self) -> &str {
        "extract_html"
    }

    // No `required_capability`: pure transform, see [`HtmlToMarkdownTool`].

    fn description(&self) -> &str {
        "Extract parts of an HTML document by CSS selector. A pure transform (no \
         network): pass `html` and a CSS `selector`; get each match's text (default), \
         inner_html/outer_html, or a named attribute (set extract=attr, attr=href)."
    }

    fn parameters_schema(&self) -> Json {
        json!({
            "type": "object",
            "properties": {
                "html": {
                    "type": "string",
                    "description": "The HTML document to query."
                },
                "selector": {
                    "type": "string",
                    "description": "A CSS selector, e.g. 'h1', 'a.title', 'div#main p'."
                },
                "extract": {
                    "type": "string",
                    "enum": ["text", "inner_html", "outer_html", "attr"],
                    "default": "text",
                    "description": "What to pull from each match. 'attr' also needs `attr`."
                },
                "attr": {
                    "type": "string",
                    "description": "Attribute to read when extract=attr (e.g. 'href', 'src')."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Cap the number of matches returned (document order)."
                }
            },
            "required": ["html", "selector"]
        })
    }

    async fn invoke(&self, args: Json, _ctx: &ToolContext) -> Result<Json> {
        let html = args
            .get("html")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`html` is required"))?;
        let selector = args
            .get("selector")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::invalid("`selector` is required"))?;

        let field = match args.get("extract").and_then(Json::as_str).unwrap_or("text") {
            "text" => ExtractField::Text,
            "inner_html" | "html" => ExtractField::InnerHtml,
            "outer_html" => ExtractField::OuterHtml,
            "attr" | "attribute" => {
                let name = args
                    .get("attr")
                    .and_then(Json::as_str)
                    .ok_or_else(|| Error::invalid("`attr` is required when extract=attr"))?;
                ExtractField::Attr(name.to_string())
            }
            other => return Err(Error::invalid(format!("unknown extract `{other}`"))),
        };

        let limit = args.get("limit").and_then(Json::as_u64).map(|n| n as usize);

        let matches = extract_html(html, selector, &field, limit).map_err(Error::invalid)?;

        Ok(json!({
            "selector": selector,
            "count": matches.len(),
            "first": matches.first().cloned(),
            "matches": matches,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::provider::FetchedPage;

    struct StubFetcher;

    #[async_trait]
    impl WebFetcher for StubFetcher {
        async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage> {
            Ok(FetchedPage {
                url: request.url,
                status: 200,
                title: Some("Stub".into()),
                content_type: Some("text/html".into()),
                content: format!("# {:?}", request.format),
                format: request.format,
                raw_bytes: 100,
                content_bytes: 10,
            })
        }
    }

    #[tokio::test]
    async fn invokes_and_shapes_result() {
        let tool = FetchUrlTool::new(Arc::new(StubFetcher));
        let out = tool
            .invoke(
                json!({ "url": "https://example.com", "format": "text" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["title"], "Stub");
        assert_eq!(out["format"], "text");
        assert_eq!(out["content_bytes"], 10);
    }

    #[tokio::test]
    async fn missing_url_is_invalid() {
        let tool = FetchUrlTool::new(Arc::new(StubFetcher));
        let err = tool
            .invoke(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn schema_advertises_url() {
        let tool = FetchUrlTool::new(Arc::new(StubFetcher));
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"][0], "url");
        assert_eq!(tool.name(), "fetch_url");
        assert!(tool.description().contains("ONE selected web page"));
    }

    #[tokio::test]
    async fn html_to_markdown_tool_converts() {
        let tool = HtmlToMarkdownTool;
        let out = tool
            .invoke(
                json!({ "html": "<main><h1>Hi</h1><p>there</p></main>" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["format"], "markdown");
        assert_eq!(out["title"], "Hi");
        let content = out["content"].as_str().unwrap();
        assert!(content.contains("# Hi"), "got: {content}");
        assert!(content.contains("there"));
    }

    #[tokio::test]
    async fn html_to_markdown_tool_needs_html() {
        let tool = HtmlToMarkdownTool;
        let err = tool
            .invoke(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[test]
    fn pure_transform_tools_need_no_capability() {
        // Pure transforms perform no egress, so they are never capability-gated.
        assert!(HtmlToMarkdownTool.required_capability().is_none());
        assert!(ExtractHtmlTool.required_capability().is_none());
    }

    #[tokio::test]
    async fn extract_html_tool_pulls_attribute() {
        let tool = ExtractHtmlTool;
        let out = tool
            .invoke(
                json!({
                    "html": "<a href=\"/x\">one</a><a href=\"/y\">two</a>",
                    "selector": "a",
                    "extract": "attr",
                    "attr": "href"
                }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["count"], 2);
        assert_eq!(out["first"], "/x");
        assert_eq!(out["matches"][1], "/y");
    }

    #[tokio::test]
    async fn extract_html_tool_defaults_to_text() {
        let tool = ExtractHtmlTool;
        let out = tool
            .invoke(
                json!({ "html": "<h2>Heading</h2>", "selector": "h2" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["first"], "Heading");
    }

    #[tokio::test]
    async fn extract_html_tool_attr_requires_attr_name() {
        let tool = ExtractHtmlTool;
        let err = tool
            .invoke(
                json!({ "html": "<a href=\"/x\">x</a>", "selector": "a", "extract": "attr" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[tokio::test]
    async fn extract_html_tool_invalid_selector_errors() {
        let tool = ExtractHtmlTool;
        let err = tool
            .invoke(
                json!({ "html": "<a>x</a>", "selector": "a..b" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    /// A recording [`WebhookSender`]: remembers the delivery it was handed and
    /// answers with a fixed status/body.
    struct StubSender {
        status: u16,
        body: &'static str,
        seen: std::sync::Mutex<Option<WebhookDelivery>>,
    }

    impl StubSender {
        fn ok() -> Self {
            Self {
                status: 200,
                body: "{\"ok\":true}",
                seen: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl WebhookSender for StubSender {
        async fn deliver(
            &self,
            delivery: WebhookDelivery,
        ) -> Result<catalerum_core::provider::WebhookResponse> {
            let url = delivery.url.clone();
            *self.seen.lock().unwrap() = Some(delivery);
            Ok(catalerum_core::provider::WebhookResponse {
                url,
                status: self.status,
                content_type: Some("application/json".into()),
                body: self.body.to_string(),
                body_bytes: self.body.len() as u64,
            })
        }
    }

    #[tokio::test]
    async fn send_webhook_defaults_to_a_json_post() {
        let sender = Arc::new(StubSender::ok());
        let tool = SendWebhookTool::new(sender.clone());
        let out = tool
            .invoke(
                json!({ "url": "https://example.com/hook", "payload": { "n": 1 } }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(out["status"], 200);
        assert_eq!(out["delivered"], true);
        assert_eq!(out["body"], "{\"ok\":true}");

        let d = sender.seen.lock().unwrap().take().unwrap();
        assert_eq!(d.method, WebhookMethod::Post);
        assert_eq!(d.body, WebhookBody::Json(json!({ "n": 1 })));
        assert!(d.headers.is_empty());
        // Omitting both payload and body pings with an empty JSON object.
        tool.invoke(
            json!({ "url": "https://example.com/hook" }),
            &ToolContext::default(),
        )
        .await
        .unwrap();
        let d = sender.seen.lock().unwrap().take().unwrap();
        assert_eq!(d.body, WebhookBody::Json(json!({})));
    }

    #[tokio::test]
    async fn send_webhook_parses_headers_method_and_raw_body() {
        let sender = Arc::new(StubSender::ok());
        let tool = SendWebhookTool::new(sender.clone());
        tool.invoke(
            json!({
                "url": "https://example.com/hook",
                "body": "a=1",
                "content_type": "application/x-www-form-urlencoded",
                "method": "put",
                "headers": { "Authorization": "Bearer t", "X-Sig": "s" },
                "timeout_secs": 10
            }),
            &ToolContext::default(),
        )
        .await
        .unwrap();
        let d = sender.seen.lock().unwrap().take().unwrap();
        assert_eq!(d.method, WebhookMethod::Put);
        assert_eq!(d.timeout_secs, Some(10));
        assert_eq!(
            d.body,
            WebhookBody::Raw {
                body: "a=1".into(),
                content_type: "application/x-www-form-urlencoded".into()
            }
        );
        let mut headers = d.headers.clone();
        headers.sort();
        assert_eq!(
            headers,
            vec![
                ("Authorization".to_string(), "Bearer t".to_string()),
                ("X-Sig".to_string(), "s".to_string())
            ]
        );
    }

    #[tokio::test]
    async fn send_webhook_rejects_bad_args() {
        let tool = SendWebhookTool::new(Arc::new(StubSender::ok()));
        // No url.
        let err = tool
            .invoke(json!({}), &ToolContext::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        // payload XOR body.
        let err = tool
            .invoke(
                json!({ "url": "https://x.test/", "payload": {}, "body": "x" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        // Unknown method.
        let err = tool
            .invoke(
                json!({ "url": "https://x.test/", "method": "delete" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
        // Non-string header value.
        let err = tool
            .invoke(
                json!({ "url": "https://x.test/", "headers": { "a": 1 } }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)));
    }

    #[tokio::test]
    async fn send_webhook_fails_the_step_on_a_refusal() {
        // A non-2xx receiver status is a tool ERROR (the graph must stop /
        // the collect cursor must not advance on an unacknowledged delivery).
        let tool = SendWebhookTool::new(Arc::new(StubSender {
            status: 503,
            body: "busy",
            seen: std::sync::Mutex::new(None),
        }));
        let err = tool
            .invoke(
                json!({ "url": "https://example.com/hook" }),
                &ToolContext::default(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("503") && msg.contains("busy"), "got: {msg}");
    }

    #[test]
    fn send_webhook_gates_on_web_write() {
        // Egress-write: `web:write`, the counterpart to fetch_url's `web:read` —
        // a read-only web grant must not be able to exfiltrate via deliveries.
        let tool = SendWebhookTool::new(Arc::new(StubSender::ok()));
        assert_eq!(
            tool.required_capability(),
            Some(Capability::new(Action::Write, Resource::domain("web")))
        );
    }
}
