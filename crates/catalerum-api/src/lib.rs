//! catalerum-api — Axum HTTP + WebSocket + SSE. The single enforcement choke
//! point: REST CRUD, streaming chat, and the live event feed, with auth and
//! workspace-scoping on every request. The LLM's tools are thin clients of
//! these same endpoints (SOUL §12, §19).
//!
//! # Building the app
//!
//! ```no_run
//! # async fn wire() -> Result<(), Box<dyn std::error::Error>> {
//! use catalerum_api::{build_router, AppState, Config};
//! use catalerum_bus::Bus;
//! use catalerum_iam::{IamService, PgIamStore};
//! use catalerum_llm::OpenRouterClient;
//! use catalerum_store::Store;
//!
//! let config = Config::default();
//! let store = Store::connect(&config.database.url).await?;
//! let iam = IamService::new(PgIamStore::new(store.pool().clone()))
//!     .with_base_url(config.server.effective_base_url());
//! let llm = OpenRouterClient::new(&config.llm.base_url, config.llm.api_key.expose());
//! let bus = Bus::in_process();
//!
//! // `None` web-fetcher, `None` webhook sender, `None` web-searcher and `None`
//! // executor, no terminal backends, no workspace sandbox, no external MCP
//! // tools, no OCR engine chain; the binary wires those from config.
//! let state = AppState::new(
//!     store, iam, llm, bus, config, None, None, None, None,
//!     std::collections::HashMap::new(), None, Vec::new(), None,
//! );
//! let app = build_router(state);
//! # let _ = app;
//! # Ok(())
//! # }
//! ```
//!
//! # HTTP surface
//!
//! | Method | Path | Auth | Body / Query | Response |
//! | ------ | ---- | ---- | ------------ | -------- |
//! | GET  | `/healthz` | none | — | `"ok"` |
//! | GET  | `/auth/magic` | none | `?token=<one-time>[&format=json]` | 302 → SPA `?token=<session>` (or [`routes::auth::SessionResponse`] JSON when `format=json`) |
//! | POST | `/conversations` | bearer | [`routes::conversations::CreateConversation`] | `Conversation` |
//! | GET  | `/conversations` | bearer | — | `Vec<Conversation>` |
//! | GET  | `/conversations/{id}` | bearer | — | `Conversation` |
//! | GET  | `/conversations/{id}/messages` | bearer | — | `Vec<Message>` |
//! | POST | `/connections` | bearer | [`routes::calendar::CreateConnection`] | `201` `Connection` |
//! | GET  | `/connections` | bearer | — | `Vec<Connection>` |
//! | POST | `/connections/{id}/sync` | bearer | — | `202` [`routes::calendar::SyncEnqueued`] |
//! | GET  | `/calendars` | bearer | — | `Vec<Calendar>` |
//! | GET  | `/events` | bearer | `?from=&to=&calendar_id=` | `Vec<Event>` |
//! | POST | `/notes` | bearer | [`routes::notes::CreateNote`] | `201` `Note` |
//! | GET  | `/notes` | bearer | — | `Vec<Note>` |
//! | GET  | `/notes/{id}` | bearer | — | `Note` |
//! | PUT  | `/notes/{id}` | bearer | [`routes::notes::UpdateNote`] | `Note` |
//! | DELETE | `/notes/{id}` | bearer | — | `204` |
//! | GET  | `/skills` | bearer (`skill:read`) | — | `Vec<Skill>` |
//! | POST | `/skills` | bearer (`skill:write`) | [`routes::skills::CreateSkill`] | `201` `Skill` |
//! | GET  | `/skills/{name}` | bearer (`skill:read`) | — | `Skill` |
//! | PUT  | `/skills/{name}` | bearer (`skill:write`) | [`routes::skills::UpdateSkill`] | `Skill` |
//! | DELETE | `/skills/{name}` | bearer (`skill:write`) | — | `204` |
//! | GET  | `/onboarding/state` | bearer (`profile:read`) | — | [`routes::onboarding::OnboardingState`] |
//! | POST | `/onboarding/personalize` | bearer (`skill:write`) | [`routes::onboarding::PersonalizeRequest`] | [`routes::onboarding::PersonalizeResponse`] |
//! | POST | `/onboarding/complete` | bearer (`profile:write`) | — | [`routes::onboarding::OnboardingState`] |
//! | GET  | `/automations` | bearer (`automation:read`) | — | `Vec<Automation>` |
//! | POST | `/automations` | bearer (`automation:write`) | [`routes::automations::CreateAutomation`] | `201` `Automation` |
//! | GET  | `/automations/{name}` | bearer (`automation:read`) | — | `Automation` |
//! | PUT  | `/automations/{name}` | bearer (`automation:write`) | [`routes::automations::UpdateAutomation`] | `Automation` |
//! | DELETE | `/automations/{name}` | bearer (`automation:write`) | — | `204` |
//! | POST | `/automations/{name}/enabled` | bearer (`automation:write`) | [`routes::automations::SetEnabled`] | `Automation` |
//! | POST | `/fetch` | bearer | [`FetchRequest`](catalerum_core::provider::FetchRequest) | [`FetchedPage`](catalerum_core::provider::FetchedPage) |
//! | POST | `/mcp` | bearer | JSON-RPC 2.0 ([`routes::mcp`], MCP over HTTP §26/§29) | JSON-RPC 2.0 (or `202` for a notification) |
//! | GET  | `/computer-agents` | bearer (`computer:read`) | — | `Vec<`[`routes::computer_agents::ComputerAgentView`]`>` |
//! | POST | `/computer-agents` | bearer (admin, `computer:write`) | [`routes::computer_agents::EnrollBody`] | `201` [`routes::computer_agents::EnrolledAgent`] (token once) |
//! | DELETE | `/computer-agents/{id}` | bearer (admin, `computer:write`) | — | `204` |
//! | GET  | `/computer-agents/connect` | agent token (`?token=`) | WS frames (SOUL §19/§20) | WS frames |
//! | GET  | `/ws/chat` | bearer (`?access_token=`) | WS frames | WS frames |
//!
//! All bearer-authenticated routes are scoped to the principal's workspace.

