//! Outbound webhook delivery (SOUL §11/§27) — the egress-**write** counterpart
//! to the fetch backends.
//!
//! [`HttpWebhookSender`] implements [`WebhookSender`] over the same guarded
//! `reqwest` construction as [`HttpFetcher`](crate::http::HttpFetcher): every
//! delivery passes the [`FetchPolicy`] SSRF gate (URL validation + DNS
//! re-resolution) **and** the connect-time [`GuardedResolver`] screen, so a
//! rebinding name can't route a delivery to a private/loopback/metadata
//! address. Redirects are **never followed** — a 3xx comes back as its status,
//! so the guard can't be bounced around and a payload only ever lands at the
//! named URL. Hop-by-hop and body-framing headers are refused; the body's
//! `Content-Type` comes from the [`WebhookBody`], never a caller header.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{
    WebhookBody, WebhookDelivery, WebhookMethod, WebhookResponse, WebhookSender,
};

use crate::http::{map_reqwest, read_capped, GuardedResolver, DEFAULT_UA};
use crate::policy::FetchPolicy;

/// Cap on response-body bytes echoed back to the caller: a webhook response is
/// an ack/error payload, not page content, so it stays small regardless of the
/// (page-sized) fetch `max_bytes`.
const RESPONSE_CAP_BYTES: u64 = 64 * 1024;

/// Ceiling on a per-delivery timeout override (5 minutes) — a receiver that
/// takes longer than this to ack is down for our purposes.
const MAX_TIMEOUT_SECS: u64 = 300;

/// Request headers a delivery may never set: `Host` and the body-framing /
/// hop-by-hop set are the transport's to manage (a caller-controlled `Host` or
/// `Content-Length` is a smuggling primitive), and `Content-Type` comes from
/// the [`WebhookBody`] so the header always matches the body actually sent.
const DENIED_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "content-type",
    "transfer-encoding",
    "connection",
    "upgrade",
    "expect",
    "te",
    "trailer",
];

/// A guarded `reqwest`-backed [`WebhookSender`] (SOUL §11/§27).
#[derive(Clone)]
pub struct HttpWebhookSender {
    http: reqwest::Client,
    policy: FetchPolicy,
    default_timeout_secs: u64,
}

impl HttpWebhookSender {
    /// Build with a user agent, default timeout, and SSRF policy — the same
    /// knobs (and the same guarded client construction) as
    /// [`HttpFetcher::new`](crate::http::HttpFetcher::new).
    pub fn new(
        user_agent: Option<&str>,
        default_timeout_secs: u64,
        policy: FetchPolicy,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent.unwrap_or(DEFAULT_UA))
            // Never follow a redirect: a delivery must land only at the named
            // URL (following would need per-hop re-vetting like the fetcher —
            // and re-POSTing a payload across hops is delivery ambiguity we
            // don't want). The 3xx is returned as the response status.
            .redirect(reqwest::redirect::Policy::none())
            // Connect only to SSRF-vetted addresses (closes the DNS-rebind
            // TOCTOU between `guard_resolved` and reqwest's own resolution).
            .dns_resolver(std::sync::Arc::new(GuardedResolver {
                allow_private: policy.allow_private_hosts,
            }))
            .build()
            .map_err(|e| Error::provider(format!("building webhook client: {e}")))?;
        Ok(Self {
            http,
            policy,
            default_timeout_secs: default_timeout_secs.max(1),
        })
    }

    /// A sender with default settings (deny private hosts, 30 s timeout).
    pub fn with_defaults() -> Result<Self> {
        Self::new(None, 30, FetchPolicy::default())
    }

    /// The SSRF policy this sender enforces.
    #[must_use]
    pub fn policy(&self) -> &FetchPolicy {
        &self.policy
    }
}

#[async_trait]
impl WebhookSender for HttpWebhookSender {
    async fn deliver(&self, delivery: WebhookDelivery) -> Result<WebhookResponse> {
        let url = self.policy.validate(&delivery.url)?;

        // Screen caller-supplied header material before any network I/O
        // (`guard_resolved` resolves DNS): a denied or malformed header must
        // surface as `Invalid` even when the target name doesn't resolve.
        let mut headers = Vec::with_capacity(delivery.headers.len());
        for (name, value) in &delivery.headers {
            headers.push((parse_header_name(name)?, parse_header_value(name, value)?));
        }
        let raw_content_type = match &delivery.body {
            WebhookBody::Raw { content_type, .. } => {
                Some(parse_header_value("content_type", content_type)?)
            }
            WebhookBody::Json(_) => None,
        };

        self.policy.guard_resolved(&url).await?;

        let timeout = Duration::from_secs(
            delivery
                .timeout_secs
                .unwrap_or(self.default_timeout_secs)
                .clamp(1, MAX_TIMEOUT_SECS),
        );
        let method = match delivery.method {
            WebhookMethod::Post => reqwest::Method::POST,
            WebhookMethod::Put => reqwest::Method::PUT,
            WebhookMethod::Patch => reqwest::Method::PATCH,
        };

        let mut request = self.http.request(method, url).timeout(timeout);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request = match &delivery.body {
            WebhookBody::Json(payload) => request.json(payload),
            WebhookBody::Raw { body, .. } => request
                .header(
                    reqwest::header::CONTENT_TYPE,
                    raw_content_type.expect("parsed above for every Raw body"),
                )
                .body(body.clone()),
        };

        let resp = request.send().await.map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // Echo only a small ack/error payload; `min` so a stricter fetch
        // `max_bytes` also tightens the webhook cap.
        let cap = self.policy.max_bytes.min(RESPONSE_CAP_BYTES);
        let (body, body_bytes) = read_capped(resp, cap).await?;

        Ok(WebhookResponse {
            url: delivery.url,
            status,
            content_type,
            body,
            body_bytes,
        })
    }
}

