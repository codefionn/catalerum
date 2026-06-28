//! [`FallbackOcr`] — the ordered engine chain (SOUL §10): each request is
//! served by the first member that supports its content type and succeeds, so
//! a dedicated OCR API degrades to the offline fallback instead of failing.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use catalerum_core::error::{Error, Result};
use catalerum_core::ocr::{OcrRequest, OcrResponse};
use catalerum_core::provider::OcrEngine;

/// An ordered fallback chain over [`OcrEngine`]s.
pub struct FallbackOcr {
    engines: Vec<Arc<dyn OcrEngine>>,
}

impl FallbackOcr {
    /// Chain `engines` in priority order (first = preferred).
    #[must_use]
    pub fn new(engines: Vec<Arc<dyn OcrEngine>>) -> Self {
        Self { engines }
    }

    /// True when no engine is configured (callers then skip OCR entirely).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.engines.is_empty()
    }

    /// The member names in chain order (status/logging surface).
    #[must_use]
    pub fn engine_names(&self) -> Vec<&'static str> {
        self.engines.iter().map(|e| e.name()).collect()
    }
}

#[async_trait]
impl OcrEngine for FallbackOcr {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn supports(&self, content_type: &str) -> bool {
        self.engines.iter().any(|e| e.supports(content_type))
    }

    async fn ocr(&self, request: OcrRequest) -> Result<OcrResponse> {
        let mut last_err: Option<Error> = None;
        for engine in &self.engines {
            if !engine.supports(&request.content_type) {
                continue;
            }
            match engine.ocr(request.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    warn!(engine = engine.name(), error = %e, "OCR engine failed; trying next");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            Error::Unsupported(format!("no OCR engine supports `{}`", request.content_type))
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A scriptable engine: supports one type, answers a canned result.
    struct Fake {
        name: &'static str,
        supports: &'static str,
        result: std::result::Result<&'static str, fn() -> Error>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl OcrEngine for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supports(&self, content_type: &str) -> bool {
            content_type == self.supports
        }
        async fn ocr(&self, _request: OcrRequest) -> Result<OcrResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.result {
                Ok(text) => Ok(OcrResponse {
                    text: (*text).to_string(),
                    engine: self.name.to_string(),
                }),
                Err(make) => Err(make()),
            }
        }
    }

    fn fake(
        name: &'static str,
        supports: &'static str,
        result: std::result::Result<&'static str, fn() -> Error>,
    ) -> Arc<Fake> {
        Arc::new(Fake {
            name,
            supports,
            result,
            calls: AtomicUsize::new(0),
        })
    }

    #[tokio::test]
    async fn first_supporting_engine_wins() {
        let a = fake("a", "image/png", Ok("from a"));
        let b = fake("b", "image/png", Ok("from b"));
        let chain = FallbackOcr::new(vec![a.clone(), b.clone()]);
        let resp = chain
            .ocr(OcrRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.text, "from a");
        assert_eq!(resp.engine, "a");
        assert_eq!(b.calls.load(Ordering::SeqCst), 0, "b never consulted");
    }

    #[tokio::test]
    async fn unsupporting_members_are_skipped() {
        let pdf_only = fake("pdf", "application/pdf", Ok("pdf text"));
        let png = fake("png", "image/png", Ok("png text"));
        let chain = FallbackOcr::new(vec![pdf_only.clone(), png]);
        let resp = chain
            .ocr(OcrRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.engine, "png");
        assert_eq!(pdf_only.calls.load(Ordering::SeqCst), 0);
        assert!(chain.supports("application/pdf"));
        assert!(chain.supports("image/png"));
        assert!(!chain.supports("image/svg+xml"));
    }

    #[tokio::test]
    async fn a_failing_engine_falls_through_to_the_next() {
        let flaky = fake("flaky", "image/png", Err(|| Error::Provider("boom".into())));
        let solid = fake("solid", "image/png", Ok("rescued"));
        let chain = FallbackOcr::new(vec![flaky.clone(), solid]);
        let resp = chain
            .ocr(OcrRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.text, "rescued");
        assert_eq!(flaky.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn all_failing_returns_the_last_error() {
        let a = fake("a", "image/png", Err(|| Error::Provider("first".into())));
        let b = fake("b", "image/png", Err(|| Error::Invalid("second".into())));
        let chain = FallbackOcr::new(vec![a, b]);
        let err = chain
            .ocr(OcrRequest::new(vec![], "image/png"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn nothing_supporting_is_unsupported() {
        let chain = FallbackOcr::new(vec![fake("a", "image/png", Ok("x"))]);
        let err = chain
            .ocr(OcrRequest::new(vec![], "image/svg+xml"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got: {err:?}");
        assert!(FallbackOcr::new(vec![]).is_empty());
        assert_eq!(chain.engine_names(), vec!["a"]);
    }
}
