//! System status REST surface (SOUL §12) — the Settings "Status" panel.
//!
//! `GET /status` returns the server version, the (non-secret) LLM gateway
//! configuration, and a live health probe of each backing service: Postgres (the
//! source of truth), the LLM gateway, the coordination bus (Valkey / in-process),
//! and the optional derived stores Qdrant (vectors) and Neo4j (graph).
//!
//! Authenticated (any workspace member) but **carries no secret** — the gateway
//! API key (`Secret`) is never surfaced; only the base URL + model names are. The
//! probes are cheap liveness checks (`SELECT 1`, `GET /healthz`, an origin `GET`)
//! — they never spend tokens or mutate state.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::auth::Auth;
use crate::error::ApiResult;
use crate::state::AppState;

/// Mount the status routes.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/status/login", get(login_status))
}

/// The non-secret LLM gateway configuration (SOUL §6.1). Boot-time static — the
/// API key is deliberately omitted (it lives behind a `Secret`, SOUL §13).
#[derive(Debug, Serialize)]
pub struct LlmInfo {
    /// The gateway origin requests are posted under (e.g. `http://localhost:8088`).
    pub base_url: String,
    /// Model used for chat completions.
    pub default_model: String,
    /// Model used for embeddings (→ Qdrant).
    pub embedding_model: String,
    /// Model used for text-to-speech.
    pub speech_model: String,
    /// Default voice used for text-to-speech.
    pub speech_voice: String,
    /// Model used for transcription (speech-to-text).
    pub transcription_model: String,
    /// The configured `[ocr]` engines in chain order (`mistral`, `vision`,
    /// `tesseract`); empty = OCR off (image objects catalogue no text).
    pub ocr_engines: Vec<String>,
}

/// The liveness of one backing service.
#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    /// Display name (e.g. `"Postgres"`).
    pub name: String,
    /// One of `"up"`, `"down"`, or `"disabled"` (not configured).
    pub state: &'static str,
    /// A short human detail (URL, mode, or error summary).
    pub detail: String,
}

impl ServiceStatus {
    fn up(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            state: "up",
            detail: detail.into(),
        }
    }
    fn down(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            state: "down",
            detail: detail.into(),
        }
    }
    fn disabled(name: &str, detail: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            state: "disabled",
            detail: detail.into(),
        }
    }
    /// Map a probe `Result` to up/down, using `detail` on success and the error
    /// (suffixed onto `detail`) on failure.
    fn probe<E: std::fmt::Display>(name: &str, detail: &str, result: Result<(), E>) -> Self {
        match result {
            Ok(()) => Self::up(name, detail),
            Err(e) => Self::down(name, format!("{detail} — {e}")),
        }
    }
}

/// How long any single liveness probe may run before it is declared `down`. A
/// hard backstop so a hung backing service can never stall `/status`: the
/// Qdrant/Neo4j `healthz` calls use a default reqwest client with **no
/// per-request timeout**, so a server that accepts the connection but never
/// replies would otherwise block the endpoint forever. (Postgres' pool and the
/// LLM gateway's 3 s client bound themselves; this generous outer limit is a
/// harmless safety net for them and the real guard for the HTTP stores.)
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run a liveness probe under `limit`, mapping success/failure via
/// [`ServiceStatus::probe`] and a timeout to `down` — so the status endpoint stays
/// responsive even when a service hangs mid-probe.
async fn probe_with_timeout<E, F>(
    name: &str,
    detail: &str,
    limit: std::time::Duration,
    fut: F,
) -> ServiceStatus
where
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<(), E>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => ServiceStatus::probe(name, detail, result),
        Err(_) => ServiceStatus::down(
            name,
            format!("{detail} — timed out after {}s", limit.as_secs()),
        ),
    }
}

