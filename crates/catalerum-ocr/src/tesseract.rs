//! The offline OCR fallback (SOUL §7/§10): shell out to a local `tesseract`
//! binary — `tesseract stdin stdout -l <langs>` with the image on stdin.
//!
//! A **runtime** dependency by design: no build/link dep (the multi-arch CI
//! cross-compile stays untouched) and self-hosters opt in with a plain
//! `apt install tesseract-ocr`. [`TesseractOcr::probe`] detects the binary at
//! startup so a missing install means "engine absent", never a per-job error.

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;

use catalerum_core::error::{Error, Result};
use catalerum_core::ocr::{OcrRequest, OcrResponse};
use catalerum_core::provider::OcrEngine;

use crate::bare_type;

/// The offline `tesseract`-CLI engine.
pub struct TesseractOcr {
    path: String,
    languages: String,
}

impl TesseractOcr {
    /// Use the `tesseract` binary at `path` (empty → `"tesseract"` on `$PATH`)
    /// with the `-l` language pack(s) `languages` (empty → `"eng"`; `+`-join
    /// multiples, e.g. `deu+eng` — tesseract's own codes, not ISO-639-1).
    #[must_use]
    pub fn new(path: &str, languages: &str) -> Self {
        let path = path.trim();
        let languages = languages.trim();
        Self {
            path: if path.is_empty() { "tesseract" } else { path }.to_string(),
            languages: if languages.is_empty() {
                "eng"
            } else {
                languages
            }
            .to_string(),
        }
    }

    /// Whether the configured binary runs **and** every configured language
    /// pack is installed (`tesseract --list-langs`). Called once at startup: a
    /// `false` keeps the engine out of the chain entirely — a present binary
    /// with a missing traineddata file would otherwise fail every single run.
    pub async fn probe(&self) -> bool {
        let output = match Command::new(&self.path)
            .arg("--list-langs")
            .stdin(std::process::Stdio::null())
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            _ => return false,
        };
        let listed: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            // The first line is a "List of available languages …" banner; real
            // entries are bare pack codes.
            .filter(|l| !l.is_empty() && !l.contains(' '))
            .map(str::to_string)
            .collect();
        self.languages
            .split('+')
            .all(|lang| listed.iter().any(|l| l == lang.trim()))
    }

    /// The CLI arguments for one run (factored out for tests): read the image
    /// from stdin, write text to stdout, in `language` (else the configured
    /// pack).
    fn args(&self, language: Option<&str>) -> Vec<String> {
        let lang = language
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(&self.languages);
        vec![
            "stdin".to_string(),
            "stdout".to_string(),
            "-l".to_string(),
            lang.to_string(),
        ]
    }
}

#[async_trait]
impl OcrEngine for TesseractOcr {
    fn name(&self) -> &'static str {
        "tesseract"
    }

    fn supports(&self, content_type: &str) -> bool {
        // The raster formats leptonica reliably decodes; no PDF (that needs
        // rasterization) and no SVG (vector text is an XML concern).
        matches!(
            bare_type(content_type).as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif" | "image/tiff" | "image/bmp"
        )
    }

    async fn ocr(&self, request: OcrRequest) -> Result<OcrResponse> {
        let mut child = Command::new(&self.path)
            .args(self.args(request.language.as_deref()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::Provider(format!("tesseract: failed to spawn: {e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Provider("tesseract: no stdin".into()))?;
        // Write-then-close so tesseract sees EOF; a broken pipe (it bailed
        // early on undecodable bytes) still surfaces via the exit status below.
        let _ = stdin.write_all(&request.document).await;
        drop(stdin);
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| Error::Provider(format!("tesseract: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A clean run that can't read the input is a permanent rejection of
            // these bytes — retrying cannot help — hence Invalid, not Provider.
            return Err(Error::Invalid(format!(
                "tesseract exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        debug!(bytes = text.len(), "tesseract OCR done");
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
    fn defaults_and_args() {
        let t = TesseractOcr::new("", "");
        assert_eq!(t.path, "tesseract");
        assert_eq!(t.args(None), vec!["stdin", "stdout", "-l", "eng"]);
        // A per-request language overrides the configured pack; blanks don't.
        assert_eq!(t.args(Some("deu+eng"))[3], "deu+eng");
        assert_eq!(t.args(Some("  "))[3], "eng");
        let t = TesseractOcr::new(" /usr/bin/tesseract ", "fra");
        assert_eq!(t.path, "/usr/bin/tesseract");
        assert_eq!(t.args(None)[3], "fra");
    }

    #[test]
    fn supports_raster_images_only() {
        let t = TesseractOcr::new("", "");
        assert!(t.supports("image/png"));
        assert!(t.supports("image/TIFF"));
        assert!(!t.supports("application/pdf"));
        assert!(!t.supports("image/svg+xml"));
    }

    /// Round-trip against a real binary — self-skips when tesseract (or any
    /// usable Latin-script pack) is not installed (same spirit as the DB-gated
    /// store tests). Tries `eng` first, then other Latin packs, so the test
    /// exercises the real pipeline on hosts with any of them.
    #[tokio::test]
    async fn ocr_reads_text_from_a_png_when_binary_present() {
        let mut engine = None;
        for lang in ["eng", "afr", "deu", "fra", "spa", "ita", "nld", "por"] {
            let t = TesseractOcr::new("", lang);
            if t.probe().await {
                engine = Some(t);
                break;
            }
        }
        let Some(t) = engine else {
            eprintln!("skipping: no `tesseract` binary / Latin language pack on PATH");
            return;
        };
        let png = include_bytes!("../tests/fixtures/hello.png").to_vec();
        let resp = t
            .ocr(OcrRequest::new(png, "image/png"))
            .await
            .expect("tesseract run");
        assert!(
            resp.text.to_ascii_lowercase().contains("hello"),
            "expected the fixture text, got: {:?}",
            resp.text
        );
        assert_eq!(resp.engine, "tesseract");
    }
}
