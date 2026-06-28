//! The vision-chat OCR engine (SOUL §7/§10): any image-capable chat model on
//! the llmleaf gateway doubles as an OCR engine — one multimodal user turn
//! (the image as a `data:` URI) with a strict transcription prompt, collected
//! to text. Sits between the dedicated OCR API and the offline tesseract
//! fallback in the `catalerum-ocr` chain.

use async_trait::async_trait;
use base64::Engine as _;

use catalerum_core::error::Result;
use catalerum_core::llm::{ChatMessage, ChatRequest};
use catalerum_core::ocr::{OcrRequest, OcrResponse};
use catalerum_core::provider::OcrEngine;

use crate::client::OpenRouterClient;

/// The transcription instruction. Strict "text only" so downstream indexing
/// never catalogues chatty framing ("The image shows…") as document content.
const OCR_SYSTEM_PROMPT: &str = "Transcribe all text visible in the image exactly. \
Preserve the reading order and line breaks. Output only the transcribed text — \
no commentary, no description. If the image contains no text, output nothing.";

/// A vision chat model acting as an [`OcrEngine`] via llmleaf.
pub struct VisionOcr {
    client: OpenRouterClient,
    default_model: String,
}

impl VisionOcr {
    /// OCR through `default_model` (a chat model advertising `image` input);
    /// [`OcrRequest::model`] overrides it per call.
    #[must_use]
    pub fn new(client: OpenRouterClient, default_model: impl Into<String>) -> Self {
        Self {
            client,
            default_model: default_model.into(),
        }
    }
}

/// Build the one-turn transcription conversation (factored out for tests).
fn ocr_messages(request: &OcrRequest) -> Vec<ChatMessage> {
    let data_uri = format!(
        "data:{};base64,{}",
        request
            .content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase(),
        base64::engine::general_purpose::STANDARD.encode(&request.document)
    );
    let mut instruction = String::from("Transcribe the text in this image.");
    if let Some(language) = request.language.as_deref().filter(|l| !l.trim().is_empty()) {
        instruction.push_str(&format!(" The text is in `{}`.", language.trim()));
    }
    let mut user = ChatMessage::user(instruction);
    user.images = vec![data_uri];
    vec![ChatMessage::system(OCR_SYSTEM_PROMPT), user]
}

#[async_trait]
impl OcrEngine for VisionOcr {
    fn name(&self) -> &'static str {
        "vision"
    }

    fn supports(&self, content_type: &str) -> bool {
        // The image types vision providers accept as multimodal input. No PDF
        // (that needs a file-input modality) and no SVG (an XML concern).
        let bare = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        matches!(
            bare.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        )
    }

    async fn ocr(&self, request: OcrRequest) -> Result<OcrResponse> {
        let model = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(&self.default_model)
            .to_string();
        let messages = ocr_messages(&request);
        let turn = self.client.chat(ChatRequest::new(model, messages)).await?;
        Ok(OcrResponse {
            text: turn.content.trim().to_string(),
            engine: self.name().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_one_system_plus_one_image_turn() {
        let req = OcrRequest::new(vec![1, 2, 3], "IMAGE/PNG; foo=bar").with_language("de");
        let msgs = ocr_messages(&req);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, OCR_SYSTEM_PROMPT);
        assert!(msgs[0].images.is_empty());
        assert_eq!(msgs[1].images.len(), 1);
        assert!(
            msgs[1].images[0].starts_with("data:image/png;base64,"),
            "got: {}",
            msgs[1].images[0]
        );
        assert!(msgs[1].content.contains("`de`"));
        // No hint → no language clause.
        let bare = ocr_messages(&OcrRequest::new(vec![], "image/jpeg"));
        assert!(!bare[1].content.contains("text is in"));
    }

    #[test]
    fn supports_multimodal_image_types_only() {
        let engine = VisionOcr::new(OpenRouterClient::new("http://localhost:1", "k"), "m");
        assert!(engine.supports("image/png"));
        assert!(engine.supports("image/webp; q=1"));
        assert!(!engine.supports("application/pdf"));
        assert!(!engine.supports("image/svg+xml"));
    }
}
