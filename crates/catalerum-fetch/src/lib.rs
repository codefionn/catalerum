//! catalerum-fetch — web fetching & browsing behind the core [`WebFetcher`]
//! trait, with HTML→Markdown so a page costs the LLM a fraction of its raw
//! context (SOUL §27).
//!
//! # What it integrates
//! - **HTTP** ([`HttpFetcher`], feature `http`, default) — a plain `reqwest` GET.
//!   Local-first, no JavaScript, the cheapest path and the `Auto` fallback.
//! - **Browser / CDP** ([`browser::CdpFetcher`], feature `browser`) — drives an
//!   external Chrome/Chromium or Playwright server over the Chrome DevTools
//!   Protocol to render JavaScript before snapshotting.
//! - **Firecrawl** ([`FirecrawlFetcher`], feature `firecrawl`, default) — the
//!   self-hostable-or-cloud scrape API, which returns Markdown directly.
//!
//! Every backend funnels its HTML through [`markdown::html_to_markdown`], which
//! strips boilerplate and extracts the main content — the "less context" win
//! (SOUL §27). [`MultiFetcher`] picks a backend per request ([`FetchMode`]).
//!
//! Safety: a [`FetchPolicy`] SSRF guard runs before any socket opens — only
//! `http(s)`, never a private/loopback address unless explicitly allowed
//! (SOUL §13, §19).

#![forbid(unsafe_code)]

pub mod extract;
pub mod markdown;
pub mod policy;
pub mod tool;

#[cfg(feature = "browser")]
pub mod browser;
#[cfg(feature = "firecrawl")]
pub mod firecrawl;
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http")]
pub mod webhook;

pub use extract::{extract_html, ExtractField};
pub use markdown::{
    extract_title, html_to_markdown, html_to_text, markdown_to_text, MarkdownOptions,
};
pub use policy::FetchPolicy;
pub use tool::{ExtractHtmlTool, FetchUrlTool, HtmlToMarkdownTool, SendWebhookTool};

// Re-export the core web-fetch + webhook surface for ergonomic `use catalerum_fetch::…`.
pub use catalerum_core::provider::{
    FetchFormat, FetchMode, FetchRequest, FetchedPage, WebFetcher, WebhookBody, WebhookDelivery,
    WebhookMethod, WebhookResponse, WebhookSender,
};

#[cfg(feature = "firecrawl")]
pub use firecrawl::{FirecrawlFetcher, FIRECRAWL_CLOUD};
#[cfg(feature = "http")]
pub use http::HttpFetcher;
#[cfg(feature = "http")]
pub use webhook::HttpWebhookSender;

#[cfg(feature = "http")]
pub use router::{BackendKind, MultiFetcher};

#[cfg(feature = "http")]
mod router {
    use async_trait::async_trait;

    use catalerum_core::error::Result;
    use catalerum_core::provider::{FetchMode, FetchRequest, FetchedPage, WebFetcher};

    use crate::http::HttpFetcher;

    /// Which backend handles a request (SOUL §27).
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum BackendKind {
        /// Plain HTTP GET (local-first).
        Http,
        /// External browser over CDP (renders JavaScript).
        Browser,
        /// Firecrawl scrape API.
        Firecrawl,
    }

    impl BackendKind {
        /// Parse a config token (`http` / `browser` / `firecrawl`).
        #[must_use]
        pub fn parse(s: &str) -> Option<Self> {
            match s.trim().to_ascii_lowercase().as_str() {
                "http" => Some(Self::Http),
                "browser" | "cdp" | "playwright" | "chromium" => Some(Self::Browser),
                "firecrawl" => Some(Self::Firecrawl),
                _ => None,
            }
        }
    }

    /// Routes each [`FetchRequest`] to a backend by [`FetchMode`] (SOUL §27). The
    /// plain-HTTP backend is always present (local-first); the browser and
    /// Firecrawl backends are optional, and unavailable choices fall back toward
    /// HTTP rather than failing.
    pub struct MultiFetcher {
        http: HttpFetcher,
        #[cfg(feature = "firecrawl")]
        firecrawl: Option<crate::firecrawl::FirecrawlFetcher>,
        #[cfg(feature = "browser")]
        browser: Option<crate::browser::CdpFetcher>,
        default: BackendKind,
    }

    impl MultiFetcher {
        /// A router with just the local-first HTTP backend, used for `Auto`.
        #[must_use]
        pub fn new(http: HttpFetcher, default: BackendKind) -> Self {
            Self {
                http,
                #[cfg(feature = "firecrawl")]
                firecrawl: None,
                #[cfg(feature = "browser")]
                browser: None,
                default,
            }
        }

        /// Attach the Firecrawl backend.
        #[cfg(feature = "firecrawl")]
        #[must_use]
        pub fn with_firecrawl(mut self, fc: crate::firecrawl::FirecrawlFetcher) -> Self {
            self.firecrawl = Some(fc);
            self
        }

        /// Attach the browser/CDP backend.
        #[cfg(feature = "browser")]
        #[must_use]
        pub fn with_browser(mut self, b: crate::browser::CdpFetcher) -> Self {
            self.browser = Some(b);
            self
        }

        /// The backend an `Auto` request resolves to.
        #[must_use]
        pub fn default_backend(&self) -> BackendKind {
            self.default
        }

        fn resolve(&self, mode: FetchMode) -> BackendKind {
            match mode {
                FetchMode::Auto => self.default,
                FetchMode::Http => BackendKind::Http,
                FetchMode::Browser => BackendKind::Browser,
            }
        }
    }

    #[async_trait]
    impl WebFetcher for MultiFetcher {
        async fn fetch(&self, request: FetchRequest) -> Result<FetchedPage> {
            match self.resolve(request.mode) {
                BackendKind::Browser => {
                    #[cfg(feature = "browser")]
                    if let Some(b) = &self.browser {
                        return b.fetch(request).await;
                    }
                    // No browser configured → best available render.
                    #[cfg(feature = "firecrawl")]
                    if let Some(fc) = &self.firecrawl {
                        return fc.fetch(request).await;
                    }
                    self.http.fetch(request).await
                }
                BackendKind::Firecrawl => {
                    #[cfg(feature = "firecrawl")]
                    if let Some(fc) = &self.firecrawl {
                        return fc.fetch(request).await;
                    }
                    self.http.fetch(request).await
                }
                BackendKind::Http => self.http.fetch(request).await,
            }
        }
    }
}
