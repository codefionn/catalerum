//! Web fetch and web search backends.

use serde::{Deserialize, Serialize};

use super::Secret;

/// Web fetching & browsing (SOUL §27). Selects the default backend and carries
/// the egress safety policy plus per-backend settings. The plain-HTTP backend is
/// always available (local-first); Firecrawl and the browser/CDP backend activate
/// when their settings are present.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct FetchConfig {
    /// Backend an `auto` fetch resolves to: `http`, `browser`, or `firecrawl`.
    pub backend: String,
    /// Override the User-Agent sent by the HTTP backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// Default per-request timeout, in seconds.
    pub timeout_secs: u64,
    /// Cap on response-body bytes read (guards against huge pages).
    pub max_bytes: u64,
    /// Allow reaching private/loopback addresses. A protected scope (SOUL §19):
    /// off by default to block SSRF; enable only for trusted internal targets.
    pub allow_private_hosts: bool,
    /// Firecrawl backend settings.
    pub firecrawl: FirecrawlConfig,
    /// Browser/CDP backend settings.
    pub browser: BrowserConfig,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            backend: "http".to_string(),
            user_agent: None,
            timeout_secs: 30,
            max_bytes: 5 * 1024 * 1024,
            allow_private_hosts: false,
            firecrawl: FirecrawlConfig::default(),
            browser: BrowserConfig::default(),
        }
    }
}

/// Firecrawl scrape API settings (self-hosted or cloud). The API key is a secret
/// — supply it via `CATALERUM_FETCH__FIRECRAWL__API_KEY` rather than committing it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct FirecrawlConfig {
    /// Base URL; empty uses the Firecrawl cloud (`https://api.firecrawl.dev`).
    pub base_url: String,
    /// API key (or any token a self-hosted deployment accepts).
    pub api_key: Secret,
}

impl FirecrawlConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Browser/CDP backend settings. `cdp_url` is the Chrome DevTools Protocol
/// WebSocket of an external browser (headless Chrome `--remote-debugging-port`,
/// a Playwright server, or Browserless). Requires the `browser` build feature.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BrowserConfig {
    /// CDP WebSocket endpoint, e.g. `ws://localhost:9222/devtools/browser/<id>`.
    pub cdp_url: String,
}

impl BrowserConfig {
    /// True when an endpoint is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.cdp_url.trim().is_empty()
    }
}

/// Web search backends (SOUL §27). Selects the default provider a no-`provider`
/// search resolves to, and carries each provider's credentials. A provider
/// activates only when its credential is set (for SearXNG, its `base_url`).
///
/// These are **billed infrastructure secrets shared by the workspace**, not
/// per-user personalization — they live here (and in env), redacted by [`Secret`],
/// exactly like the Firecrawl key, and are never exposed through the web UI.
/// Supply each via `CATALERUM_SEARCH__<PROVIDER>__API_KEY` rather than committing it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Provider a search with no explicit `provider` resolves to (e.g. `brave`).
    pub backend: String,
    /// Brave Search.
    pub brave: BraveConfig,
    /// Tavily (LLM/RAG-optimized).
    pub tavily: TavilyConfig,
    /// Exa (neural + keyword).
    pub exa: ExaConfig,
    /// SearXNG (self-hosted; no key).
    pub searxng: SearxngConfig,
    /// Google Programmable Search Engine (CSE).
    pub google: GoogleCseConfig,
    /// SerpAPI (Google/Bing SERP scrape).
    pub serpapi: SerpApiConfig,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            backend: "brave".to_string(),
            brave: BraveConfig::default(),
            tavily: TavilyConfig::default(),
            exa: ExaConfig::default(),
            searxng: SearxngConfig::default(),
            google: GoogleCseConfig::default(),
            serpapi: SerpApiConfig::default(),
        }
    }
}

impl SearchConfig {
    /// Each provider id paired with whether it is configured (usable). The single
    /// source of truth the settings UI iterates to show which engines are live;
    /// order matches `catalerum_search::PROVIDER_IDS`.
    #[must_use]
    pub fn provider_status(&self) -> Vec<(&'static str, bool)> {
        vec![
            ("brave", self.brave.is_enabled()),
            ("tavily", self.tavily.is_enabled()),
            ("exa", self.exa.is_enabled()),
            ("searxng", self.searxng.is_enabled()),
            ("google", self.google.is_enabled()),
            ("serpapi", self.serpapi.is_enabled()),
        ]
    }

    /// True if at least one provider is configured (so a searcher can be built).
    #[must_use]
    pub fn any_enabled(&self) -> bool {
        self.provider_status().iter().any(|(_, on)| *on)
    }
}

/// Brave Search settings. Supply the subscription token via
/// `CATALERUM_SEARCH__BRAVE__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct BraveConfig {
    /// Brave subscription token.
    pub api_key: Secret,
}

impl BraveConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Tavily settings. Supply the key (`tvly-…`) via `CATALERUM_SEARCH__TAVILY__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct TavilyConfig {
    /// Tavily API key.
    pub api_key: Secret,
}

impl TavilyConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Exa settings. Supply the key via `CATALERUM_SEARCH__EXA__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ExaConfig {
    /// Exa API key.
    pub api_key: Secret,
}

impl ExaConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// SearXNG settings — a self-hosted metasearch instance; no key, just the base
/// URL of a deployment that has the JSON output format enabled.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct SearxngConfig {
    /// Base URL of the SearXNG instance, e.g. `https://searx.example.org`.
    pub base_url: String,
}

impl SearxngConfig {
    /// True when a base URL is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.base_url.trim().is_empty()
    }
}

/// Google Programmable Search Engine settings — needs **both** an API `key` and a
/// search engine id (`cx`). Supply the key via `CATALERUM_SEARCH__GOOGLE__API_KEY`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GoogleCseConfig {
    /// Google API key.
    pub api_key: Secret,
    /// Programmable Search Engine id (`cx`).
    pub cx: String,
}

impl GoogleCseConfig {
    /// True when both the key and the engine id are configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty() && !self.cx.trim().is_empty()
    }
}

/// SerpAPI settings. Supply the key via `CATALERUM_SEARCH__SERPAPI__API_KEY`; the
/// `engine` selects the SERP source (default `google`).
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct SerpApiConfig {
    /// SerpAPI key.
    pub api_key: Secret,
    /// SERP engine, e.g. `google`, `bing`, `duckduckgo`.
    pub engine: String,
}

impl Default for SerpApiConfig {
    fn default() -> Self {
        Self {
            api_key: Secret::default(),
            engine: "google".to_string(),
        }
    }
}

impl SerpApiConfig {
    /// True when a key is configured (the backend is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}
