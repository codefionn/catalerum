//! Plain-HTTP fetch backend (SOUL §27) — the local-first default.
//!
//! A `reqwest` GET, then HTML→Markdown. No JavaScript: cheapest and needs no
//! browser, so it is always available and is the fallback for `FetchMode::Auto`.
//! Every request passes the [`FetchPolicy`] SSRF guard before any socket opens,
//! including each redirect hop and a post-resolution DNS re-check.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use catalerum_core::error::{Error, Result};
use catalerum_core::provider::{FetchFormat, FetchRequest, FetchedPage, WebFetcher};

use crate::markdown::{self, MarkdownOptions};
use crate::policy::FetchPolicy;

/// Default browser-ish UA so servers don't serve a degraded/blocked page.
/// Shared with the webhook sender (`crate::webhook`), which rides the same
/// client construction.
pub(crate) const DEFAULT_UA: &str =
    "Mozilla/5.0 (compatible; catalerum/0.1; +https://codefionn.eu/catalerum)";

/// Max redirect hops followed. Matches the prior reqwest policy's cap; each hop is
/// re-vetted through the async SSRF guard ([`HttpFetcher::vet_redirect`]).
const MAX_REDIRECTS: usize = 10;

/// A plain-HTTP [`WebFetcher`] (SOUL §27).
#[derive(Clone)]
pub struct HttpFetcher {
    http: reqwest::Client,
    policy: FetchPolicy,
    default_timeout_secs: u64,
}

impl HttpFetcher {
    /// Build with a user agent, default timeout, and SSRF policy.
    pub fn new(
        user_agent: Option<&str>,
        default_timeout_secs: u64,
        policy: FetchPolicy,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(user_agent.unwrap_or(DEFAULT_UA))
            // Auto-redirect is disabled: we follow redirects manually in `fetch` so
            // every hop runs the *async* SSRF guard (reqwest's redirect hook is
            // synchronous and cannot resolve DNS — see `vet_redirect`).
            .redirect(reqwest::redirect::Policy::none())
            // Screen every *connect-time* resolution through the SSRF guard, so
            // reqwest connects only to vetted addresses — this closes the DNS-rebind
            // TOCTOU between the pre-flight `guard_resolved` and reqwest's own
            // resolution (see `GuardedResolver`).
            .dns_resolver(Arc::new(GuardedResolver {
                allow_private: policy.allow_private_hosts,
            }))
            .build()
            .map_err(|e| Error::provider(format!("building http client: {e}")))?;
        Ok(Self {
            http,
            policy,
            default_timeout_secs: default_timeout_secs.max(1),
        })
    }

    /// A fetcher with default settings (deny private hosts, 30s timeout).
    pub fn with_defaults() -> Result<Self> {
        Self::new(None, 30, FetchPolicy::default())
    }

    /// The SSRF policy this fetcher enforces.
    #[must_use]
    pub fn policy(&self) -> &FetchPolicy {
        &self.policy
    }

    /// Resolve a redirect `location` (absolute, relative, or protocol-relative)
    /// against the `current` URL and run it through the **same** SSRF guard as the
    /// initial request: scheme + literal-IP/local-name check ([`FetchPolicy::validate`])
    /// then async DNS re-resolution ([`FetchPolicy::guard_resolved`]). This is the
    /// check reqwest's synchronous redirect hook could not perform (no async DNS),
    /// so a hop to a public host that resolves to a private/loopback/metadata IP is
    /// now refused too (SOUL §19).
    async fn vet_redirect(&self, current: &url::Url, location: &str) -> Result<url::Url> {
        let next = current
            .join(location)
            .map_err(|e| Error::invalid(format!("invalid redirect target `{location}`: {e}")))?;
        let url = self.policy.validate(next.as_str())?;
        self.policy.guard_resolved(&url).await?;
        Ok(url)
    }
}

