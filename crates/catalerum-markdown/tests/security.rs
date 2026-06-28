//! Injection-safety: the HTML output must be sound to drop into `inner_html` for
//! fully untrusted Markdown. Raw HTML is escaped, and only vetted URL schemes
//! survive on links/images.

use catalerum_markdown::to_html;

#[test]
fn raw_html_is_escaped_not_passed_through() {
    let html = to_html("<script>alert(1)</script>");
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
}

#[test]
fn raw_html_in_code_is_escaped() {
    let html = to_html("`<img onerror=x>`");
    assert!(
        html.contains("<code>&lt;img onerror=x&gt;</code>"),
        "{html}"
    );
    assert!(!html.contains("<img"), "{html}");
}

#[test]
fn javascript_link_scheme_dropped() {
    let html = to_html("[click](javascript:alert(1))");
    assert!(!html.contains("<a "), "{html}");
    assert!(!html.contains("javascript:"), "{html}");
    assert!(html.contains("click"), "{html}");
}

#[test]
fn data_and_vbscript_schemes_dropped() {
    for md in [
        "[x](data:text/html,<script>alert(1)</script>)",
        "[x](vbscript:msgbox)",
        "![x](javascript:alert(1))",
    ] {
        let html = to_html(md);
        assert!(!html.contains("<a "), "{md}: {html}");
        assert!(!html.contains("<img"), "{md}: {html}");
        assert!(!html.contains("javascript:"), "{md}: {html}");
        assert!(!html.contains("vbscript:"), "{md}: {html}");
    }
}

#[test]
fn autolink_with_dangerous_scheme_renders_as_text() {
    let html = to_html("<javascript:alert(1)>");
    assert!(!html.contains("<a "), "{html}");
    assert!(!html.contains("href"), "{html}");
}

#[test]
fn attribute_breakout_is_escaped() {
    // A quote/`>` in the destination must not break out of `href="…"`.
    let html = to_html("[x](https://e.com/\"onmouseover=alert(1))");
    assert!(html.contains("&quot;"), "{html}");
    assert!(!html.contains("\"onmouseover"), "{html}");
}

#[test]
fn title_breakout_is_escaped() {
    let html = to_html("[x](https://e.com \"a\\\"><script>\")");
    assert!(!html.contains("<script>"), "{html}");
}

#[test]
fn image_src_unsafe_scheme_dropped_keeps_alt() {
    let html = to_html("![the alt](javascript:alert(1))");
    assert!(!html.contains("<img"), "{html}");
    assert!(html.contains("the alt"), "{html}");
}

#[test]
fn url_filter_is_not_fooled_by_whitespace_or_case() {
    // Leading/trailing whitespace, control chars, and case must not smuggle a
    // dangerous scheme past `is_safe_url` (both inline links and autolinks).
    // No anchor (hence no dangerous `href`) may be emitted. For autolinks the
    // rejected URL survives as *inert text* (the label is the URL itself), which is
    // harmless — so the property is "no `<a`/`href`", not "scheme absent from text".
    for md in [
        "[x]( javascript:alert(1))",
        "[x](\tJaVaScRiPt:alert(1))",
        "[x](javascript:alert(1)  )",
        "<JAVASCRIPT:alert(1)>",
        "<vbscript:x>",
    ] {
        let html = to_html(md);
        assert!(!html.contains("<a "), "{md:?} -> {html}");
        assert!(!html.contains("href"), "{md:?} -> {html}");
    }
}

#[test]
fn percent_encoded_scheme_is_a_harmless_relative_link() {
    // `%6a%61...%3a` is percent-encoded `javascript:`. With the colon encoded there
    // is no scheme, so it is treated as a *relative* reference — a browser navigates
    // to that path (harmless 404), it does not execute. No literal `javascript:`.
    let html = to_html("[click](%6a%61%76%61%73%63%72%69%70%74%3aalert%281%29)");
    assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
}

#[test]
fn reference_definition_with_unsafe_scheme_is_dropped() {
    let html = to_html("[link][ref]\n\n[ref]: javascript:alert(1)");
    assert!(!html.contains("<a "), "{html}");
    assert!(!html.contains("javascript:"), "{html}");
    assert!(html.contains("link"), "{html}");
}

#[test]
fn code_blocks_escape_all_content() {
    // Fenced, fenced-with-language, and indented code all escape `<>&`.
    for md in [
        "```\n<script>alert(1)</script>\n```",
        "```js\n<img src=x onerror=alert(1)>\n```",
        "    <script>alert(1)</script>",
    ] {
        let html = to_html(md);
        assert!(!html.contains("<script"), "{md:?} -> {html}");
        assert!(!html.contains("<img "), "{md:?} -> {html}");
        assert!(html.contains("&lt;"), "{md:?} -> {html}");
    }
}

#[test]
fn fenced_language_cannot_inject_into_class_attribute() {
    // A crafted info string must not break out of `class="language-…"`.
    let html = to_html("```foo\" onclick=\"alert(1)\ncode\n```");
    assert!(!html.contains("\" onclick"), "{html}");
    assert!(
        html.contains("&quot;") || !html.contains("onclick"),
        "{html}"
    );
}

#[test]
fn mermaid_labels_cannot_inject() {
    // A node label containing markup is escaped inside the generated SVG.
    let html = to_html("```mermaid\ngraph TD\n A[\"<img src=x onerror=alert(1)>\"] --> B\n```");
    assert!(!html.contains("<img"), "{html}");
    assert!(html.contains("&lt;img"), "{html}");
}

#[test]
fn math_content_is_escaped() {
    // LaTeX text/operators that look like markup are escaped inside MathML.
    let inline = to_html("$\\text{<script>alert(1)</script>}$");
    assert!(!inline.contains("<script>"), "{inline}");
    assert!(inline.contains("&lt;script&gt;"), "{inline}");
    let cmp = to_html("$a < b$");
    assert!(!cmp.contains("<script"), "{cmp}");
    assert!(cmp.contains("&lt;"), "{cmp}");
}
