//! [`ImagePreviewer`] — the in-process image engine (SOUL §9/§10): decode a
//! raster image with the pure-Rust `image` crate, resize it to fit the request's
//! bound (never upscaling), and re-encode it. Runs on the API host itself (no
//! sandbox), so image thumbnails work even without an exec backend configured.
//!
//! The codec work is CPU-bound, so it runs on a blocking thread to keep the
//! async runtime free.

// `image` here is the extern crate; the module is also named `image`, so the
// crate is always referenced through the leading `::`.
use std::io::Cursor;

use async_trait::async_trait;
use tracing::debug;

use catalerum_core::error::{Error, Result};
use catalerum_core::preview::{is_image_type, PreviewFormat, PreviewRequest, PreviewResponse};
use catalerum_core::provider::Previewer;

/// The pure-Rust, in-process image thumbnailer.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImagePreviewer;

impl ImagePreviewer {
    /// A new image engine (stateless).
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Decode, resize (never upscale), and encode `document` into `format`, fitting
/// the longest side within `max_dimension`. Returns the encoded bytes, the
/// **actual** format used (WebP falls back to PNG if this `image` build cannot
/// encode WebP), and the output dimensions. Pure/blocking — call under
/// `spawn_blocking`. Shared with the document engine, which feeds it the poppler
/// page raster to re-encode into the requested format.
pub(crate) fn render(
    document: &[u8],
    max_dimension: u32,
    format: PreviewFormat,
) -> Result<(Vec<u8>, PreviewFormat, u32, u32)> {
    let img = ::image::load_from_memory(document)
        .map_err(|e| Error::Invalid(format!("unreadable image: {e}")))?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let bound = max_dimension.max(1);
    // Only ever shrink: compute a proportional target when the longest side
    // exceeds the bound, otherwise keep the source resolution.
    let longest = w.max(h);
    let resized = if longest > bound {
        let scale = f64::from(bound) / f64::from(longest);
        let nw = ((f64::from(w) * scale).round() as u32).max(1);
        let nh = ((f64::from(h) * scale).round() as u32).max(1);
        img.resize(nw, nh, ::image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let (ow, oh) = (resized.width(), resized.height());
    let (bytes, used) = encode(&resized, format)?;
    Ok((bytes, used, ow, oh))
}

/// Encode `img` into `format`. JPEG flattens away any alpha channel; WebP falls
/// back to PNG when the linked `image` build lacks a WebP encoder (so callers
/// still get a valid image rather than an error).
fn encode(img: &::image::DynamicImage, format: PreviewFormat) -> Result<(Vec<u8>, PreviewFormat)> {
    let write = |image: &::image::DynamicImage, fmt: ::image::ImageFormat| -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut buf), fmt)
            .map_err(|e| Error::Provider(format!("image encode failed: {e}")))?;
        Ok(buf)
    };
    match format {
        PreviewFormat::Png => Ok((write(img, ::image::ImageFormat::Png)?, PreviewFormat::Png)),
        PreviewFormat::Jpeg => {
            // JPEG has no alpha channel — flatten to RGB first.
            let rgb = ::image::DynamicImage::ImageRgb8(img.to_rgb8());
            Ok((
                write(&rgb, ::image::ImageFormat::Jpeg)?,
                PreviewFormat::Jpeg,
            ))
        }
        PreviewFormat::Webp => {
            // Try WebP; a build without a WebP encoder degrades to PNG so the
            // caller still gets a valid image rather than an error.
            let mut buf = Vec::new();
            match img.write_to(&mut Cursor::new(&mut buf), ::image::ImageFormat::WebP) {
                Ok(()) => Ok((buf, PreviewFormat::Webp)),
                Err(_) => Ok((write(img, ::image::ImageFormat::Png)?, PreviewFormat::Png)),
            }
        }
    }
}

