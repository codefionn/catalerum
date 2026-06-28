//! `catalerum-preview-service` — the standalone preview render service (SOUL
//! §9/§10). A small HTTP server that turns a posted document into a raster image
//! preview, shipped as its own slim container image (LibreOffice + poppler + this
//! binary). The distroless catalerum API talks to it over the network via the
//! `HttpPreviewer` client — the API keeps no render toolchain of its own.
//!
//! Endpoints:
//! - `POST /render?size=&fmt=&page=` — body = document bytes, `Content-Type`
//!   header = the document's type; returns the rendered `image/*` bytes with
//!   `X-Preview-Width` / `-Height` / `-Page-Count` headers.
//! - `GET /healthz` — unauthenticated liveness (`ok`).
//!
//! Config is env-only (twelve-factor; the API passes matching values):
//! `PREVIEW_BIND` (default `0.0.0.0:8790`), `PREVIEW_TOKEN` (optional bearer —
//! when set, `/render` requires `Authorization: Bearer <token>`),
//! `PREVIEW_TIMEOUT_SECS`, `PREVIEW_MAX_BODY_BYTES`, `PREVIEW_HARD_MAX_DIMENSION`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use catalerum_core::error::Error;
use catalerum_core::preview::{PreviewFormat, PreviewRequest, DEFAULT_MAX_DIMENSION};
use catalerum_core::provider::Previewer;
use catalerum_preview::{DocumentPreviewer, ImagePreviewer, PreviewChain};

/// Shared server state.
struct AppState {
    chain: PreviewChain,
    /// Required bearer token for `/render`, when configured.
    token: Option<String>,
    /// Ceiling the requested longest-side bound is clamped to.
    hard_max_dimension: u32,
}

/// `?size=&fmt=&page=` for `/render`.
#[derive(Debug, Default, Deserialize)]
struct RenderQuery {
    #[serde(default)]
    size: Option<u32>,
    #[serde(default)]
    fmt: Option<String>,
    #[serde(default)]
    page: Option<u32>,
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,catalerum_preview=info".into()),
        )
        .init();

    let timeout_secs = env_u64("PREVIEW_TIMEOUT_SECS", 120);
    let max_body = env_u64("PREVIEW_MAX_BODY_BYTES", 64 * 1024 * 1024) as usize;
    let hard_max_dimension = env_u64("PREVIEW_HARD_MAX_DIMENSION", 2048) as u32;
    let bind = std::env::var("PREVIEW_BIND").unwrap_or_else(|_| "0.0.0.0:8790".into());
    let token = std::env::var("PREVIEW_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());

    // Engines: the in-process image thumbnailer always, the document engine only
    // when poppler is present (it ships in this image, but degrade cleanly if not).
    let mut engines: Vec<Arc<dyn Previewer>> = vec![Arc::new(ImagePreviewer::new())];
    let document = DocumentPreviewer::new().with_timeout_secs(timeout_secs);
    if document.probe().await {
        engines.push(Arc::new(document));
    } else {
        tracing::warn!("poppler (`pdftoppm`) not found — document previews disabled, images only");
    }
    let chain = PreviewChain::new(engines);
    tracing::info!(engines = ?chain.engine_names(), auth = token.is_some(), "preview service starting");

    let state = Arc::new(AppState {
        chain,
        token,
        hard_max_dimension,
    });

    let app = Router::new()
        .route("/render", post(render))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve on SIGTERM (k8s pod termination) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

async fn render(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<RenderQuery>,
    body: Bytes,
) -> Response {
    // Optional bearer auth.
    if let Some(expected) = &state.token {
        let ok = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|t| t == expected);
        if !ok {
            return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    if body.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty document body").into_response();
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();

    let size = q
        .size
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_DIMENSION)
        .clamp(16, state.hard_max_dimension.max(16));
    let format = q
        .fmt
        .as_deref()
        .map_or(PreviewFormat::Webp, PreviewFormat::parse_or_default);
    let request = PreviewRequest::new(body.to_vec(), content_type)
        .with_max_dimension(size)
        .with_format(format)
        .with_page(q.page.unwrap_or(1));

    match state.chain.preview(request).await {
        Ok(resp) => (
            [
                ("content-type", resp.content_type),
                ("cache-control", "private, max-age=3600".to_string()),
                ("x-preview-width", resp.width.to_string()),
                ("x-preview-height", resp.height.to_string()),
                ("x-preview-page-count", resp.page_count.to_string()),
            ],
            resp.image,
        )
            .into_response(),
        Err(e) => (err_status(&e), e.to_string()).into_response(),
    }
}

/// Map an engine error to an HTTP status.
fn err_status(e: &Error) -> StatusCode {
    match e {
        Error::Unsupported(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        Error::Invalid(_) => StatusCode::BAD_REQUEST,
        Error::Timeout => StatusCode::GATEWAY_TIMEOUT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}
