//! End-to-end `Markdown → HTML` correctness across the supported block + inline
//! grammar and the GFM extensions. The renderer is injection-safe, so these also
//! pin the escaping behaviour.

use catalerum_markdown::to_html;

#[test]
fn headings_atx_and_setext() {
    // Headings carry a GitHub-style anchor id derived from their text.
    assert_eq!(to_html("# Title"), "<h1 id=\"title\">Title</h1>");
    assert_eq!(to_html("### Three ###"), "<h3 id=\"three\">Three</h3>");
    assert_eq!(to_html("####### seven"), "<p>####### seven</p>");
    assert_eq!(to_html("Title\n====="), "<h1 id=\"title\">Title</h1>");
    assert_eq!(to_html("Sub\n---"), "<h2 id=\"sub\">Sub</h2>");
}

#[test]
fn heading_anchor_slugs() {
    // Multi-word: lowercased, spaces → single `-`.
    assert_eq!(
        to_html("## Getting Started"),
        "<h2 id=\"getting-started\">Getting Started</h2>"
    );
    // Punctuation dropped; `_` kept; trailing punctuation leaves no dangling dash.
    assert_eq!(
        to_html("## What's new_today?"),
        "<h2 id=\"whats-new_today\">What's new_today?</h2>"
    );
    // Inline markup contributes only its text to the slug.
    assert_eq!(
        to_html("# A `code` and **bold**"),
        "<h1 id=\"a-code-and-bold\">A <code>code</code> and <strong>bold</strong></h1>"
    );
    // Unicode letters survive (lowercased); a symbol-only heading gets no id.
    assert!(
        to_html("# Café").contains("id=\"café\">"),
        "{}",
        to_html("# Café")
    );
    assert_eq!(to_html("# +++"), "<h1>+++</h1>");
    // The id is attribute-safe even when the heading text isn't.
    let html = to_html("# a<b>&\"c");
    assert!(html.starts_with("<h1 id=\"ab"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
}

#[test]
fn paragraphs_join_soft_lines() {
    assert_eq!(to_html("a\nb"), "<p>a\nb</p>");
    assert_eq!(to_html("a\n\nb"), "<p>a</p><p>b</p>");
}

#[test]
fn inline_emphasis_and_code() {
    assert_eq!(to_html("**bold**"), "<p><strong>bold</strong></p>");
    assert_eq!(to_html("*it*"), "<p><em>it</em></p>");
    assert_eq!(to_html("`x`"), "<p><code>x</code></p>");
    assert_eq!(to_html("~~no~~"), "<p><del>no</del></p>");
    // Nested + mixed delimiters resolved by the delimiter stack.
    assert_eq!(
        to_html("**a _b_ c**"),
        "<p><strong>a <em>b</em> c</strong></p>"
    );
    assert_eq!(
        to_html("*a **b** c*"),
        "<p><em>a <strong>b</strong> c</em></p>"
    );
    assert_eq!(to_html("***x***"), "<p><em><strong>x</strong></em></p>");
    // Intraword underscore does not emphasise.
    assert_eq!(to_html("a_b_c"), "<p>a_b_c</p>");
    // Unmatched delimiters survive as literal text.
    assert_eq!(to_html("a * b"), "<p>a * b</p>");
}

#[test]
fn code_span_normalisation() {
    assert_eq!(to_html("`` ` ``"), "<p><code>`</code></p>");
    assert_eq!(to_html("`a   b`"), "<p><code>a   b</code></p>");
}

#[test]
fn fenced_code_with_language_class() {
    let html = to_html("```rust\nfn main() {}\n```");
    assert_eq!(
        html,
        "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>"
    );
    // Tilde fences work too, and the body is escaped.
    let html = to_html("~~~\n<b> & 'x'\n~~~");
    assert_eq!(html, "<pre><code>&lt;b&gt; &amp; 'x'\n</code></pre>");
}

#[test]
fn indented_code_block() {
    assert_eq!(
        to_html("    let x = 1;\n    let y = 2;"),
        "<pre><code>let x = 1;\nlet y = 2;\n</code></pre>"
    );
}

#[test]
fn mermaid_fence_renders_inline_svg() {
    let html = to_html("```mermaid\ngraph TD\n  A[Start] --> B[End]\n```");
    assert!(
        html.contains("<figure class=\"catalerum-mermaid\">"),
        "{html}"
    );
    assert!(html.contains("<svg"), "{html}");
    assert!(
        html.contains(">Start</text>") && html.contains(">End</text>"),
        "{html}"
    );
    assert!(html.contains("marker-end"), "{html}");
    // An unsupported diagram type falls back to the raw source in a <pre>.
    let bad = to_html("```mermaid\npacket-beta\n  title Packet\n  0-7: Flags\n```");
    assert!(bad.contains("<pre class=\"mermaid\">"), "{bad}");
}

#[test]
fn math_renders_to_mathml() {
    // Inline `$…$`, display `$$…$$`, and a ```math fence all produce MathML.
    let inline = to_html("Euler: $e^{i\\pi}+1=0$.");
    assert!(inline.contains("<math"), "{inline}");
    assert!(inline.contains("<msup>"), "{inline}");
    let display = to_html("$$\\frac{a}{b}$$");
    assert!(display.contains("display=\"block\""), "{display}");
    assert!(display.contains("<mfrac>"), "{display}");
    let fence = to_html("```math\n\\sqrt{x}\n```");
    assert!(fence.contains("<msqrt>"), "{fence}");
    // A block-math fence is wrapped in the `catalerum-math-block` styling hook
    // (centering + overflow-x), the same wrapper the Leptos renderer emits.
    assert!(fence.contains("class=\"catalerum-math-block\""), "{fence}");
    // A bare currency `$` is not math.
    let money = to_html("it costs $5 today");
    assert!(!money.contains("<math"), "{money}");
    assert!(money.contains("$5"), "{money}");
}

#[test]
fn bullet_and_ordered_lists_tight() {
    assert_eq!(to_html("- a\n- b"), "<ul><li>a</li><li>b</li></ul>");
    assert_eq!(to_html("1. a\n2. b"), "<ol><li>a</li><li>b</li></ol>");
    assert_eq!(
        to_html("3. a\n4. b"),
        "<ol start=\"3\"><li>a</li><li>b</li></ol>"
    );
}

#[test]
fn nested_lists() {
    assert_eq!(
        to_html("- a\n  - b\n  - c\n- d"),
        "<ul><li>a<ul><li>b</li><li>c</li></ul></li><li>d</li></ul>"
    );
}

#[test]
fn loose_list_wraps_paragraphs() {
    assert_eq!(
        to_html("- a\n\n- b"),
        "<ul><li><p>a</p></li><li><p>b</p></li></ul>"
    );
}

#[test]
fn task_list_items() {
    let html = to_html("- [ ] todo\n- [x] done");
    assert!(html.contains("type=\"checkbox\" disabled> todo"), "{html}");
    assert!(
        html.contains("type=\"checkbox\" checked disabled> done"),
        "{html}"
    );
}

#[test]
fn blockquote_nested() {
    assert_eq!(to_html("> hi"), "<blockquote><p>hi</p></blockquote>");
    assert_eq!(to_html("> a\n> b"), "<blockquote><p>a\nb</p></blockquote>");
}

#[test]
fn thematic_breaks() {
    for rule in ["---", "***", "___", "- - -", "* * *"] {
        assert_eq!(to_html(rule), "<hr>", "{rule:?}");
    }
    assert_eq!(to_html("--"), "<p>--</p>");
}

#[test]
fn links_inline_and_reference() {
    assert_eq!(
        to_html("[x](https://e.com)"),
        "<p><a href=\"https://e.com\" rel=\"noopener noreferrer\" target=\"_blank\">x</a></p>"
    );
    // Reference defined after use still resolves.
    let html = to_html("see [docs]\n\n[docs]: https://e.com \"t\"");
    assert!(html.contains("href=\"https://e.com\""), "{html}");
    assert!(html.contains("title=\"t\""), "{html}");
    assert!(html.contains(">docs</a>"), "{html}");
}

#[test]
fn autolinks() {
    let html = to_html("<https://e.com>");
    assert!(html.contains("href=\"https://e.com\""), "{html}");
    let html = to_html("<a@b.com>");
    assert!(html.contains("href=\"mailto:a@b.com\""), "{html}");
}

#[test]
fn bare_autolinks_gfm_extension() {
    // A bare http(s) URL in running text becomes a link.
    let html = to_html("see https://e.com here");
    assert!(html.contains("href=\"https://e.com\""), "{html}");
    assert!(html.contains(">https://e.com</a> here"), "{html}");
    // `www.` gets an `http://` destination but keeps the shown text.
    let html = to_html("at www.rust-lang.org now");
    assert!(html.contains("href=\"http://www.rust-lang.org\""), "{html}");
    assert!(html.contains(">www.rust-lang.org</a>"), "{html}");
    // Underscores in the path are kept (the `_` is intraword, so it survives as
    // literal text and re-merges into one node before scanning).
    let html = to_html("https://en.wikipedia.org/wiki/Foo_bar");
    assert!(
        html.contains("href=\"https://en.wikipedia.org/wiki/Foo_bar\""),
        "{html}"
    );
    // Trailing punctuation and an unbalanced closing paren are excluded.
    assert!(to_html("go https://e.com.").contains("href=\"https://e.com\""));
    let html = to_html("(see https://e.com/a)");
    assert!(html.contains("href=\"https://e.com/a\""), "{html}");
    assert!(html.contains("</a>)"), "trailing paren excluded: {html}");
}

#[test]
fn bare_autolinks_do_not_over_trigger() {
    // No dot in the authority ⇒ not a domain ⇒ not linked.
    assert_eq!(
        to_html("run http://localhost:3000/x"),
        "<p>run http://localhost:3000/x</p>"
    );
    // Inside a code span the URL stays literal.
    assert!(!to_html("`https://e.com`").contains("<a "));
    // An existing markdown link is untouched (its text isn't re-autolinked).
    assert_eq!(
        to_html("[https://e.com](https://e.com)"),
        "<p><a href=\"https://e.com\" rel=\"noopener noreferrer\" target=\"_blank\">https://e.com</a></p>"
    );
    // A `<` ends the URL; the rest is escaped, not injected.
    let html = to_html("x http://evil.com/<script>");
    assert!(html.contains("href=\"http://evil.com/\""), "{html}");
    assert!(
        html.contains("&lt;script&gt;") && !html.contains("<script>"),
        "{html}"
    );
}

#[test]
fn bare_email_autolinks() {
    // A bare email becomes a `mailto:` link; `+`/`.` in the local part survive.
    let html = to_html("ping bob+tag@mail.example.org now");
    assert!(
        html.contains("href=\"mailto:bob+tag@mail.example.org\""),
        "{html}"
    );
    assert!(html.contains(">bob+tag@mail.example.org</a>"), "{html}");
    // Trailing punctuation is excluded from the address.
    assert!(to_html("write a@b.co.uk.").contains("href=\"mailto:a@b.co.uk\""));
    // A handle with no local part, and a dotless host, are NOT emails.
    assert_eq!(to_html("hi @handle there"), "<p>hi @handle there</p>");
    assert_eq!(to_html("ssh user@localhost"), "<p>ssh user@localhost</p>");
    // Inside code it stays literal; an email in emphasis still links.
    assert!(!to_html("`x@y.com`").contains("mailto:"));
    assert!(
        to_html("*a@b.com*").contains("<em><a href=\"mailto:a@b.com\""),
        "{}",
        to_html("*a@b.com*")
    );
}

#[test]
fn images() {
    let html = to_html("![alt](https://e.com/i.png \"cap\")");
    assert!(html.contains("<img src=\"https://e.com/i.png\""), "{html}");
    assert!(html.contains("alt=\"alt\""), "{html}");
    assert!(html.contains("title=\"cap\""), "{html}");
}

#[test]
fn gfm_table_with_alignment() {
    let md = "| a | b | c |\n|:--|:-:|--:|\n| 1 | 2 | 3 |";
    let html = to_html(md);
    assert!(html.starts_with("<table><thead><tr>"), "{html}");
    assert!(
        html.contains("<th style=\"text-align:left\">a</th>"),
        "{html}"
    );
    assert!(
        html.contains("<th style=\"text-align:center\">b</th>"),
        "{html}"
    );
    assert!(
        html.contains("<th style=\"text-align:right\">c</th>"),
        "{html}"
    );
    assert!(
        html.contains("<td style=\"text-align:center\">2</td>"),
        "{html}"
    );
    assert!(html.ends_with("</tbody></table>"), "{html}");
}

#[test]
fn hard_and_soft_breaks() {
    assert_eq!(to_html("a  \nb"), "<p>a<br>b</p>");
    assert_eq!(to_html("a\\\nb"), "<p>a<br>b</p>");
    assert_eq!(to_html("a\nb"), "<p>a\nb</p>");
}

#[test]
fn backslash_escapes() {
    assert_eq!(to_html("\\*not emph\\*"), "<p>*not emph*</p>");
    assert_eq!(to_html("a \\& b"), "<p>a &amp; b</p>");
}

#[test]
fn unresolved_reference_links_render_as_literal_text() {
    // An undefined reference (full/collapsed/shortcut/nested, link or image) is not
    // a link — it stays literal text and must never desync the scanner or panic.
    let cases = [
        ("[b]", "<p>[b]</p>"),
        ("[b][c]", "<p>[b][c]</p>"),
        ("![b][c]", "<p>![b][c]</p>"),
        ("[a][b][c]", "<p>[a][b][c]</p>"),
        ("[x][]", "<p>[x][]</p>"),
        ("[][]", "<p>[][]</p>"),
        ("[ref][missing] tail", "<p>[ref][missing] tail</p>"),
        ("a [b][c] d", "<p>a [b][c] d</p>"),
        ("[[x]][y]", "<p>[[x]][y]</p>"),
    ];
    for (md, want) in cases {
        assert_eq!(to_html(md), want, "input {md:?}");
    }
}