/// The full status payload.
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    /// The server (workspace) version.
    pub version: &'static str,
    /// The deployment mode (`single_user` | `multi_user`, SOUL §18) — presentation
    /// only. The web app reads it here to shape nav / settings depth / member-role
    /// chrome; it changes nothing about schema, API, authz, or tenancy.
    pub mode: &'static str,
    /// Whether OIDC single sign-on is configured (SOUL §18/§29) — lets the web app
    /// show an "SSO login" button pointing at `GET /auth/sso/login`. The web button
    /// itself is a follow-up; this flag is the seam for it.
    pub sso: bool,
    /// Whether the deployment enables workbench-managed llmleaf providers and
    /// routes. This is a presentation capability only; the operator routes are
    /// also omitted entirely when it is false.
    pub llm_control_plane: bool,
    /// A single rolled-up verdict: `true` iff no backing service is `down`. Lets a
    /// UI badge / external monitor read one value instead of scanning the list.
    pub healthy: bool,
    /// The LLM gateway configuration (non-secret).
    pub llm: LlmInfo,
    /// Per-service health, in display order.
    pub services: Vec<ServiceStatus>,
}

/// The **anonymous** slice of [`StatusResponse`] a login page needs before any
/// session exists (SOUL §12/§18): just the presentation flags. Deliberately no
/// health, services, or LLM config — those stay behind auth on `GET /status`.
#[derive(Debug, Serialize)]
pub struct LoginStatusResponse {
    /// Whether OIDC single sign-on is configured — drives the login view's
    /// "Sign in with SSO" button (`GET /auth/sso/login`).
    pub sso: bool,
    /// The full browser-facing `GET /auth/sso/login` URL, present when SSO is on
    /// **and** config pins one (`[sso].public_url`, else `[server].base_url`).
    /// Lets the login button reach an API living on a different domain than the
    /// SPA-derived `api.<spa-host>` (e.g. behind a Kubernetes ingress). Absent ⇒
    /// the SPA derives the origin itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sso_login_url: Option<String>,
    /// The deployment mode (`single_user` | `multi_user`) — presentation only.
    pub mode: &'static str,
}

/// `GET /status/login` — unauthenticated by design: an anonymous visitor must be
/// able to learn *how to log in* (and nothing else). Both fields are non-secret
/// presentation flags; everything sensitive stays on the authed `GET /status`.
async fn login_status(State(state): State<AppState>) -> Json<LoginStatusResponse> {
    let cfg = state.config();
    let sso = cfg.sso.is_enabled();
    Json(LoginStatusResponse {
        sso,
        sso_login_url: sso
            .then(|| cfg.sso.public_login_url(cfg.server.base_url.as_deref()))
            .flatten(),
        mode: cfg.server.mode.as_str(),
    })
}

/// Roll the per-service probes up into one verdict: healthy iff **no** service is
/// `down`. A `disabled` (not-configured) optional store does not count against
/// health — it's intentionally off, not broken.
fn overall_healthy(services: &[ServiceStatus]) -> bool {
    services.iter().all(|s| s.state != "down")
}

