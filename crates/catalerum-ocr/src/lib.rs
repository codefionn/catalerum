//! catalerum-ocr — [`OcrEngine`](catalerum_core::provider::OcrEngine) impls
//! (SOUL §7/§10): document → text without core ever naming a vendor.
//!
//! Engines:
//! - [`MistralOcr`] — a dedicated OCR API speaking the Mistral `/v1/ocr`
//!   dialect (cloud or any compatible self-hosted endpoint). The strongest
//!   option: layout-aware Markdown, and the only engine that takes PDFs.
//! - [`TesseractOcr`] — the **offline fallback**: shells out to a local
//!   `tesseract` binary (a runtime dependency, deliberately not a build/link
//!   one, so cross-compiled CI images stay untouched).
//! - [`FallbackOcr`] — an ordered chain that serves each request from the
//!   first engine that supports its content type and succeeds.
//!
//! The third engine — a vision chat model via llmleaf — lives in
//! `catalerum-llm` (it is a chat-client concern); the chain composes all of
//! them behind one `Arc<dyn OcrEngine>`.

#![forbid(unsafe_code)]

mod chain;
mod mistral;
mod tesseract;

pub use chain::FallbackOcr;
pub use mistral::{MistralOcr, MISTRAL_DEFAULT_BASE_URL, MISTRAL_DEFAULT_MODEL};
pub use tesseract::TesseractOcr;

/// The bare, lowercased media type of `content_type` — parameters such as
/// `; charset=…` stripped — mirroring how `catalerum-ingest` classifies text.
#[must_use]
pub(crate) fn bare_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::bare_type;

    #[test]
    fn bare_type_strips_parameters_and_case() {
        assert_eq!(bare_type("IMAGE/PNG"), "image/png");
        assert_eq!(bare_type("image/jpeg; quality=85"), "image/jpeg");
        assert_eq!(bare_type("  application/pdf ; x=y"), "application/pdf");
        assert_eq!(bare_type(""), "");
    }
}
