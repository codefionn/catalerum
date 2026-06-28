//! [`DocumentPreviewer`] — the document engine (SOUL §9/§10): render a PDF /
//! office document / spreadsheet / presentation / SVG to a first-page image by
//! shelling the local **poppler** (`pdfinfo`, `pdftoppm`) and **LibreOffice**
//! (`soffice`) binaries. Office formats convert to PDF via LibreOffice first;
//! the chosen PDF page then rasters via poppler and re-encodes through the
//! shared image engine into the requested format.
//!
//! Runtime dependencies by design (no build/link deps): the preview service
//! image ships `poppler-utils` + `libreoffice-nogui`. [`DocumentPreviewer::probe`]
//! checks the binaries at startup so a missing install keeps the engine out of
//! the chain, never a per-job error.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;
use tokio::process::Command;
use tracing::debug;

use catalerum_core::error::{Error, Result};
use catalerum_core::preview::{bare_media_type, is_document_type, PreviewRequest, PreviewResponse};
use catalerum_core::provider::Previewer;

/// Default per-render wall-clock budget (seconds). Generous — a cold LibreOffice
/// start plus the office→PDF conversion can be slow.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Renders documents via the poppler + LibreOffice CLIs.
pub struct DocumentPreviewer {
    pdftoppm: String,
    pdfinfo: String,
    soffice: String,
    timeout_secs: u64,
}

impl Default for DocumentPreviewer {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentPreviewer {
    /// Use the default binary names on `$PATH` (`pdftoppm`, `pdfinfo`, `soffice`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            pdftoppm: "pdftoppm".to_string(),
            pdfinfo: "pdfinfo".to_string(),
            soffice: "soffice".to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Override the per-render wall-clock timeout (seconds; 0 → the default).
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        if secs > 0 {
            self.timeout_secs = secs;
        }
        self
    }

    /// Whether the poppler binaries run (`pdftoppm -v`). Called once at startup:
    /// a `false` keeps the engine out of the chain so a bare install serves only
    /// image previews rather than failing every document. (LibreOffice is only
    /// needed for office formats, so it is not required for the engine to load —
    /// PDFs work with poppler alone.)
    pub async fn probe(&self) -> bool {
        Command::new(&self.pdftoppm)
            .arg("-v")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Run a CLI to completion under the timeout, capturing output. A non-zero
    /// exit is `Invalid` (these bytes can't be rendered — retrying won't help);
    /// a spawn failure is `Provider`; a timeout is `Timeout`.
    async fn exec(&self, program: &str, args: &[&str]) -> Result<std::process::Output> {
        let fut = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output();
        match tokio::time::timeout(Duration::from_secs(self.timeout_secs), fut).await {
            Err(_) => Err(Error::Timeout),
            Ok(Err(e)) => Err(Error::Provider(format!("{program}: failed to spawn: {e}"))),
            Ok(Ok(out)) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                Err(Error::Invalid(format!(
                    "{program} exited with {}: {}",
                    out.status,
                    tail(stderr.trim(), 400)
                )))
            }
            Ok(Ok(out)) => Ok(out),
        }
    }
}

/// The file extension to write for LibreOffice to detect an office/SVG input.
/// `None` = not an office format handled here (PDF is rendered directly).
fn office_ext(bare: &str) -> Option<&'static str> {
    Some(match bare {
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "application/vnd.oasis.opendocument.text" => "odt",
        "application/rtf" | "text/rtf" => "rtf",
        "application/vnd.ms-excel" => "xls",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => "xlsx",
        "application/vnd.oasis.opendocument.spreadsheet" => "ods",
        "text/csv" => "csv",
        "application/vnd.ms-powerpoint" => "ppt",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => "pptx",
        "application/vnd.oasis.opendocument.presentation" => "odp",
        "image/svg+xml" => "svg",
        _ => return None,
    })
}

/// The last `n` chars of `s` (char-safe), the actionable tail of a CLI stderr.
fn tail(s: &str, n: usize) -> String {
    let rev: String = s.chars().rev().take(n).collect();
    rev.chars().rev().collect()
}

/// Parse poppler `pdfinfo` output for the `Pages:` count (≥1).
fn parse_pages(pdfinfo_stdout: &str) -> u32 {
    pdfinfo_stdout
        .lines()
        .find_map(|l| l.strip_prefix("Pages:"))
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(1)
        .max(1)
}