async fn status(State(state): State<AppState>, _auth: Auth) -> ApiResult<Json<StatusResponse>> {
    let cfg = state.config();
    let llm = LlmInfo {
        base_url: cfg.llm.base_url.clone(),
        default_model: cfg.llm.default_model.clone(),
        embedding_model: cfg.llm.embedding_model.clone(),
        speech_model: cfg.llm.speech_model.clone(),
        speech_voice: cfg.llm.speech_voice.clone(),
        transcription_model: cfg.llm.transcription_model.clone(),
        ocr_engines: state
            .ocr()
            .map(|c| c.engine_names().iter().map(|n| n.to_string()).collect())
            .unwrap_or_default(),
    };

    // Probe every backing service concurrently and under a hard per-probe timeout,
    // so the endpoint's latency is bounded by the slowest single probe (not their
    // sum) and a hung service degrades to `down` rather than stalling `/status`.

    // Relational source of truth. A `SELECT 1` round-trip.
    #[cfg(not(feature = "sqlite"))]
    let database_name = "Postgres";
    #[cfg(not(feature = "sqlite"))]
    let database_detail = "source of truth";
    #[cfg(feature = "sqlite")]
    let database_name = "SQLite";
    #[cfg(feature = "sqlite")]
    let database_detail = "source of truth (single-node)";
    let postgres = probe_with_timeout(
        database_name,
        database_detail,
        PROBE_TIMEOUT,
        state.store().ping(),
    );

    // LLM gateway — a cheap origin reachability probe (no chat request).
    let base = state.llm().base_url().to_string();
    let llm_probe = probe_with_timeout("LLM gateway", &base, PROBE_TIMEOUT, state.llm().ping());

    // Qdrant (vectors) — optional (`[qdrant].enabled`).
    let qdrant = async {
        if !cfg.qdrant.enabled {
            ServiceStatus::disabled("Qdrant (vectors)", "not configured")
        } else if let Some(v) = state.vector() {
            probe_with_timeout(
                "Qdrant (vectors)",
                &cfg.qdrant.url,
                PROBE_TIMEOUT,
                v.healthz(),
            )
            .await
        } else {
            ServiceStatus::disabled("Qdrant (vectors)", "configured but unavailable")
        }
    };

    // Neo4j (graph) — optional (`[neo4j].enabled`).
    let neo4j = async {
        if !cfg.neo4j.enabled {
            match state.graph() {
                Some(graph) => ServiceStatus::probe(
                    "Database graph",
                    "relational fallback",
                    graph.healthz().await,
                ),
                None => ServiceStatus::disabled("Database graph", "unavailable"),
            }
        } else if let Some(g) = state.graph() {
            probe_with_timeout("Neo4j (graph)", &cfg.neo4j.url, PROBE_TIMEOUT, g.healthz()).await
        } else {
            ServiceStatus::disabled("Neo4j (graph)", "configured but unavailable")
        }
    };

    let (postgres, llm_status, qdrant, neo4j) = tokio::join!(postgres, llm_probe, qdrant, neo4j);

    // Coordination bus — a synchronous mode check (distributed Valkey/Redis vs the
    // in-process fallback); no network round-trip, so it is not part of the join.
    let bus = if state.bus().is_distributed() {
        ServiceStatus::up("Coordination bus", "Valkey / Redis (distributed)")
    } else {
        ServiceStatus::up("Coordination bus", "in-process (single-node)")
    };

    // Display order: truth, gateway, bus, then the optional derived stores.
    let services = vec![postgres, llm_status, bus, qdrant, neo4j];

    Ok(Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        mode: cfg.server.mode.as_str(),
        sso: cfg.sso.is_enabled(),
        llm_control_plane: cfg.llm.control_plane_enabled,
        healthy: overall_healthy(&services),
        llm,
        services,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_maps_ok_and_err() {
        let ok = ServiceStatus::probe("X", "detail", Ok::<(), String>(()));
        assert_eq!(ok.state, "up");
        assert_eq!(ok.detail, "detail");
        let err = ServiceStatus::probe("X", "detail", Err::<(), _>("boom"));
        assert_eq!(err.state, "down");
        assert!(err.detail.contains("boom") && err.detail.contains("detail"));
    }

    #[test]
    fn disabled_state_token() {
        assert_eq!(
            ServiceStatus::disabled("Q", "not configured").state,
            "disabled"
        );
    }

    #[tokio::test]
    async fn probe_with_timeout_passes_ready_results_through() {
        let up = probe_with_timeout(
            "X",
            "detail",
            std::time::Duration::from_secs(1),
            std::future::ready(Ok::<(), String>(())),
        )
        .await;
        assert_eq!(up.state, "up");
        assert_eq!(up.detail, "detail");

        let down = probe_with_timeout(
            "X",
            "detail",
            std::time::Duration::from_secs(1),
            std::future::ready(Err::<(), _>("boom")),
        )
        .await;
        assert_eq!(down.state, "down");
        assert!(down.detail.contains("boom"));
    }

    #[test]
    fn overall_healthy_counts_down_but_not_disabled() {
        // No service down → healthy (a disabled optional store doesn't count).
        assert!(overall_healthy(&[
            ServiceStatus::up("A", "ok"),
            ServiceStatus::disabled("B", "not configured"),
        ]));
        // Any service down → not healthy.
        assert!(!overall_healthy(&[
            ServiceStatus::up("A", "ok"),
            ServiceStatus::down("C", "boom"),
        ]));
        // Nothing probed → vacuously healthy.
        assert!(overall_healthy(&[]));
    }

    #[tokio::test]
    async fn probe_with_timeout_marks_a_hung_probe_down() {
        // A probe that never resolves must degrade to `down` at the limit, not hang.
        let never = std::future::pending::<Result<(), String>>();
        let s =
            probe_with_timeout("X", "detail", std::time::Duration::from_millis(10), never).await;
        assert_eq!(s.state, "down");
        assert!(s.detail.contains("timed out"));
    }
}
