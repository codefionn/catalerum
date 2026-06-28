//! Provider-agnostic preview shapes: a document → a raster **image** preview
//! (SOUL §9/§10) — the first page of a PDF/office document, a rendered
//! spreadsheet/presentation, or a resized thumbnail of an image.
//!
//! Unlike chat/embeddings there is no single gateway modality for previews —
//! image formats decode in-process (the pure-Rust engine), while PDF/office/
//! presentation/spreadsheet rendering needs the batteries-included tools
//! (LibreOffice, poppler, pymupdf) that only exist in the exec sandbox. These
//! types are what the [`Previewer`](crate::provider::Previewer) trait consumes
//! and produces; the concrete engines live in `catalerum-preview`.

use serde::{Deserialize, Serialize};

/// The encoding of a rendered preview image. WebP is the default — the smallest
/// lossy container for a thumbnail, which matters because the sandbox engine
/// returns the bytes over a size-capped channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewFormat {
    /// `image/webp` — the default (smallest for a thumbnail).
    #[default]
    Webp,
    /// `image/png` — lossless, for previews that must stay crisp.
    Png,
    /// `image/jpeg` — lossy, widely compatible.
    Jpeg,
}

impl PreviewFormat {
    /// The MIME type this format encodes to.
    #[must_use]
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Webp => "image/webp",
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }

    /// The bare file extension (no dot).
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Webp => "webp",
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }

    /// Parse a format token (`webp` | `png` | `jpeg`/`jpg`), case-insensitively.
    /// An empty or unknown token yields the [default](PreviewFormat::default)
    /// (WebP) so a caller never has to error on a missing query parameter.
    #[must_use]
    pub fn parse_or_default(token: &str) -> Self {
        match token.trim().to_ascii_lowercase().as_str() {
            "png" => Self::Png,
            "jpeg" | "jpg" => Self::Jpeg,
            _ => Self::Webp,
        }
    }
}

/// The default longest-side pixel bound for a preview when the caller sets none.
pub const DEFAULT_MAX_DIMENSION: u32 = 1024;

/// A preview request: render one document to a single fitted image.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewRequest {
    /// Raw document bytes. Skipped from serialization so logs/events never dump
    /// the blob (transparent about transformations, not about leaking megabytes).
    #[serde(skip)]
    pub document: Vec<u8>,
    /// The document's MIME type (`application/pdf`, `image/png`, an office type,
    /// …) — engines route on it.
    pub content_type: String,
    /// Longest-side pixel bound: the image is scaled to fit a `max_dimension`
    /// square, preserving aspect ratio and **never upscaling**.
    pub max_dimension: u32,
    /// The encoding of the returned image.
    pub format: PreviewFormat,
    /// Which page to render, **1-indexed** (paged documents only; images ignore
    /// it). Clamped to the document's page count by the engine.
    pub page: u32,
}

impl PreviewRequest {
    /// Preview `document` of type `content_type` with the default bound, WebP,
    /// page 1.
    #[must_use]
    pub fn new(document: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self {
            document,
            content_type: content_type.into(),
            max_dimension: DEFAULT_MAX_DIMENSION,
            format: PreviewFormat::default(),
            page: 1,
        }
    }

    /// Set the longest-side pixel bound (0 → the default, never zero).
    #[must_use]
    pub fn with_max_dimension(mut self, max_dimension: u32) -> Self {
        self.max_dimension = if max_dimension == 0 {
            DEFAULT_MAX_DIMENSION
        } else {
            max_dimension
        };
        self
    }

    /// Set the output image format.
    #[must_use]
    pub fn with_format(mut self, format: PreviewFormat) -> Self {
        self.format = format;
        self
    }

    /// Set the 1-indexed page to render (0 → page 1).
    #[must_use]
    pub fn with_page(mut self, page: u32) -> Self {
        self.page = page.max(1);
        self
    }
}

/// The result of a [`PreviewRequest`]: the rendered image plus its dimensions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewResponse {
    /// The rendered image bytes. Skipped from serialization (a binary blob, like
    /// [`PreviewRequest::document`]).
    #[serde(skip)]
    pub image: Vec<u8>,
    /// The image's MIME type (matches the requested [`PreviewFormat`]).
    pub content_type: String,
    /// Rendered image width in pixels.
    pub width: u32,
    /// Rendered image height in pixels.
    pub height: u32,
    /// The source document's total page count (`1` for images / single-page
    /// documents) — lets a caller show "page 1 of N".
    pub page_count: u32,
    /// Which engine produced it (`image`, `sandbox`, …) — a fallback chain
    /// reports the member that actually served the request.
    pub engine: String,
}