// The emerged-UI schema reference behind `explain_ui_schema` / `get_ui_schema`
// is one large nested `json!` literal; its depth×keys product exceeds the
// default macro recursion limit.
#![recursion_limit = "256"]

mod action_runner;
mod active_turns;
mod article_index;
mod auth;
mod calendar_writeback;
mod channel_listener;
mod chat_compact;
mod chat_meta;
mod chat_context;
mod computer_registry;
mod config;
mod connection_status;
mod db_migrate;
mod download_link;
mod error;
mod external_db;
mod google_channel_link;
mod google_oauth_state;
pub mod google_watch;
mod guidance;
mod mcp_endpoint;
mod mcp_endpoint_link;
mod mcp_manager;
mod mcp_providers;
mod mcp_push_bridge;
mod model_validation;
mod node_index;
mod personalization_cache;
mod pod_forward;
mod preview_client;
mod profile_agent;
mod routes;
mod sandbox;
mod sso_state;
mod state;
pub mod storage_watch;
mod subagent_runs;
mod terminal;
#[cfg(test)]
mod test_db;
mod tool_gate;
mod tool_index;
mod tools;
mod trigger_link;
mod ui_runtime;

pub use action_runner::ToolActionRunner;
pub use auth::Auth;
pub use channel_listener::ChannelListener;
pub use config::{
    AuthConfig, BackupConfig, BraveConfig, BrowserConfig, Config, CurationConfig, DatabaseConfig,
    DeploymentMode, ExaConfig, ExecConfig, ExternalDbConfig, ExternalDbConnectionConfig,
    FetchConfig, FirecrawlConfig, GoogleCseConfig, LangfuseTelemetryConfig, LlmConfig,
    McpAuthConfig, McpConfig, McpServerConfig, Neo4jConfig, OtelExporterConfig, QdrantConfig,
    S3StorageConfig, SearchConfig, SearxngConfig, Secret, SerpApiConfig, ServerConfig, SsoConfig,
    StorageConfig, TavilyConfig, TelemetryConfig, TelemetryContent, ValkeyConfig,
    WebDavStorageConfig,
};
pub use error::{ApiError, ApiResult};
pub use google_watch::GoogleWatchWorker;
pub use mcp_manager::McpManager;
pub use mcp_providers::{SkillPromptProvider, WorkspaceResourceProvider};
pub use mcp_push_bridge::{install_mcp_push_bridge, publish_mcp_push};
pub use node_index::{NodeDocHit, NodeDocIndex};
pub use state::{build_backend, build_ocr_chain, build_storage_backend, AppState, Iam};
pub use storage_watch::StorageWatchWorker;
pub use tool_index::{ToolHit, ToolIndex};

