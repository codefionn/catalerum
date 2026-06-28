//! HTML escaping and URL safety — the security boundary.
//!
//! Everything the renderers emit as text passes through here, so injecting the
//! result via `inner_html` is sound even for fully untrusted Markdown. We never
//! pass raw inline HTML through (a stray `<script>` is escaped to text, see the
//! parser), and only vetted URL schemes survive [`is_safe_url`] — a
//! `javascript:` destination is dropped and the link renders as plain text.
//!
//! Escaping copies *clean runs* in bulk: [`crate::scan::ByteSet::find`] locates
//! the next metacharacter with SIMD, we `push_str` everything up to it in one go,
//! then emit the entity. Long ordinary text costs almost nothing.

use crate::scan::ByteSet;

/// Text-context metacharacters: `&`, `<`, `>`. (`"`/`'` are only special inside
/// attribute values, handled by [`escape_href`].)
static TEXT_SPECIAL: ByteSet = ByteSet::new(b"&<>");
/// Attribute-context metacharacters: adds `"` so a destination cannot break out
/// of `href="…"`.
static ATTR_SPECIAL: ByteSet = ByteSet::new(b"&<>\"");

/// Append `s` to `out`, escaping `&<>` so it is safe as element text.
pub(crate) fn escape_text(out: &mut String, s: &str) {
    escape_with(out, s, &TEXT_SPECIAL);
}

/// Append `s` to `out`, escaping `&<>"` so it is safe inside a double-quoted
/// attribute value.
pub(crate) fn escape_attr(out: &mut String, s: &str) {
    escape_with(out, s, &ATTR_SPECIAL);
}

fn escape_with(out: &mut String, s: &str, set: &ByteSet) {
    let bytes = s.as_bytes();
    let mut start = 0usize;
    // Reserve once for the common case (a little growth for entities is fine).
    out.reserve(s.len());
    while let Some(rel) = set.find(&bytes[start..]) {
        let at = start + rel;
        // `at` is an ASCII metacharacter, hence a char boundary: this slice is safe.
        out.push_str(&s[start..at]);
        out.push_str(match bytes[at] {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            _ => unreachable!("escape set only contains &<>\""),
        });
        start = at + 1;
    }
    out.push_str(&s[start..]);
}

/// Whether `url` is safe to emit as an `href`/`src`. Mirrors the workbench's prior
/// policy (SOUL §21/§12): only `http(s)`, `mailto:`, root-relative (`/…`) and
/// in-page anchors (`#…`) are allowed; everything else (notably `javascript:`,
/// `data:`, `vbscript:`) is rejected so the caller renders the label as text.
///
/// The scheme check is case-insensitive and tolerant of leading control/space
/// bytes, which is how `javascript:` filter bypasses are usually smuggled.
pub(crate) fn is_safe_url(url: &str) -> bool {
    let trimmed = url.trim_matches(|c: char| c.is_whitespace() || c.is_control());
    if trimmed.starts_with('/') || trimmed.starts_with('#') {
        return true;
    }
    // A scheme is `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`. If there is no
    // scheme before the first `/`, `?`, `#`, it is a relative reference → allowed.
    let scheme_end = trimmed.find(':');
    let Some(end) = scheme_end else {
        return true; // no colon ⇒ relative path/fragment
    };
    // A colon that appears after a `/`, `?` or `#` is part of the path, not a
    // scheme (e.g. `foo/bar:baz`), so this is still a relative reference.
    let before = &trimmed[..end];
    if before.contains('/') || before.contains('?') || before.contains('#') {
        return true;
    }
    let scheme = before.to_ascii_lowercase();
    matches!(scheme.as_str(), "http" | "https" | "mailto" | "tel")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esc(s: &str) -> String {
        let mut out = String::new();
        escape_text(&mut out, s);
        out
    }

    #[test]
    fn escapes_text_metacharacters_only() {
        assert_eq!(esc("a & b < c > d"), "a &amp; b &lt; c &gt; d");
        // Quotes and apostrophes are literal in text context.
        assert_eq!(esc("say \"hi\" it's fine"), "say \"hi\" it's fine");
        // Long clean run then a single metachar (exercises the SIMD bulk copy).
        let clean = "x".repeat(100);
        assert_eq!(esc(&format!("{clean}<")), format!("{clean}&lt;"));
    }

    #[test]
    fn escapes_attribute_quotes() {
        let mut out = String::new();
        escape_attr(&mut out, "a\"b&c");
        assert_eq!(out, "a&quot;b&amp;c");
    }

    #[test]
    fn rejects_dangerous_url_schemes() {
        for bad in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "  javascript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>",
            "vbscript:msgbox",
        ] {
            assert!(!is_safe_url(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn allows_safe_urls() {
        for ok in [
            "https://example.com",
            "http://example.com/a?b=1&c=2",
            "mailto:a@b.com",
            "tel:+123",
            "/root/relative",
            "#anchor",
            "relative/path",
            "./rel",
            "page.html",
        ] {
            assert!(is_safe_url(ok), "{ok:?} must be allowed");
        }
    }
}
