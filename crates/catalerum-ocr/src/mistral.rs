//! The Mistral-dialect OCR API engine (SOUL §7/§10): `POST {base}/v1/ocr` with
//! the document riding in-band as a `data:` URI, answering layout-aware
//! Markdown per page. `base_url` is configurable so any compatible endpoint —
//! Mistral cloud or a self-hosted server speaking the same dialect — plugs in.

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tracing::debug;

use catalerum_core::error::{Error, Result};
use catalerum_core::ocr::{OcrRequest, OcrResponse};
use catalerum_core::provider::OcrEngine;

use crate::bare_type;

/// The Mistral cloud endpoint, used when no `base_url` is configured.
pub const MISTRAL_DEFAULT_BASE_URL: &str = "https://api.mistral.ai";

/// The default OCR model id (a routing alias that tracks the latest release).
pub const MISTRAL_DEFAULT_MODEL: &str = "mistral-ocr-latest";

/// Per-request timeout. OCR of a many-page PDF is slow; generous but bounded so
/// a hung upstream still surfaces as [`Error::Timeout`] and the job can retry.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// A dedicated-OCR-API engine speaking the Mistral `/v1/ocr` dialect.
pub struct MistralOcr {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

impl MistralOcr {
    /// Build against `base_url` (empty → [`MISTRAL_DEFAULT_BASE_URL`]) with
    /// `model` (empty → [`MISTRAL_DEFAULT_MODEL`]).
    #[must_use]
    pub fn new(base_url: &str, api_key: impl Into<String>, model: &str) -> Self {
        let base_url = base_url.trim().trim_end_matches('/');
        let model = model.trim();
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("reqwest client"),
            base_url: if base_url.is_empty() {
                MISTRAL_DEFAULT_BASE_URL.to_string()
            } else {
                base_url.to_string()
            },
            api_key: api_key.into(),
            model: if model.is_empty() {
                MISTRAL_DEFAULT_MODEL.to_string()
            } else {
                model.to_string()
            },
        }
    }
}

/// The `document` union of the OCR dialect: images ride as `image_url`, PDFs as
/// `document_url` — both accept `data:` URIs, so the bytes stay in-band.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OcrDocument {
    #[serde(rename = "image_url")]
    Image { image_url: String },
    #[serde(rename = "document_url")]
    Document { document_url: String },
}

#[derive(Debug, Serialize)]
struct OcrApiRequest {
    model: String,
    document: OcrDocument,
    /// The pages' embedded images re-encoded into the response — never wanted
    /// here (we keep only the text), so spare the bandwidth.
    include_image_base64: bool,
}

/// One recognized page. Only `markdown` matters to us; the dialect's `images` /
/// `dimensions` fields are ignored by serde.
#[derive(Debug, Deserialize)]
struct OcrApiPage {
    #[serde(default)]
    markdown: String,
}

#[derive(Debug, Deserialize)]
struct OcrApiResponse {
    #[serde(default)]
    pages: Vec<OcrApiPage>,
}

/// Build the request body for `request` (factored out for tests).
fn api_request(model: &str, request: &OcrRequest) -> OcrApiRequest {
    let data_uri = format!(
        "data:{};base64,{}",
        bare_type(&request.content_type),
        base64::engine::general_purpose::STANDARD.encode(&request.document)
    );
    let document = if bare_type(&request.content_type) == "application/pdf" {
        OcrDocument::Document {
            document_url: data_uri,
        }
    } else {
        OcrDocument::Image {
            image_url: data_uri,
        }
    };
    OcrApiRequest {
        model: model.to_string(),
        document,
        include_image_base64: false,
    }
}

