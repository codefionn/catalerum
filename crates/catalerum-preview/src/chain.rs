//! [`PreviewChain`] — the ordered engine chain (SOUL §9/§10): each request is
//! served by the first member that supports its content type and succeeds, so
//! the in-process image engine and the sandbox document engine compose behind
//! one `Arc<dyn Previewer>` (mirroring the OCR `FallbackOcr`).

use std::sync::Arc;

use async_trait::async_trait;
use tracing::warn;

use catalerum_core::error::{Error, Result};
use catalerum_core::preview::{PreviewRequest, PreviewResponse};
use catalerum_core::provider::Previewer;

/// An ordered fallback chain over [`Previewer`]s.
pub struct PreviewChain {
    engines: Vec<Arc<dyn Previewer>>,
}

impl PreviewChain {
    /// Chain `engines` in priority order (first = preferred).
    #[must_use]
    pub fn new(engines: Vec<Arc<dyn Previewer>>) -> Self {
        Self { engines }
    }

    /// True when no engine is configured (callers then skip previews entirely).
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
impl Previewer for PreviewChain {
    fn name(&self) -> &'static str {
        "chain"
    }

    fn supports(&self, content_type: &str) -> bool {
        self.engines.iter().any(|e| e.supports(content_type))
    }

    async fn preview(&self, request: PreviewRequest) -> Result<PreviewResponse> {
        let mut last_err: Option<Error> = None;
        for engine in &self.engines {
            if !engine.supports(&request.content_type) {
                continue;
            }
            match engine.preview(request.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(e) => {
                    warn!(engine = engine.name(), error = %e, "preview engine failed; trying next");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            Error::Unsupported(format!(
                "no preview engine supports `{}`",
                request.content_type
            ))
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use catalerum_core::preview::PreviewFormat;

    use super::*;

    /// A scriptable engine: supports one type, answers a canned result.
    struct Fake {
        name: &'static str,
        supports: &'static str,
        result: std::result::Result<u32, fn() -> Error>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Previewer for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn supports(&self, content_type: &str) -> bool {
            content_type == self.supports
        }
        async fn preview(&self, _request: PreviewRequest) -> Result<PreviewResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.result {
                Ok(width) => Ok(PreviewResponse {
                    image: vec![],
                    content_type: PreviewFormat::Webp.content_type().to_string(),
                    width: *width,
                    height: 1,
                    page_count: 1,
                    engine: self.name.to_string(),
                }),
                Err(make) => Err(make()),
            }
        }
    }

    fn fake(
        name: &'static str,
        supports: &'static str,
        result: std::result::Result<u32, fn() -> Error>,
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
        let a = fake("a", "image/png", Ok(11));
        let b = fake("b", "image/png", Ok(22));
        let chain = PreviewChain::new(vec![a, b.clone()]);
        let resp = chain
            .preview(PreviewRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.engine, "a");
        assert_eq!(b.calls.load(Ordering::SeqCst), 0, "b never consulted");
    }

    #[tokio::test]
    async fn unsupporting_members_are_skipped() {
        let pdf = fake("pdf", "application/pdf", Ok(1));
        let png = fake("png", "image/png", Ok(2));
        let chain = PreviewChain::new(vec![pdf.clone(), png]);
        let resp = chain
            .preview(PreviewRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.engine, "png");
        assert_eq!(pdf.calls.load(Ordering::SeqCst), 0);
        assert!(chain.supports("application/pdf"));
        assert!(!chain.supports("image/svg+xml"));
    }

    #[tokio::test]
    async fn a_failing_engine_falls_through() {
        let flaky = fake("flaky", "image/png", Err(|| Error::Provider("boom".into())));
        let solid = fake("solid", "image/png", Ok(9));
        let chain = PreviewChain::new(vec![flaky.clone(), solid]);
        let resp = chain
            .preview(PreviewRequest::new(vec![], "image/png"))
            .await
            .unwrap();
        assert_eq!(resp.engine, "solid");
        assert_eq!(flaky.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_supporting_is_unsupported() {
        let chain = PreviewChain::new(vec![fake("a", "image/png", Ok(1))]);
        let err = chain
            .preview(PreviewRequest::new(vec![], "application/pdf"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "got: {err:?}");
        assert!(PreviewChain::new(vec![]).is_empty());
        assert_eq!(chain.engine_names(), vec!["a"]);
    }
}