use axum::Router;
use opentelemetry::propagation::Extractor;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_opentelemetry::OpenTelemetrySpanExt;

struct HeaderExtractor<'a>(&'a axum::http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(axum::http::HeaderName::as_str).collect()
    }
}

/// Query-string keys whose values are credentials (session bearers, one-time
/// login/handoff tokens, OIDC authorization codes). They are redacted from the
/// request-tracing span so a live token never lands in logs / traces.
const SENSITIVE_QUERY_KEYS: &[&str] = &["token", "access_token", "code"];

/// Render a request's query string for the tracing span with credential values
/// ([`SENSITIVE_QUERY_KEYS`]) replaced by `[redacted]`; everything else passes
/// through verbatim.
fn scrub_query_for_trace(query: Option<&str>) -> String {
    let Some(q) = query else {
        return String::new();
    };
    q.split('&')
        .map(|pair| {
            let key = pair.split('=').next().unwrap_or("");
            if SENSITIVE_QUERY_KEYS.contains(&key) {
                format!("{key}=[redacted]")
            } else {
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the CORS layer from config (SOUL §13): an **allow-list** — the SPA
/// origin (`[server].web_url`) plus any `[server].cors_extra_origins` — with
/// the methods/headers the workbench actually uses. No credentials: the API
/// authenticates via the `Authorization` header, not ambient cookies, so
/// cross-origin browser calls only ever carry a token the caller already holds.
fn build_cors_layer(config: &Config) -> CorsLayer {
    let origins: Vec<axum::http::HeaderValue> = config
        .server
        .cors_allowed_origins()
        .iter()
        .filter_map(|o| match o.parse() {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(origin = %o, error = %e, "ignoring unparseable CORS origin");
                None
            }
        })
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
        ])
}

/// Build the complete Axum router from application state.
///
/// Wires every route module, the config-driven CORS allow-list (the SPA is
/// served from a different origin), and request tracing (with credential
/// query-params redacted). Returns a router ready to serve with [`serve`] or
/// `axum::serve`.
pub fn build_router(state: AppState) -> Router {
    // Object uploads buffer the whole body in memory; bound it (SOUL §9). The
    // `[storage].max_object_bytes` limit (default 64 MiB) replaces axum's global
    // 2 MiB default *for the storage routes only* — large enough for real files,
    // small enough to reject an unbounded-upload OOM with `413` before reading.
    let upload_limit = state.config().storage.max_object_bytes() as usize;
    // A disabled control plane has no HTTP surface at all. The all-in-one
    // profile opts in; regular deployments default to immutable llmleaf config.
    let llmleaf_router = if state.config().llm.control_plane_enabled {
        routes::llmleaf::router()
    } else {
        Router::new()
    };
    let router = Router::new()
        .merge(routes::health::router())
        .merge(routes::auth::router())
        // OIDC single sign-on (SOUL §18/§29): `GET /auth/sso/login` + `/callback`.
        // Both `404` when `[sso]` is unconfigured; the callback ends at the shared
        // `issue_session` chokepoint like magic-link.
        .merge(routes::sso::router())
        .merge(routes::google_oauth::router())
        .merge(routes::microsoft_oauth::router())
        .merge(routes::conversations::router())
        .merge(routes::calendar::router())
        .merge(routes::notes::router())
        .merge(routes::links::router())
        .merge(routes::skills::router())
        .merge(routes::tasks::router())
        .merge(routes::memory::router())
        .merge(routes::settings::router())
        .merge(routes::onboarding::router());
    router
        .merge(routes::organisations::router())
        .merge(routes::automations::router())
        .merge(routes::articles::router())
        .merge(routes::agent_profiles::router())
        .merge(routes::grants::router())
        .merge(routes::webhooks::router())
        // Google Calendar push webhook (§8/§16 M7): public `POST
        // /webhooks/google/calendar` whose `X-Goog-Channel-Token` is its own
        // authorization (no `Auth`). The static path takes precedence over the
        // generic `/webhooks/{*path}` catch-all above.
        .merge(routes::google_calendar_push::router())
        // Named-signal trigger sources (§11/§12): authed `POST /triggers/{name}` +
        // `/triggers/mint/{name}`, and the public `POST /triggers/fire/{token}` whose
        // signed token is its own authorization (no `Auth`).
        .merge(routes::triggers::router())
        .merge(routes::channels::router())
        .merge(routes::storage::router().layer(axum::extract::DefaultBodyLimit::max(upload_limit)))
        // The public signed-download redeem route (`GET /download/{token}`, §9): no
        // `Auth` — the HMAC-signed token is its own authorization.
        .merge(routes::download::router())
        .merge(routes::email::router())
        .merge(routes::db_connections::router())
        .merge(routes::fetch::router())
        .merge(routes::graph::router())
        .merge(routes::status::router())
        // Speech-to-text (SOUL §7): `POST /audio/transcriptions` — the chat mic
        // records a blob and posts it here for the composer's dictation.
        .merge(routes::audio::router())
        .merge(routes::ocr::router())
        .merge(routes::tools::router())
        .merge(routes::tokens::router())
        .merge(routes::terminals::router())
        .merge(routes::ui::router())
        .merge(routes::mcp::router())
        .merge(routes::mcp_endpoints::router())
        .merge(routes::mcp_servers::router())
        .merge(llmleaf_router)
        .merge(routes::computer_agents::router())
        // Cross-pod session forwarding (§16 M7): `POST /internal/pod`, sealed
        // AES-256-GCM envelopes under the shared master key — the envelope is
        // its own authorization (no `Auth`); `404` when no master key is set.
        .merge(routes::internal::router())
        .merge(routes::ws::router())
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
                let span = tracing::info_span!(
                    "http.request",
                    otel.kind = "server",
                    http.request.method = %request.method(),
                    url.path = %request.uri().path(),
                    // Credentials ride in query strings on a few routes (magic
                    // link, WS handshakes, browser media) — never log them.
                    url.query = scrub_query_for_trace(request.uri().query()),
                    network.protocol.version = ?request.version(),
                );
                let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
                    propagator.extract(&HeaderExtractor(request.headers()))
                });
                let _ = span.set_parent(parent);
                span
            }),
        )
        .layer(build_cors_layer(state.config()))
        .with_state(state)
}

