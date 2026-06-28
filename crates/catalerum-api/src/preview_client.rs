//! [`HttpPreviewer`] — the API's thin client to the standalone preview render
//! service (SOUL §9/§10). The API image is distroless and carries no render
//! toolchain; it POSTs a document to `catalerum-preview-service` over HTTP and
//! streams back the rendered image. This is the single `Previewer` behind
//! [`AppState::previewer`](crate::state::AppState::previewer); the storage
//! preview routes call it exactly like any other engine.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::StatusCode;

use catalerum_core::error::{Error, Result};
use catalerum_core::preview::{is_previewable, PreviewRequest, PreviewResponse};
use catalerum_core::provider::Previewer;

/// Default HTTP timeout when `[preview].timeout_secs` is `0` — generous enough
/// for a cold LibreOffice start plus an office→PDF conversion in the service.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// HTTP client to the preview render service.
pub struct HttpPreviewer {
    client: reqwest::Client,
    /// Service base URL with any trailing slash trimmed.
    base_url: String,
    /// Bearer token the service requires (its `PREVIEW_TOKEN`), if any.
    token: Option<String>,
}

impl HttpPreviewer {
    /// Build a client for the service at `base_url`. `token` (when non-empty) is
    /// sent as a bearer credential; `timeout_secs` `0` → the built-in default.
    pub fn new(base_url: &str, token: Option<String>, timeout_secs: u64) -> Result<Self> {
        let secs = if timeout_secs == 0 {
            DEFAULT_TIMEOUT_SECS
        } else {
            timeout_secs
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(secs))
            .build()
            .map_err(|e| Error::provider(format!("preview client: {e}")))?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.trim().is_empty()),
        })
    }
}

/// Read a numeric `X-Preview-*` header, defaulting to `0`.
fn header_u32(resp: &reqwest::Response, name: &str) -> u32 {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Map a non-2xx service response to the matching core error (the service uses
/// the same status contract the API's own error mapping expects).
fn map_status(status: StatusCode, body: String) -> Error {
    let msg = if body.trim().is_empty() {
        status.to_string()
    } else {
        body
    };
    match status {
        StatusCode::UNSUPPORTED_MEDIA_TYPE => Error::Unsupported(msg),
        StatusCode::BAD_REQUEST => Error::Invalid(msg),
        StatusCode::GATEWAY_TIMEOUT => Error::Timeout,
        StatusCode::NOT_FOUND => Error::NotFound,
        _ => Error::Provider(format!("preview service {status}: {msg}")),
    }
}

#[async_trait]
impl Previewer for HttpPreviewer {
    fn name(&self) -> &'static str {
        "http"
    }

    fn supports(&self, content_type: &str) -> bool {
        // Reject an obviously-unpreviewable type without a round-trip; the
        // service is the final authority for anything that passes.
        is_previewable(content_type)
    }

    async fn preview(&self, request: PreviewRequest) -> Result<PreviewResponse> {
        let url = format!(
            "{}/render?size={}&fmt={}&page={}",
            self.base_url,
            request.max_dimension,
            request.format.extension(),
            request.page,
        );
        let mut builder = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, &request.content_type)
            .body(request.document);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        let resp = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Timeout
            } else {
                Error::provider(format!("preview service request: {e}"))
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(map_status(status, body));
        }
        // Extract metadata (borrows) before consuming the body.
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/webp")
            .to_string();
        let width = header_u32(&resp, "x-preview-width");
        let height = header_u32(&resp, "x-preview-height");
        let page_count = header_u32(&resp, "x-preview-page-count").max(1);
        let image = resp
            .bytes()
            .await
            .map_err(|e| Error::provider(format!("preview response body: {e}")))?
            .to_vec();
        Ok(PreviewResponse {
            image,
            content_type,
            width,
            height,
            page_count,
            engine: self.name().to_string(),
        })
    }
}
