//! OCR engines and the document preview service.

use serde::{Deserialize, Serialize};

use super::Secret;

/// OCR engines (SOUL §7/§10): image/PDF objects → searchable text. Engines are
/// chained **mistral → vision → tesseract**; each document is served by the
/// first configured engine that supports its type and succeeds. All engines off
/// (the default) preserves the pre-OCR behavior exactly: binary objects
/// catalogue no text. Ingest always uses this config-level chain (jobs carry no
/// acting user); a per-user `llm_settings.ocr_model` additionally routes
/// user-invoked OCR (the `ocr_document` tool, `POST /ocr`) through the vision
/// engine with that model.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct OcrConfig {
    /// Vision chat model for OCR via llmleaf (a chat model advertising `image`
    /// input, e.g. a pixtral/gpt-4o-class model). Empty = the vision engine is
    /// off. Deliberately **not** defaulted to `echo` — the dev echo model would
    /// catalogue prompt echoes as document text for every uploaded image.
    pub vision_model: String,
    /// Byte cap for OCR-ing a raster image. Oversized documents are **skipped,
    /// never truncated** — truncated image bytes are corrupt, unlike text.
    /// The base64 data URI expands this ×4/3 on the wire.
    pub max_image_bytes: usize,
    /// Byte cap for OCR-ing a PDF (the Mistral-dialect engine only).
    pub max_document_bytes: usize,
    /// The dedicated Mistral-dialect `/v1/ocr` API (cloud or compatible
    /// self-hosted). Enabled iff its `api_key` is set.
    pub mistral: MistralOcrConfig,
    /// The offline `tesseract`-CLI fallback.
    pub tesseract: TesseractOcrConfig,
}

impl Default for OcrConfig {
    fn default() -> Self {
        Self {
            vision_model: String::new(),
            max_image_bytes: 8 * 1024 * 1024,
            max_document_bytes: 32 * 1024 * 1024,
            mistral: MistralOcrConfig::default(),
            tesseract: TesseractOcrConfig::default(),
        }
    }
}

/// Preview rendering (SOUL §9/§10): a stored object → a raster image preview
/// (the first page of a PDF/office document, a rendered spreadsheet/
/// presentation, or a resized image thumbnail). Rendering runs in the
/// **standalone preview service** (`catalerum-preview-service`, its own slim
/// container image with LibreOffice + poppler); the distroless API just POSTs
/// the document to it over HTTP. With no `service_url` set, previews are off and
/// the routes report "not configured".
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PreviewConfig {
    /// Master switch. `false` disables the preview routes entirely.
    pub enabled: bool,
    /// Base URL of the preview render service, e.g. `http://catalerum-preview`
    /// (in-cluster Service) or `http://localhost:8790` (dev). Empty → previews
    /// are disabled (the API carries no render toolchain of its own).
    pub service_url: String,
    /// Optional bearer token the service requires (matches its `PREVIEW_TOKEN`).
    /// Empty → no `Authorization` header is sent. Set via env, not committed.
    pub service_token: Secret,
    /// Default output image format when the request names none: `webp` (the
    /// default, smallest), `png`, or `jpeg`.
    pub default_format: String,
    /// Default longest-side pixel bound when the request names no size.
    pub max_dimension: u32,
    /// Hard ceiling on the requested longest-side bound — a request asking for
    /// more is clamped to this (bounds render cost + response size).
    pub hard_max_dimension: u32,
    /// HTTP request timeout to the preview service (seconds). `0` → a built-in
    /// default generous enough for a cold LibreOffice start plus conversion.
    pub timeout_secs: u64,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            service_url: String::new(),
            service_token: Secret::default(),
            default_format: "webp".to_string(),
            max_dimension: catalerum_core::preview::DEFAULT_MAX_DIMENSION,
            hard_max_dimension: 1600,
            timeout_secs: 0,
        }
    }
}

/// Mistral-dialect OCR API settings. The API key is a secret — supply it via
/// `CATALERUM_OCR__MISTRAL__API_KEY` rather than committing it.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct MistralOcrConfig {
    /// Base URL; empty uses the Mistral cloud (`https://api.mistral.ai`). Any
    /// endpoint speaking the same `/v1/ocr` dialect plugs in here.
    pub base_url: String,
    /// API key; the engine is enabled iff non-empty.
    pub api_key: Secret,
    /// OCR model id; empty uses `mistral-ocr-latest`.
    pub model: String,
}

impl MistralOcrConfig {
    /// True when a key is configured (the engine is usable).
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Offline `tesseract` fallback settings. A **runtime** binary, not a build
/// dep: enabled-by-default but probed at startup — no binary (or a missing
/// language pack) just means the engine stays out of the chain.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct TesseractOcrConfig {
    /// Probe for (and chain) the binary at startup.
    pub enabled: bool,
    /// Binary path or name on `$PATH`.
    pub path: String,
    /// `-l` language pack(s), `+`-joined (tesseract codes, e.g. `deu+eng`).
    pub languages: String,
}

impl Default for TesseractOcrConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "tesseract".to_string(),
            languages: "eng".to_string(),
        }
    }
}