/// Join the pages' Markdown into one document text.
fn join_pages(response: OcrApiResponse) -> String {
    response
        .pages
        .into_iter()
        .map(|p| p.markdown.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Map a non-2xx OCR API status to the [`Error`] class the ingest retry
/// contract keys on: client-side rejections (bad key, bad payload, unknown
/// model) are permanent — retrying the same bytes cannot succeed — while
/// rate limits and server errors are transient.
fn status_error(status: reqwest::StatusCode, body: &str) -> Error {
    let msg = format!("mistral OCR: HTTP {status}: {body}");
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        Error::Provider(msg)
    } else {
        Error::Invalid(msg)
    }
}

#[async_trait]
impl OcrEngine for MistralOcr {
    fn name(&self) -> &'static str {
        "mistral"
    }

    fn supports(&self, content_type: &str) -> bool {
        matches!(
            bare_type(content_type).as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "application/pdf"
        )
    }

    async fn ocr(&self, request: OcrRequest) -> Result<OcrResponse> {
        let body = api_request(&self.model, &request);
        let url = format!("{}/v1/ocr", self.base_url);
        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Error::Timeout
                } else {
                    Error::Provider(format!("mistral OCR: {e}"))
                }
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(status_error(status, body.trim()));
        }
        let parsed: OcrApiResponse = response
            .json()
            .await
            .map_err(|e| Error::Provider(format!("mistral OCR: malformed response: {e}")))?;
        let text = join_pages(parsed);
        debug!(model = %self.model, bytes = text.len(), "mistral OCR done");
        Ok(OcrResponse {
            text,
            engine: self.name().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_ride_as_image_url_data_uris() {
        let req = OcrRequest::new(vec![1, 2, 3], "image/png");
        let body = api_request("mistral-ocr-latest", &req);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["model"], "mistral-ocr-latest");
        assert_eq!(json["document"]["type"], "image_url");
        let uri = json["document"]["image_url"].as_str().unwrap();
        assert!(uri.starts_with("data:image/png;base64,"), "got: {uri}");
        assert_eq!(json["include_image_base64"], false);
    }

    #[test]
    fn pdfs_ride_as_document_url() {
        let req = OcrRequest::new(vec![b'%', b'P', b'D', b'F'], "application/pdf; x=y");
        let body = api_request("m", &req);
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["document"]["type"], "document_url");
        let uri = json["document"]["document_url"].as_str().unwrap();
        assert!(
            uri.starts_with("data:application/pdf;base64,"),
            "got: {uri}"
        );
    }

    #[test]
    fn pages_join_with_blank_lines_and_skip_empties() {
        let parsed: OcrApiResponse = serde_json::from_str(
            r##"{"pages":[{"index":0,"markdown":"# One"},{"index":1,"markdown":"  "},{"index":2,"markdown":"Two"}],"model":"m","usage_info":{"pages_processed":3}}"##,
        )
        .unwrap();
        assert_eq!(join_pages(parsed), "# One\n\nTwo");
    }

    #[test]
    fn supports_images_and_pdf_only() {
        let engine = MistralOcr::new("", "key", "");
        assert!(engine.supports("image/png"));
        assert!(engine.supports("IMAGE/JPEG; q=1"));
        assert!(engine.supports("application/pdf"));
        assert!(!engine.supports("image/svg+xml"));
        assert!(!engine.supports("text/plain"));
        assert_eq!(engine.base_url, MISTRAL_DEFAULT_BASE_URL);
        assert_eq!(engine.model, MISTRAL_DEFAULT_MODEL);
    }

    #[test]
    fn status_classification_matches_retry_contract() {
        // Permanent client-side rejections must NOT retry (clean skip at ingest).
        assert!(matches!(
            status_error(reqwest::StatusCode::UNAUTHORIZED, "bad key"),
            Error::Invalid(_)
        ));
        assert!(matches!(
            status_error(reqwest::StatusCode::UNPROCESSABLE_ENTITY, "bad doc"),
            Error::Invalid(_)
        ));
        // Transient upstream trouble retries.
        assert!(matches!(
            status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down"),
            Error::Provider(_)
        ));
        assert!(matches!(
            status_error(reqwest::StatusCode::BAD_GATEWAY, "oops"),
            Error::Provider(_)
        ));
    }
}