#[async_trait]
impl WebFetcher for HttpFetcher {
    async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage> {
        let mut url = self.policy.validate(&request.url)?;
        self.policy.guard_resolved(&url).await?;

        let timeout = std::time::Duration::from_secs(
            request
                .timeout_secs
                .unwrap_or(self.default_timeout_secs)
                .max(1),
        );

        // Follow redirects ourselves (the client has auto-redirect off) so each hop
        // runs the same async SSRF guard as the initial URL. reqwest's built-in
        // redirect hook is synchronous and can't resolve DNS, so it would let a
        // redirect to a public host that resolves to a private/loopback/metadata IP
        // slip through (SOUL §19).
        let mut redirects = 0usize;
        let resp = loop {
            let resp = self
                .http
                .get(url.clone())
                .timeout(timeout)
                .send()
                .await
                .map_err(map_reqwest)?;
            if !resp.status().is_redirection() {
                break resp;
            }
            // Own the Location string so no borrow of `resp` is held across the move.
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let Some(location) = location else {
                break resp; // a 3xx without a usable Location — treat it as final
            };
            redirects += 1;
            if redirects > MAX_REDIRECTS {
                return Err(Error::provider(format!(
                    "too many redirects (>{MAX_REDIRECTS}) fetching `{}`",
                    request.url
                )));
            }
            url = self.vet_redirect(&url, &location).await?;
        };

        let status = resp.status().as_u16();
        let final_url = resp.url().clone();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        let (body, raw_bytes) = read_capped(resp, self.policy.max_bytes).await?;

        let page = render(&request, &final_url, status, content_type, &body, raw_bytes);
        Ok(page)
    }
}

/// Stream a response body, capping at `max_bytes`. Returns the decoded text and
/// the raw byte count. Shared with the webhook sender (`crate::webhook`).
pub(crate) async fn read_capped(resp: reqwest::Response, max_bytes: u64) -> Result<(String, u64)> {
    let mut collected: Vec<u8> = Vec::new();
    let mut total: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest)?;
        total += chunk.len() as u64;
        if (collected.len() as u64) < max_bytes {
            let remaining = (max_bytes - collected.len() as u64) as usize;
            let take = remaining.min(chunk.len());
            collected.extend_from_slice(&chunk[..take]);
        }
        // Keep draining to count `total`, but stop accumulating past the cap.
        if total > max_bytes.saturating_mul(4) {
            break; // pathological body; we already have our cap's worth.
        }
    }
    // If we capped mid-stream the last bytes may be a partial UTF-8 char; trim to
    // the last whole char so the cap boundary never produces a replacement glyph.
    let bytes = if total > max_bytes {
        truncate_to_char_boundary(&collected)
    } else {
        &collected[..]
    };
    let text = String::from_utf8_lossy(bytes).into_owned();
    Ok((text, total))
}

