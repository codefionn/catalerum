//! catalerum-preview — the render engines behind the standalone **preview
//! service** (SOUL §9/§10): a document → a raster image preview without core
//! ever naming a vendor.
//!
//! This crate is both a **library** of `Previewer` engines and the **binary**
//! (`catalerum-preview-service`, `src/bin/service.rs`) that serves them over an
//! HTTP `/render` endpoint. It ships as its own slim container image (the
//! LibreOffice + poppler toolchain plus the binary); the distroless API talks to
//! it over the network via a thin `HttpPreviewer` client (in `catalerum-api`),
//! keeping no render toolchain of its own.
//!
//! Engines:
//! - [`ImagePreviewer`] — pure-Rust `image`-crate decode → resize → encode for
//!   the common raster formats (png/jpeg/gif/webp/bmp/tiff/ico/qoi/…).
//! - [`DocumentPreviewer`] — the document engine: renders PDFs, office
//!   documents, spreadsheets, presentations, and SVG by shelling the local
//!   `pdftoppm`/`pdfinfo`/`soffice` binaries (office → PDF via LibreOffice, then
//!   the PDF page → a raster via poppler), re-encoded through the image engine.
//! - [`PreviewChain`] — an ordered chain serving each request from the first
//!   engine that supports its content type and succeeds.

#![forbid(unsafe_code)]

mod chain;
mod document;
mod image;

pub use chain::PreviewChain;
pub use document::DocumentPreviewer;
pub use image::ImagePreviewer;