#[async_trait]
impl Previewer for ImagePreviewer {
    fn name(&self) -> &'static str {
        "image"
    }

    fn supports(&self, content_type: &str) -> bool {
        is_image_type(content_type)
    }

    async fn preview(&self, request: PreviewRequest) -> Result<PreviewResponse> {
        let PreviewRequest {
            document,
            max_dimension,
            format,
            ..
        } = request;
        // Codec work is CPU-bound: run it off the async runtime.
        let (bytes, used, width, height) =
            tokio::task::spawn_blocking(move || render(&document, max_dimension, format))
                .await
                .map_err(|e| Error::other(format!("preview task join: {e}")))??;
        debug!(
            width,
            height,
            format = used.extension(),
            "image preview rendered"
        );
        Ok(PreviewResponse {
            image: bytes,
            content_type: used.content_type().to_string(),
            width,
            height,
            page_count: 1,
            engine: self.name().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-colour PNG of `w`×`h` for round-trip tests.
    fn png(w: u32, h: u32) -> Vec<u8> {
        let buf = ::image::RgbaImage::from_pixel(w, h, ::image::Rgba([10, 120, 200, 255]));
        let mut out = Vec::new();
        ::image::DynamicImage::ImageRgba8(buf)
            .write_to(&mut Cursor::new(&mut out), ::image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn supports_a_curated_raster_set_only() {
        let e = ImagePreviewer::new();
        assert!(e.supports("image/png"));
        assert!(e.supports("image/JPEG; foo=bar"));
        assert!(e.supports("image/webp"));
        assert!(e.supports("image/tiff"));
        // Vector + documents route to the sandbox engine, not here.
        assert!(!e.supports("image/svg+xml"));
        assert!(!e.supports("application/pdf"));
        assert!(!e.supports("image/heic"));
    }

    #[tokio::test]
    async fn downscales_a_large_image_to_the_bound_without_upscaling() {
        let e = ImagePreviewer::new();
        // 400×200 → bound 100 → longest side becomes 100, aspect preserved.
        let resp = e
            .preview(
                PreviewRequest::new(png(400, 200), "image/png")
                    .with_max_dimension(100)
                    .with_format(PreviewFormat::Png),
            )
            .await
            .unwrap();
        assert_eq!(resp.width, 100);
        assert_eq!(resp.height, 50);
        assert_eq!(resp.page_count, 1);
        assert_eq!(resp.engine, "image");
        // Decodes back to the reported dimensions.
        let back = ::image::load_from_memory(&resp.image).unwrap();
        assert_eq!((back.width(), back.height()), (100, 50));
    }

    #[tokio::test]
    async fn never_upscales_a_small_image() {
        let e = ImagePreviewer::new();
        let resp = e
            .preview(PreviewRequest::new(png(32, 24), "image/png").with_max_dimension(1024))
            .await
            .unwrap();
        // Source is already within the bound → dimensions unchanged.
        assert_eq!((resp.width, resp.height), (32, 24));
    }

    #[tokio::test]
    async fn encodes_each_requested_format() {
        let e = ImagePreviewer::new();
        for (fmt, magic) in [
            (PreviewFormat::Png, &b"\x89PNG"[..]),
            (PreviewFormat::Jpeg, &b"\xff\xd8"[..]),
        ] {
            let resp = e
                .preview(PreviewRequest::new(png(64, 64), "image/png").with_format(fmt))
                .await
                .unwrap();
            assert!(
                resp.image.starts_with(magic),
                "format {fmt:?} produced wrong magic bytes"
            );
        }
        // WebP: either a real WebP (RIFF….WEBP) or a PNG fallback — both valid.
        let resp = e
            .preview(PreviewRequest::new(png(64, 64), "image/png").with_format(PreviewFormat::Webp))
            .await
            .unwrap();
        let is_webp = resp.image.starts_with(b"RIFF") && resp.image[8..12] == *b"WEBP";
        let is_png = resp.image.starts_with(b"\x89PNG");
        assert!(
            is_webp || is_png,
            "webp preview should be webp or png-fallback"
        );
        let expected_ct = if is_webp { "image/webp" } else { "image/png" };
        assert_eq!(resp.content_type, expected_ct);
    }

    #[tokio::test]
    async fn rejects_undecodable_bytes() {
        let e = ImagePreviewer::new();
        let err = e
            .preview(PreviewRequest::new(vec![0, 1, 2, 3, 4], "image/png"))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "got: {err:?}");
    }
}
