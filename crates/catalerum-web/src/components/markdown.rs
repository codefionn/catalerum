//! The web UI's Markdown→HTML entry point (SOUL §21/§12), used by the Notes
//! live-preview, the Chat panel (finalized assistant replies), tool-result cards,
//! Skills, tasks and emerged UIs.
//!
//! The engine is the shared [`catalerum_markdown`] crate, but it runs in a Web
//! Worker ([`crate::md_worker`]) so a long reply or a mermaid diagram never
//! blocks input or paint. Rendering is therefore asynchronous. Each helper here
//! returns a reactive closure for `inner_html`: a cached result renders at once,
//! otherwise it shows a plain-text placeholder (or the previous HTML) and
//! re-runs when the worker answers. **Untrusted-input safe:** every text node is
//! HTML-escaped and only vetted URL schemes become links, so the result is sound
//! for `inner_html`; see [`catalerum_markdown`] for the guarantees.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;

use leptos::prelude::*;

use crate::md_worker;

/// Per-view render state for worker-rendered Markdown. Reading [`html`](Self::html)
/// inside a reactive closure subscribes it, so the closure re-runs once the
/// worker delivers.
#[derive(Clone, Copy)]
pub(crate) struct LiveMarkdown {
    ready: Trigger,
    /// The last HTML this view received, keyed by a hash of its Markdown. Covers
    /// a cache eviction between delivery and re-run, and lets reactive sources
    /// keep showing the previous render while the next is in flight.
    last: StoredValue<Option<(u64, Rc<str>)>, LocalStorage>,
}

impl LiveMarkdown {
    pub(crate) fn new() -> Self {
        Self { ready: Trigger::new(), last: StoredValue::new_local(None) }
    }

    /// The HTML for `md`, or `None` while the worker renders it.
    pub(crate) fn html(&self, md: &str) -> Option<String> {
        self.ready.track();
        let key = hash(md);
        let (ready, last) = (self.ready, self.last);
        // Checked before the cache so a delivered result that didn't fit the
        // cache isn't requested again (and again).
        let delivered = last.with_value(|l| {
            l.as_ref().filter(|(k, _)| *k == key).map(|(_, h)| h.to_string())
        });
        if delivered.is_some() {
            return delivered;
        }
        let html = md_worker::html_or_request(md, move |html| {
            if last.try_set_value(Some((key, html))).is_none() {
                ready.notify();
            }
        });
        let html = html?;
        let out = html.to_string();
        last.set_value(Some((key, html)));
        Some(out)
    }

    /// The last HTML received for any Markdown, if any.
    pub(crate) fn previous(&self) -> Option<String> {
        self.last.with_value(|l| l.as_ref().map(|(_, h)| h.to_string()))
    }
}

fn hash(md: &str) -> u64 {
    let mut h = DefaultHasher::new();
    md.hash(&mut h);
    h.finish()
}

/// Escape `text` for use as HTML text content.
pub(crate) fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What shows until the worker's HTML arrives: the Markdown source as plain,
/// line-wrapped text, so the layout is close to final and nothing is blank.
pub(crate) fn pending_html(md: &str) -> String {
    format!("<div class=\"md-pending\">{}</div>", escape_html(md))
}

/// Render a fixed Markdown string. Empty input renders as empty HTML: display
/// surfaces such as chat messages and tool cards must not inherit the editor
/// preview's empty-state copy.
pub(crate) fn markdown_html(md: impl Into<String>) -> impl Fn() -> String + Send + Sync + 'static {
    let md = md.into();
    let live = LiveMarkdown::new();
    move || live.html(&md).unwrap_or_else(|| pending_html(&md))
}

/// Render a reactive Markdown source. While a new render is in flight the
/// previous HTML stays up, so typing in an editor doesn't flicker.
pub(crate) fn reactive_markdown_html(
    src: impl Fn() -> String + Send + Sync + 'static,
) -> impl Fn() -> String + Send + Sync + 'static {
    let live = LiveMarkdown::new();
    move || {
        let md = src();
        live.html(&md)
            .or_else(|| live.previous())
            .unwrap_or_else(|| pending_html(&md))
    }
}

const PREVIEW_EMPTY: &str = "<p class=\"notes-preview-empty\">Nothing to preview yet.</p>";

/// [`reactive_markdown_html`] for editor previews: blank input shows the
/// empty-state placeholder instead.
pub(crate) fn markdown_preview_html(
    src: impl Fn() -> String + Send + Sync + 'static,
) -> impl Fn() -> String + Send + Sync + 'static {
    let src = std::sync::Arc::new(src);
    let blank = {
        let src = src.clone();
        move || src().trim().is_empty()
    };
    let render = reactive_markdown_html(move || src());
    move || if blank() { PREVIEW_EMPTY.to_string() } else { render() }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Natively the worker client renders in-thread, so results are synchronous.

    #[test]
    fn empty_input_shows_placeholder() {
        Owner::new().with(|| {
            assert!(markdown_preview_html(|| "   \n".to_string())().contains("Nothing to preview yet."));
        });
    }

    #[test]
    fn empty_display_markdown_has_no_editor_placeholder() {
        Owner::new().with(|| {
            let html = markdown_html("   \n")();
            assert!(html.trim().is_empty());
            assert!(!html.contains("Nothing to preview yet."));
        });
    }

    #[test]
    fn delegates_to_the_engine_safely() {
        Owner::new().with(|| {
            let html = markdown_preview_html(|| "# Title\n\n- **milk**\n- `eggs`\n\n> quote".to_string())();
            assert!(html.contains("<h1 id=\"title\">Title</h1>"));
            assert!(html.contains("<li><strong>milk</strong></li>"));
            assert!(html.contains("<li><code>eggs</code></li>"));
            assert!(html.contains("<blockquote><p>quote</p></blockquote>"));
            // Injection stays escaped.
            let xss = markdown_html("<script>alert(1)</script> [x](javascript:alert(1))")();
            assert!(!xss.contains("<script>"));
            assert!(!xss.contains("javascript:"));
        });
    }

    #[test]
    fn pending_placeholder_escapes_source() {
        assert_eq!(pending_html("<b>&"), "<div class=\"md-pending\">&lt;b&gt;&amp;</div>");
    }
}
