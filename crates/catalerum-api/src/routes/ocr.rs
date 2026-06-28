//! OCR REST (SOUL §7/§10) — `POST /ocr` extracts the text of an image (or PDF)
//! request body and returns it.
//!
//! The byte-in/text-out mirror of `POST /audio/transcriptions`: unlike the
//! `ocr_document` tool (which reads an already-stored object by key), this OCRs
//! bytes straight from the request — the client POSTs the blob with its
//! `Content-Type` and gets the text back.
//!
//! Engine resolution matches the tool exactly: the caller's per-user
//! `ocr_model` override routes through the **vision** chat engine with that
//! model; otherwise the boot-built `[ocr]` chain (mistral → vision → tesseract)
//! serves the request; neither configured → `400` with a pointer at `[ocr]`.
//!
//! Authenticated and **capability-gated** (SOUL §19): OCR burns LLM provider
//! quota, so the route requires `storage:write` (the domain OCR serves — file
//! ingestion) — a Viewer or an empty grant-scoped token cannot spend quota here.

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use serde::Serialize;
use std::sync::Arc;

use catalerum_core::capability::Action;
use catalerum_core::ocr::OcrRequest;
use catalerum_core::provider::OcrEngine;
use catalerum_llm::VisionOcr;

use crate::auth::Auth;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Mount the OCR route. The body limit covers the PDF cap (the per-kind caps
/// are enforced precisely in the handler, this is the transport ceiling).
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ocr", post(ocr))
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
}

/// The extracted text plus which engine served it (mirrors the `ocr_document`
/// tool's JSON, minus the storage key).
#[derive(Debug, Serialize)]
pub struct OcrResult {
    /// The extracted text (empty when the document contains none).
    pub text: String,
    /// The engine that served the request (`mistral`, `vision`, `tesseract`).
    pub engine: String,
}

async fn ocr(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<OcrResult>> {
    // Quota-spend gate (SOUL §19): OCR is a paid LLM call — require write
    // authority over the domain it serves.
    auth.require(Action::Write, "storage")?;
    let p = auth.principal();
    if body.is_empty() {
        return Err(ApiError::bad_request("empty document body"));
    }
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|ct| !ct.is_empty())
        .ok_or_else(|| {
            ApiError::bad_request("a Content-Type header naming the document format is required")
        })?
        .to_string();
    // Effective engine: the caller's per-user `ocr_model` override targets the
    // vision engine with that model; otherwise the configured chain decides
    // (the `ocr_document` tool's exact resolution).
    let model = state
        .store()
        .llm_settings()
        .get(p.workspace_id, p.user_id)
        .await
        .ok()
        .and_then(|s| s.ocr_model);
    let engine: Arc<dyn OcrEngine> = match &model {
        Some(m) => Arc::new(VisionOcr::new(state.llm().clone(), m.clone())),
        None => match state.ocr() {
            Some(chain) => chain.clone(),
            None => {
                return Err(ApiError::bad_request(
                    "no OCR engine configured; set [ocr] in the server config or pick an OCR model in Settings",
                ))
            }
        },
    };
    if !engine.supports(&content_type) {
        return Err(ApiError::bad_request(format!(
            "the {} OCR engine does not support `{content_type}`",
            engine.name()
        )));
    }
    let cfg = &state.config().ocr;
    let max = if content_type.starts_with("application/pdf") {
        cfg.max_document_bytes
    } else {
        cfg.max_image_bytes
    };
    if body.len() > max {
        return Err(ApiError::bad_request(format!(
            "document is {} bytes, over the {max}-byte OCR cap",
            body.len()
        )));
    }
    let mut request = OcrRequest::new(body.to_vec(), content_type);
    if let Some(m) = model {
        request = request.with_model(m);
    }
    let response = engine.ocr(request).await?;
    Ok(Json(OcrResult {
        text: response.text,
        engine: response.engine,
    }))
}