/// Parse + screen a caller-supplied header name (see [`DENIED_HEADERS`]).
fn parse_header_name(name: &str) -> Result<HeaderName> {
    let lower = name.trim().to_ascii_lowercase();
    if DENIED_HEADERS.contains(&lower.as_str()) {
        return Err(Error::invalid(format!(
            "header `{name}` cannot be set on a webhook delivery (the transport manages it; \
             for a custom Content-Type use a raw `body` + `content_type`)"
        )));
    }
    HeaderName::from_bytes(lower.as_bytes())
        .map_err(|e| Error::invalid(format!("invalid header name `{name}`: {e}")))
}

/// Parse a caller-supplied header value (must be visible-ASCII, per HTTP).
fn parse_header_value(name: &str, value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value)
        .map_err(|e| Error::invalid(format!("invalid value for header `{name}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn blocks_localhost_by_default() {
        let s = HttpWebhookSender::with_defaults().unwrap();
        let err = s
            .deliver(WebhookDelivery::json("http://127.0.0.1:9/hook", json!({})))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn rejects_bad_scheme_and_local_names() {
        let s = HttpWebhookSender::with_defaults().unwrap();
        for url in [
            "ftp://example.com/",
            "http://localhost/hook",
            "http://x.internal/",
        ] {
            let err = s
                .deliver(WebhookDelivery::json(url, json!({})))
                .await
                .unwrap_err();
            assert!(
                matches!(err, Error::Invalid(_) | Error::Unauthorized(_)),
                "{url} should be refused, got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_transport_headers() {
        let s = HttpWebhookSender::with_defaults().unwrap();
        for h in [
            "Host",
            "content-length",
            "Content-Type",
            "Transfer-Encoding",
        ] {
            let mut d = WebhookDelivery::json("https://example.com/hook", json!({}));
            d.headers = vec![(h.to_string(), "x".to_string())];
            let err = s.deliver(d).await.unwrap_err();
            assert!(
                matches!(err, Error::Invalid(_)),
                "header {h} should be refused, got {err:?}"
            );
        }
    }

    /// A one-shot loopback HTTP receiver: accepts a single connection, reads
    /// the request until the body is complete, and answers with `response`.
    /// Returns the bound URL and a handle resolving to the raw request text.
    async fn one_shot_receiver(
        response: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(head_end) = text.find("\r\n\r\n") {
                    let content_length = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(str::trim)
                                .map(str::to_string)
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (format!("http://{addr}/hook"), handle)
    }

    /// A sender allowed to reach the loopback receiver (the tests' opt-in —
    /// the deny-by-default path is covered above).
    fn private_ok_sender() -> HttpWebhookSender {
        HttpWebhookSender::new(
            None,
            5,
            FetchPolicy {
                allow_private_hosts: true,
                ..FetchPolicy::default()
            },
        )
        .unwrap()
    }

    #[tokio::test]
    async fn delivers_json_payload_with_headers() {
        let (url, received) =
            one_shot_receiver("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").await;
        let mut d = WebhookDelivery::json(&url, json!({ "event": "done", "n": 3 }));
        d.headers = vec![("X-Signature".to_string(), "abc123".to_string())];
        let resp = private_ok_sender().deliver(d).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.is_success());
        assert_eq!(resp.body, "{\"ok\":true}");
        assert_eq!(resp.content_type.as_deref(), Some("application/json"));

        let raw = received.await.unwrap();
        assert!(raw.starts_with("POST /hook HTTP/1.1\r\n"), "got: {raw}");
        let lower = raw.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/json"));
        assert!(lower.contains("x-signature: abc123"));
        assert!(raw.ends_with("{\"event\":\"done\",\"n\":3}"), "got: {raw}");
    }

    #[tokio::test]
    async fn delivers_raw_body_with_content_type_and_method() {
        let (url, received) =
            one_shot_receiver("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await;
        let d = WebhookDelivery {
            url: url.clone(),
            method: WebhookMethod::Put,
            headers: Vec::new(),
            body: WebhookBody::Raw {
                body: "a=1&b=2".to_string(),
                content_type: "application/x-www-form-urlencoded".to_string(),
            },
            timeout_secs: Some(5),
        };
        let resp = private_ok_sender().deliver(d).await.unwrap();
        assert_eq!(resp.status, 204);

        let raw = received.await.unwrap();
        assert!(raw.starts_with("PUT /hook HTTP/1.1\r\n"), "got: {raw}");
        assert!(raw
            .to_ascii_lowercase()
            .contains("content-type: application/x-www-form-urlencoded"));
        assert!(raw.ends_with("a=1&b=2"), "got: {raw}");
    }

    #[tokio::test]
    async fn non_2xx_is_a_response_not_a_transport_error() {
        // Trait semantics: a completed exchange returns Ok with the status —
        // the delivery TOOL is what maps a refusal to a failed step.
        let (url, received) =
            one_shot_receiver("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\n\r\nbusy")
                .await;
        let resp = private_ok_sender()
            .deliver(WebhookDelivery::json(&url, json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 503);
        assert!(!resp.is_success());
        assert_eq!(resp.body, "busy");
        received.await.unwrap();
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        // A 3xx comes back as-is: the payload must never be re-sent to a
        // Location the guard hasn't vetted (and 127.0.0.1:9 would be refused).
        let (url, received) = one_shot_receiver(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/evil\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let resp = private_ok_sender()
            .deliver(WebhookDelivery::json(&url, json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 302);
        assert!(!resp.is_success());
        received.await.unwrap();
    }
}