/// Bind `addr` and serve `router` until the process is terminated.
///
/// A convenience so the binary need not depend on `axum` directly. `addr` is
/// any `tokio::net::TcpListener::bind` target, e.g. `"0.0.0.0:8787"`.
pub async fn serve(addr: &str, router: Router) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "failed to listen for Ctrl-C");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(%error, "failed to listen for Ctrl-C");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_query_redacts_credentials_and_keeps_the_rest() {
        // Bearer + one-time tokens and OIDC codes are redacted…
        assert_eq!(
            scrub_query_for_trace(Some("token=secret123")),
            "token=[redacted]"
        );
        assert_eq!(
            scrub_query_for_trace(Some("access_token=abc&code=xyz")),
            "access_token=[redacted]&code=[redacted]"
        );
        // …other params pass through verbatim, in order…
        assert_eq!(
            scrub_query_for_trace(Some("store=minio&token=secret&p=2")),
            "store=minio&token=[redacted]&p=2"
        );
        // …a same-prefix non-credential key is untouched…
        assert_eq!(scrub_query_for_trace(Some("token_hint=x")), "token_hint=x");
        // …and no query renders empty.
        assert_eq!(scrub_query_for_trace(None), "");
        assert_eq!(scrub_query_for_trace(Some("")), "");
    }

    #[test]
    fn cors_allow_list_comes_from_config() {
        let mut config = Config::default();
        // Default: exactly the SPA origin.
        assert_eq!(
            config.server.cors_allowed_origins(),
            vec!["http://localhost:8080".to_string()]
        );
        // Extras are trimmed + appended.
        config.server.cors_extra_origins =
            vec![" https://admin.example.com/ ".to_string(), String::new()];
        assert_eq!(
            config.server.cors_allowed_origins(),
            vec![
                "http://localhost:8080".to_string(),
                "https://admin.example.com".to_string()
            ]
        );
    }
}
