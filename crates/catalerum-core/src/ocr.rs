//! Provider-agnostic OCR shapes: image/PDF → text, SOUL §7/§10.
//!
//! Unlike chat/embeddings/audio there is no single gateway modality for OCR —
//! engines range from dedicated OCR APIs (Mistral-style `/v1/ocr`) through
//! vision chat models to an offline `tesseract` binary. These types are what
//! the [`OcrEngine`](crate::provider::OcrEngine) trait consumes and produces;
//! the concrete engines live in `catalerum-ocr` and `catalerum-llm`.

use serde::{Deserialize, Serialize};

/// An OCR request: extract the text of one image (or PDF) document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OcrRequest {
    /// Raw document bytes. Skipped from serialization so logs/events never dump
    /// the blob (transparent about transformations, not about leaking megabytes).
    #[serde(skip)]
    pub document: Vec<u8>,
    /// The document's MIME type (`image/png`, `application/pdf`, …) — engines
    /// route and build data URIs from it.
    pub content_type: String,
    /// ISO-639-1 language hint (a tesseract `-l` pack / a prompt hint).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Per-call model override for engines that take one (the vision engine);
    /// engines with a fixed model ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl OcrRequest {
    /// OCR `document` of type `content_type`.
    #[must_use]
    pub fn new(document: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self {
            document,
            content_type: content_type.into(),
            language: None,
            model: None,
        }
    }

    /// Add an ISO-639-1 language hint.
    #[must_use]
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }

    /// Override the engine's model for this call.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// The result of an [`OcrRequest`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrResponse {
    /// The extracted text (empty when the document contains none).
    pub text: String,
    /// Which engine produced it (`mistral`, `vision`, `tesseract`) — a fallback
    /// chain reports the member that actually served the request.
    pub engine: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ocr_builders_set_fields() {
        let req = OcrRequest::new(vec![1, 2, 3], "image/png")
            .with_language("de")
            .with_model("pixtral");
        assert_eq!(req.content_type, "image/png");
        assert_eq!(req.language.as_deref(), Some("de"));
        assert_eq!(req.model.as_deref(), Some("pixtral"));
    }

    #[test]
    fn ocr_document_is_not_serialized() {
        let req = OcrRequest::new(vec![0xde, 0xad], "image/png").with_language("en");
        let json = serde_json::to_string(&req).unwrap();
        // The document blob must never leak into serialized form.
        assert!(!json.contains("document"));
        assert!(json.contains("image/png"));
        assert!(json.contains("\"en\""));
    }

    #[test]
    fn ocr_response_round_trips() {
        let resp = OcrResponse {
            text: "hello world".into(),
            engine: "tesseract".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: OcrResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(resp, back);
    }
}
