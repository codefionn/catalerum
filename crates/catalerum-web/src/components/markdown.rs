//! The web UI's Markdown→HTML entry point (SOUL §21/§12), used by the Notes
//! live-preview, the Chat panel (finalized assistant replies), tool-result cards,
//! Skills, and emerged UIs.
//!
//! The actual engine lives in the shared [`catalerum_markdown`] crate — a
//! streaming, SIMD-accelerated CommonMark+GFM parser with HTML and Leptos
//! renderers, mermaid support, and its own correctness/security/allocation test
//! suites. This module is a thin adapter that adds the empty-state hint only for
//! editor previews. **Untrusted-input safe:** every text node is HTML-escaped and
//! only vetted URL schemes become links, so the result is sound for `inner_html`;
//! see [`catalerum_markdown`] for the guarantees. For new code that wants real
//! DOM nodes (no `inner_html`), prefer [`catalerum_markdown::render_markdown`].

/// Render `markdown` to safe HTML.
///
/// Empty input deliberately renders as empty HTML. Display surfaces such as chat
/// messages and tool cards must not inherit the editor preview's empty-state
/// copy.
pub(crate) fn markdown_html(markdown: &str) -> String {
    catalerum_markdown::to_html(markdown)
}

/// Render `markdown` to safe HTML, or the editors' empty-state placeholder when
/// there is nothing to preview.
pub(crate) fn markdown_preview_html(markdown: &str) -> String {
    if markdown.trim().is_empty() {
        return "<p class=\"notes-preview-empty\">Nothing to preview yet.</p>".to_string();
    }
    markdown_html(markdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_shows_placeholder() {
        assert!(markdown_preview_html("   \n").contains("Nothing to preview yet."));
    }

    #[test]
    fn empty_display_markdown_has_no_editor_placeholder() {
        let html = markdown_html("   \n");
        assert!(html.trim().is_empty());
        assert!(!html.contains("Nothing to preview yet."));
    }

    #[test]
    fn delegates_to_the_engine_safely() {
        let html = markdown_preview_html("# Title\n\n- **milk**\n- `eggs`\n\n> quote");
        assert!(html.contains("<h1 id=\"title\">Title</h1>"));
        assert!(html.contains("<li><strong>milk</strong></li>"));
        assert!(html.contains("<li><code>eggs</code></li>"));
        assert!(html.contains("<blockquote><p>quote</p></blockquote>"));
        // Injection stays escaped.
        let xss = markdown_preview_html("<script>alert(1)</script> [x](javascript:alert(1))");
        assert!(!xss.contains("<script>"));
        assert!(!xss.contains("javascript:"));
    }
}