#[async_trait]
impl Previewer for DocumentPreviewer {
    fn name(&self) -> &'static str {
        "document"
    }

    fn supports(&self, content_type: &str) -> bool {
        is_document_type(content_type)
    }

    async fn preview(&self, request: PreviewRequest) -> Result<PreviewResponse> {
        let bare = bare_media_type(&request.content_type);
        let dir = tempdir().map_err(|e| Error::Provider(format!("preview tempdir: {e}")))?;
        let path = |name: &str| {
            dir.path()
                .join(name)
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| Error::Provider("non-utf8 temp path".into()))
        };

        // Resolve a PDF on disk: a PDF passes straight through; an office/SVG
        // input converts via LibreOffice first.
        let is_pdf = bare == "application/pdf" || request.document.starts_with(b"%PDF-");
        let pdf_path = if is_pdf {
            let p = path("in.pdf")?;
            tokio::fs::write(&p, &request.document)
                .await
                .map_err(|e| Error::Provider(format!("write pdf: {e}")))?;
            p
        } else if let Some(ext) = office_ext(&bare) {
            let src = path(&format!("in.{ext}"))?;
            tokio::fs::write(&src, &request.document)
                .await
                .map_err(|e| Error::Provider(format!("write source: {e}")))?;
            let outdir = path("")?;
            // A per-call `UserInstallation` profile lets concurrent renders run
            // without contending on LibreOffice's shared profile lock.
            let userinst = format!("-env:UserInstallation=file://{}", path("loprofile")?);
            self.exec(
                &self.soffice,
                &[
                    "--headless",
                    "--norestore",
                    "--nolockcheck",
                    &userinst,
                    "--convert-to",
                    "pdf",
                    "--outdir",
                    outdir.trim_end_matches('/'),
                    &src,
                ],
            )
            .await?;
            let produced = path("in.pdf")?;
            if tokio::fs::metadata(&produced).await.is_err() {
                return Err(Error::Invalid(
                    "LibreOffice produced no PDF for this document".into(),
                ));
            }
            produced
        } else {
            return Err(Error::Unsupported(format!(
                "no document engine supports `{}`",
                request.content_type
            )));
        };

        // Page count + the clamped 1-indexed page to render.
        let info = self.exec(&self.pdfinfo, &[&pdf_path]).await?;
        let pages = parse_pages(&String::from_utf8_lossy(&info.stdout));
        let page = request.page.clamp(1, pages);

        // Raster the page to a PNG whose longest side is ≈ max_dimension.
        let prefix = path("page")?;
        let page_s = page.to_string();
        let scale_s = request.max_dimension.max(16).to_string();
        self.exec(
            &self.pdftoppm,
            &[
                "-png",
                "-f",
                &page_s,
                "-l",
                &page_s,
                "-scale-to",
                &scale_s,
                "-singlefile",
                &pdf_path,
                &prefix,
            ],
        )
        .await?;
        let png = tokio::fs::read(format!("{prefix}.png"))
            .await
            .map_err(|e| Error::Provider(format!("read rendered page: {e}")))?;

        // Re-encode the page raster into the requested format (shared image path).
        let (max, format) = (request.max_dimension, request.format);
        let (image, used, width, height) =
            tokio::task::spawn_blocking(move || crate::image::render(&png, max, format))
                .await
                .map_err(|e| Error::other(format!("preview task join: {e}")))??;
        debug!(pages, page, width, height, "document preview rendered");
        Ok(PreviewResponse {
            image,
            content_type: used.content_type().to_string(),
            width,
            height,
            page_count: pages,
            engine: self.name().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supports_documents_not_images() {
        let e = DocumentPreviewer::new();
        assert!(e.supports("application/pdf"));
        assert!(e.supports("image/svg+xml"));
        assert!(e.supports("text/csv"));
        assert!(
            e.supports("application/vnd.openxmlformats-officedocument.presentationml.presentation")
        );
        assert!(!e.supports("image/png"));
        assert!(!e.supports("text/plain"));
    }

    #[test]
    fn office_ext_maps_the_family() {
        assert_eq!(office_ext("text/csv"), Some("csv"));
        assert_eq!(
            office_ext("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
            Some("xlsx")
        );
        assert_eq!(office_ext("image/svg+xml"), Some("svg"));
        assert_eq!(office_ext("application/pdf"), None);
        assert_eq!(office_ext("image/png"), None);
    }

    #[test]
    fn parse_pages_reads_the_count() {
        let out = "Title:  x\nPages:  7\nEncrypted: no\n";
        assert_eq!(parse_pages(out), 7);
        assert_eq!(parse_pages("no pages line"), 1);
        assert_eq!(parse_pages("Pages: 0"), 1);
    }

    /// Round-trip against the real binaries — self-skips when poppler is absent
    /// (same spirit as the DB-gated store tests). Renders a minimal one-page PDF.
    #[tokio::test]
    async fn renders_a_pdf_when_poppler_present() {
        let e = DocumentPreviewer::new();
        if !e.probe().await {
            eprintln!("skipping: no `pdftoppm` on PATH");
            return;
        }
        // A hand-written minimal single-page PDF (no external deps).
        let pdf = b"%PDF-1.1\n1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n2 0 obj<</Type/Pages/Kids[3 0 R]/Count 1>>endobj\n3 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>endobj\nxref\n0 4\n0000000000 65535 f \n0000000009 00000 n \n0000000052 00000 n \n0000000101 00000 n \ntrailer<</Size 4/Root 1 0 R>>\nstartxref\n164\n%%EOF".to_vec();
        let resp = e
            .preview(PreviewRequest::new(pdf, "application/pdf").with_max_dimension(128))
            .await
            .expect("pdf render");
        assert_eq!(resp.engine, "document");
        assert_eq!(resp.page_count, 1);
        assert!(resp.width <= 128 && resp.height <= 128);
        assert!(!resp.image.is_empty());
    }
}