/// Trim a trailing partial UTF-8 sequence from a byte slice so a hard byte cut
/// never splits a multi-byte character.
fn truncate_to_char_boundary(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    // Walk back over UTF-8 continuation bytes (`10xxxxxx`).
    while end > 0 && (bytes[end - 1] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    // `end-1` is now a lead byte (or end==0). Drop it too if its sequence is
    // incomplete (fewer bytes present than the lead advertises).
    if end > 0 {
        let lead = bytes[end - 1];
        let need = match lead {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        let have = bytes.len() - (end - 1);
        if have < need {
            end -= 1;
        }
    }
    &bytes[..end]
}

/// Build a [`FetchedPage`] from a fetched body in the requested representation.
/// Shared by the HTTP and browser backends (both end with raw HTML in hand).
pub(crate) fn render(
    request: &FetchRequest,
    final_url: &url::Url,
    status: u16,
    content_type: Option<String>,
    body: &str,
    raw_bytes: u64,
) -> FetchedPage {
    let looks_html = content_type
        .as_deref()
        .map(|c| c.contains("html"))
        .unwrap_or(true);

    let title = looks_html.then(|| markdown::extract_title(body)).flatten();

    let content = if !looks_html {
        // Already plain text / JSON / etc. — pass through unchanged.
        body.to_string()
    } else {
        let opts = MarkdownOptions {
            base_url: Some(final_url.to_string()),
            main_content_only: request.main_content_only,
            ..MarkdownOptions::default()
        };
        match request.format {
            FetchFormat::Html => body.to_string(),
            FetchFormat::Markdown => markdown::html_to_markdown(body, &opts),
            FetchFormat::Text => markdown::html_to_text(body, &opts),
        }
    };

    let content_bytes = content.len() as u64;
    FetchedPage {
        url: final_url.to_string(),
        status,
        title,
        content_type,
        content,
        format: request.format,
        raw_bytes,
        content_bytes,
    }
}

/// A reqwest DNS resolver that screens every resolved address through the SSRF
/// guard at **connect time**. reqwest resolves DNS itself when it connects —
/// separately from the pre-flight [`FetchPolicy::guard_resolved`] — so a name that
/// rebinds in between could still land on a private/loopback/metadata IP. With this
/// resolver, reqwest only ever connects to an address the guard allowed (or the
/// resolution fails), closing that TOCTOU (SOUL §19). Honours the
/// `allow_private_hosts` opt-in like every other check.
#[derive(Debug, Clone)]
pub(crate) struct GuardedResolver {
    pub(crate) allow_private: bool,
}

impl Resolve for GuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow_private = self.allow_private;
        let host = name.as_str().to_string();
        Box::pin(async move {
            // Resolve with port 0 — reqwest overrides it with the request's port.
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if let Some(bad) = crate::policy::first_blocked_addr(&addrs, allow_private) {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("SSRF guard: host `{host}` resolved to a blocked address {bad}"),
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

pub(crate) fn map_reqwest(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Timeout
    } else {
        Error::provider(format!("http fetch failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalerum_core::provider::FetchFormat;

    fn req(url: &str, format: FetchFormat) -> FetchRequest {
        FetchRequest::new(url).format(format)
    }

    #[test]
    fn render_html_to_markdown() {
        let url = url::Url::parse("https://example.com/page").unwrap();
        let page = render(
            &req("https://example.com/page", FetchFormat::Markdown),
            &url,
            200,
            Some("text/html; charset=utf-8".to_string()),
            "<html><head><title>T</title></head><body><main><h1>Hi</h1><p>Body.</p></main></body></html>",
            120,
        );
        assert_eq!(page.status, 200);
        assert_eq!(page.title.as_deref(), Some("T"));
        assert_eq!(page.content, "# Hi\n\nBody.");
        assert_eq!(page.format, FetchFormat::Markdown);
        assert_eq!(page.raw_bytes, 120);
        assert!(page.context_ratio().unwrap() < 1.0);
    }

    #[test]
    fn render_passthrough_non_html() {
        let url = url::Url::parse("https://example.com/data.json").unwrap();
        let page = render(
            &req("https://example.com/data.json", FetchFormat::Markdown),
            &url,
            200,
            Some("application/json".to_string()),
            "{\"a\":1}",
            7,
        );
        assert_eq!(page.content, "{\"a\":1}");
        assert_eq!(page.title, None);
    }

    #[test]
    fn render_html_format_keeps_raw() {
        let url = url::Url::parse("https://example.com/").unwrap();
        let html = "<body><main><p>x</p></main></body>";
        let page = render(
            &req("https://example.com/", FetchFormat::Html),
            &url,
            200,
            Some("text/html".to_string()),
            html,
            html.len() as u64,
        );
        assert_eq!(page.content, html);
    }

    #[tokio::test]
    async fn fetch_blocks_localhost() {
        let f = HttpFetcher::with_defaults().unwrap();
        let err = f
            .fetch(FetchRequest::new("http://127.0.0.1:9/"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn fetch_rejects_bad_scheme() {
        let f = HttpFetcher::with_defaults().unwrap();
        assert!(f
            .fetch(FetchRequest::new("ftp://example.com/"))
            .await
            .is_err());
    }

    // The redirect hop vetting is the SSRF-relevant logic (reqwest's sync hook
    // could not DNS-resolve a hop). `vet_redirect` reuses `validate` +
    // `guard_resolved`, so these exercise the exact gate every hop now passes —
    // offline, using literal/name targets (no network DNS).

    #[tokio::test]
    async fn vet_redirect_resolves_relative_and_protocol_relative() {
        let f = HttpFetcher::with_defaults().unwrap();
        let base = url::Url::parse("https://93.184.216.34/a/b").unwrap();
        // A relative Location resolves against the current URL (public literal → ok;
        // `guard_resolved` is a no-op for an IP literal).
        let next = f.vet_redirect(&base, "/c").await.unwrap();
        assert_eq!(next.as_str(), "https://93.184.216.34/c");
        // A protocol-relative `//host` redirect to the cloud-metadata link-local
        // address is refused — a classic SSRF target that must not slip through.
        let err = f
            .vet_redirect(&base, "//169.254.169.254/latest")
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unauthorized(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn vet_redirect_blocks_private_loopback_and_bad_scheme() {
        let f = HttpFetcher::with_defaults().unwrap();
        let base = url::Url::parse("https://example-host.test/").unwrap();
        for loc in [
            "http://127.0.0.1:1/",     // loopback literal
            "http://10.0.0.1/admin",   // private literal
            "http://localhost/secret", // local name
            "http://foo.internal/",    // internal name
            "file:///etc/passwd",      // non-http scheme
        ] {
            let err = f.vet_redirect(&base, loc).await.unwrap_err();
            assert!(
                matches!(err, Error::Unauthorized(_) | Error::Invalid(_)),
                "redirect to {loc} should be refused, got {err:?}"
            );
        }
    }
}
