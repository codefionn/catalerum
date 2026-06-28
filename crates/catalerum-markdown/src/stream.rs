//! Streaming / incremental rendering.
//!
//! LLM replies arrive token by token. Rendering the *whole* buffer as Markdown on
//! every delta is wasteful and shows half-open constructs (a dangling `**`, an
//! unclosed fence) as broken markup. [`StreamRenderer`] instead renders only the
//! part of the buffer that can no longer change — the prefix up to the last block
//! boundary outside a code fence — and hands back the unstable tail for the caller
//! to show as plain text (with a cursor). Each delta does work proportional to the
//! *new* stable text, not the whole buffer.
//!
//! The boundary is a blank line that is not inside a fenced code block — a
//! CommonMark block separator. Everything before it is a whole number of blocks
//! and parses identically in isolation. The approximation: a reference link
//! defined *after* the boundary will not resolve in already-committed text, and a
//! loose list split across the boundary renders as two lists. Those resolve
//! correctly once the caller does a final full render ([`StreamRenderer::finish`]
//! or a plain [`crate::to_html`]) on the completed text.

use crate::parser::block::{is_closing_fence, open_fence};

/// Byte index up to which `full` is stable: parsing `&full[..stable_boundary(full)]`
/// will not change as more text is appended. It is the position just after the
/// last blank line that lies outside any open code fence (`0` if there is none).
pub fn stable_boundary(full: &str) -> usize {
    scan_boundary(full, 0)
}

/// [`stable_boundary`] resumed from `from`, which **must** be a line start that lies
/// outside any open code fence — every committed boundary is, by construction. This
/// lets [`StreamRenderer::update`] scan only the newly-arrived tail each delta
/// instead of re-scanning the whole buffer, so boundary detection across a stream
/// is O(total) rather than O(n²). The result is identical to scanning from 0,
/// because the prefix `[..from]` is whole blocks ending outside a fence and so
/// contributes no boundary beyond `from`.
fn scan_boundary(full: &str, from: usize) -> usize {
    let mut in_fence: Option<(u8, usize)> = None;
    let mut last_safe = from;
    let mut line_start = from;
    let bytes = full.as_bytes();
    let mut i = from;
    loop {
        let at_end = i == bytes.len();
        if at_end || bytes[i] == b'\n' {
            let raw = &full[line_start..i];
            let line = raw.strip_suffix('\r').unwrap_or(raw);
            let t = line.trim_start();
            if let Some((ch, len)) = in_fence {
                if is_closing_fence(t, ch, len) {
                    in_fence = None;
                }
            } else if let Some(fence) = open_fence(t) {
                in_fence = Some(fence);
            } else if line.trim().is_empty() && !at_end {
                // Blank line outside a fence — commit through its newline.
                last_safe = i + 1;
            }
            if at_end {
                break;
            }
            line_start = i + 1;
        }
        i += 1;
    }
    last_safe
}

/// Incremental HTML renderer for a growing Markdown buffer (e.g. a streaming chat
/// reply). Keeps an internal HTML buffer of already-stable blocks.
#[derive(Default)]
pub struct StreamRenderer {
    html: String,
    committed: usize,
}

impl StreamRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the full text received so far. Appends any newly-stable HTML to the
    /// internal buffer and returns the still-unstable tail to render as plain text.
    ///
    /// `full` is expected to grow between calls; if it ever *shrinks* below what
    /// was already committed (e.g. the buffer was reset), the committed mark is
    /// pulled back to the new end so slicing/`clamp` stay sound.
    pub fn update<'a>(&mut self, full: &'a str) -> &'a str {
        self.committed = self.committed.min(full.len());
        // Resume the boundary scan from the committed mark (a line start outside any
        // fence) so each delta only scans the new tail, not the whole buffer.
        let boundary = scan_boundary(full, self.committed).clamp(self.committed, full.len());
        if boundary > self.committed {
            crate::render::html::push_html(&mut self.html, &full[self.committed..boundary]);
            self.committed = boundary;
        }
        &full[self.committed..]
    }

    /// Commit the remaining tail once the stream is complete.
    pub fn finish(&mut self, full: &str) {
        if full.len() > self.committed {
            crate::render::html::push_html(&mut self.html, &full[self.committed..]);
            self.committed = full.len();
        }
    }

    /// The HTML rendered for the stable (committed) prefix so far.
    pub fn html(&self) -> &str {
        &self.html
    }

    /// Consume the renderer, returning its accumulated HTML.
    pub fn into_html(self) -> String {
        self.html
    }
}