/// The bare, lowercased media type of `content_type` (parameters such as
/// `; charset=…` stripped) — the form the preview type predicates match on.
#[must_use]
pub fn bare_media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Whether `content_type` is a raster image the **in-process image engine**
/// decodes (the pure-Rust `image`-crate formats). SVG is excluded — vector text
/// is a document-engine concern. Provider-agnostic (a format predicate, not a
/// vendor), so it lives in core as the single source both the engines and the
/// HTTP client agree on.
#[must_use]
pub fn is_image_type(content_type: &str) -> bool {
    matches!(
        bare_media_type(content_type).as_str(),
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/bmp"
            | "image/x-bmp"
            | "image/x-ms-bmp"
            | "image/tiff"
            | "image/x-tiff"
            | "image/x-icon"
            | "image/vnd.microsoft.icon"
            | "image/x-portable-anymap"
            | "image/x-portable-bitmap"
            | "image/x-portable-graymap"
            | "image/x-portable-pixmap"
            | "image/x-targa"
            | "image/x-tga"
            | "image/qoi"
            | "image/x-qoi"
            | "image/vnd.radiance"
            | "image/x-exr"
    )
}

/// Whether `content_type` is a document the **document engine** renders via the
/// LibreOffice/poppler toolchain: PDF, the office family (documents /
/// spreadsheets / presentations — OOXML + legacy + OpenDocument), CSV, RTF, SVG.
#[must_use]
pub fn is_document_type(content_type: &str) -> bool {
    matches!(
        bare_media_type(content_type).as_str(),
        "application/pdf"
            | "image/svg+xml"
            | "application/msword"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            | "application/vnd.oasis.opendocument.text"
            | "application/rtf"
            | "text/rtf"
            | "application/vnd.ms-excel"
            | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.oasis.opendocument.spreadsheet"
            | "text/csv"
            | "application/vnd.ms-powerpoint"
            | "application/vnd.openxmlformats-officedocument.presentationml.presentation"
            | "application/vnd.oasis.opendocument.presentation"
    )
}

/// Whether any preview engine can render `content_type` — the union the HTTP
/// client checks to reject an obviously-unpreviewable type without a round-trip.
#[must_use]
pub fn is_previewable(content_type: &str) -> bool {
    is_image_type(content_type) || is_document_type(content_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_predicates_partition_by_engine() {
        assert!(is_image_type("image/PNG"));
        assert!(is_image_type("image/jpeg; quality=85"));
        assert!(!is_image_type("image/svg+xml"));
        assert!(!is_image_type("application/pdf"));

        assert!(is_document_type("application/pdf"));
        assert!(is_document_type("image/svg+xml"));
        assert!(is_document_type("text/csv"));
        assert!(is_document_type(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        ));
        assert!(!is_document_type("image/png"));

        assert!(is_previewable("image/webp"));
        assert!(is_previewable("application/pdf"));
        assert!(!is_previewable("text/plain"));
        assert!(!is_previewable("application/zip"));
    }

    #[test]
    fn format_content_types_and_parse() {
        assert_eq!(PreviewFormat::Webp.content_type(), "image/webp");
        assert_eq!(PreviewFormat::Png.extension(), "png");
        assert_eq!(PreviewFormat::Jpeg.extension(), "jpg");
        assert_eq!(PreviewFormat::parse_or_default("PNG"), PreviewFormat::Png);
        assert_eq!(PreviewFormat::parse_or_default("jpg"), PreviewFormat::Jpeg);
        assert_eq!(PreviewFormat::parse_or_default("jpeg"), PreviewFormat::Jpeg);
        // Empty / unknown → the WebP default (never an error).
        assert_eq!(PreviewFormat::parse_or_default(""), PreviewFormat::Webp);
        assert_eq!(PreviewFormat::parse_or_default("gif"), PreviewFormat::Webp);
    }

    #[test]
    fn builders_set_fields_with_sane_floors() {
        let req = PreviewRequest::new(vec![1, 2, 3], "application/pdf")
            .with_max_dimension(256)
            .with_format(PreviewFormat::Png)
            .with_page(3);
        assert_eq!(req.content_type, "application/pdf");
        assert_eq!(req.max_dimension, 256);
        assert_eq!(req.format, PreviewFormat::Png);
        assert_eq!(req.page, 3);
        // Zero floors: never a zero dimension, never a zero page.
        let req = PreviewRequest::new(vec![], "image/png")
            .with_max_dimension(0)
            .with_page(0);
        assert_eq!(req.max_dimension, DEFAULT_MAX_DIMENSION);
        assert_eq!(req.page, 1);
    }

    #[test]
    fn document_and_image_blobs_are_not_serialized() {
        let req = PreviewRequest::new(vec![0xde, 0xad], "application/pdf");
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("document"));
        assert!(json.contains("application/pdf"));

        let resp = PreviewResponse {
            image: vec![0xff, 0xd8],
            content_type: "image/webp".into(),
            width: 800,
            height: 600,
            page_count: 4,
            engine: "sandbox".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("image\"") && !json.contains("[255"));
        assert!(json.contains("image/webp"));
        assert!(json.contains("\"page_count\":4"));
        // The metadata round-trips (the blob is intentionally dropped).
        let back: PreviewResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.width, 800);
        assert_eq!(back.page_count, 4);
        assert!(back.image.is_empty());
    }
}
