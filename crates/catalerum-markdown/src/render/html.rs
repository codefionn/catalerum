//! `Markdown → HTML` rendering.
//!
//! Walks the block/inline tree and writes HTML into a single growing `String` —
//! one allocation that doubles as needed, no per-node buffers. Everything is
//! escaped (see [`crate::escape`]) and only [`crate::escape::is_safe_url`] URLs
//! become anchors/images, so the output is safe to inject via `inner_html` even
//! for untrusted Markdown.
//!
//! A fenced block whose info string is `mermaid` becomes `<pre class="mermaid">`,
//! which the [mermaid](https://mermaid.js.org) client script renders into a
//! diagram in place; other languages get `<code class="language-…">` for syntax
//! highlighters.

use crate::ast::{Block, Inline, ListItem};
use crate::escape::{escape_attr, escape_text, is_safe_url};
use crate::event::Alignment;
use crate::parser::block::parse_document;
use crate::parser::inline::heading_slug;

/// Render `md` to an HTML string.
pub fn to_html(md: &str) -> String {
    // Pre-size for the typical markup-expansion ratio so the buffer rarely regrows.
    let mut out = String::with_capacity(md.len() + md.len() / 2 + 16);
    push_html(&mut out, md);
    out
}

/// Render `md`, appending the HTML to `out` (no intermediate buffer).
pub fn push_html(out: &mut String, md: &str) {
    let (blocks, _refs) = parse_document(md);
    render_blocks(&blocks, out);
}

fn render_blocks(blocks: &[Block<'_>], out: &mut String) {
    for b in blocks {
        render_block(b, out);
    }
}

fn render_block(block: &Block<'_>, out: &mut String) {
    match block {
        Block::Paragraph(inlines) => {
            out.push_str("<p>");
            render_inlines(inlines, out);
            out.push_str("</p>");
        }
        Block::Heading { level, content } => {
            let i = (*level).clamp(1, 6) as usize - 1;
            out.push_str(["<h1", "<h2", "<h3", "<h4", "<h5", "<h6"][i]);
            // A GitHub-style anchor id so headings are deep-linkable / TOC targets.
            let slug = heading_slug(content);
            if !slug.is_empty() {
                out.push_str(" id=\"");
                escape_attr(out, &slug);
                out.push('"');
            }
            out.push('>');
            render_inlines(content, out);
            out.push_str(["</h1>", "</h2>", "</h3>", "</h4>", "</h5>", "</h6>"][i]);
        }
        Block::CodeBlock {
            info,
            indented,
            literal,
        } => render_code_block(info, *indented, literal, out),
        Block::Quote(blocks) => {
            out.push_str("<blockquote>");
            render_blocks(blocks, out);
            out.push_str("</blockquote>");
        }
        Block::List {
            start,
            tight,
            items,
        } => render_list(*start, *tight, items, out),
        Block::Table {
            alignments,
            head,
            rows,
        } => render_table(alignments, head, rows, out),
        Block::Rule => out.push_str("<hr>"),
    }
}

fn render_code_block(info: &str, indented: bool, literal: &str, out: &mut String) {
    let lang = if indented {
        ""
    } else {
        info.split_whitespace().next().unwrap_or("")
    };
    if lang.eq_ignore_ascii_case("mermaid") {
        // Render the diagram to inline SVG (pure Rust). On any parse error, fall
        // back to the raw source in a `<pre>` so nothing is lost.
        match crate::mermaid::to_svg(literal) {
            Ok(svg) => {
                out.push_str("<figure class=\"catalerum-mermaid\">");
                out.push_str(&svg);
                out.push_str("</figure>");
            }
            Err(_) => {
                out.push_str("<pre class=\"mermaid\">");
                escape_text(out, literal);
                out.push('\n');
                out.push_str("</pre>");
            }
        }
        return;
    }
    if crate::math::is_math_lang(lang) {
        // A ```math / ```latex fence is display (block) math → MathML, wrapped in
        // the `catalerum-math-block` styling hook (centering + `overflow-x` so a wide
        // equation scrolls instead of overflowing) — matching the Leptos renderer so
        // both output paths of the shared engine style block math identically.
        out.push_str("<div class=\"catalerum-math-block\">");
        out.push_str(&crate::math::to_mathml(literal, true));
        out.push_str("</div>");
        return;
    }
    out.push_str("<pre><code");
    if !lang.is_empty() {
        out.push_str(" class=\"language-");
        escape_attr(out, lang);
        out.push('"');
    }
    out.push('>');
    escape_text(out, literal);
    out.push('\n');
    out.push_str("</code></pre>");
}

fn render_list(start: Option<u64>, tight: bool, items: &[ListItem<'_>], out: &mut String) {
    match start {
        Some(n) if n != 1 => {
            out.push_str("<ol start=\"");
            out.push_str(&n.to_string());
            out.push_str("\">");
        }
        Some(_) => out.push_str("<ol>"),
        None => out.push_str("<ul>"),
    }
    for item in items {
        render_item(item, tight, out);
    }
    out.push_str(if start.is_some() { "</ol>" } else { "</ul>" });
}

fn render_item(item: &ListItem<'_>, tight: bool, out: &mut String) {
    out.push_str("<li>");
    if let Some(checked) = item.task {
        out.push_str(if checked {
            "<input type=\"checkbox\" checked disabled> "
        } else {
            "<input type=\"checkbox\" disabled> "
        });
    }
    for b in &item.blocks {
        // Tight list items render paragraph text without the wrapping `<p>`.
        if tight {
            if let Block::Paragraph(inlines) = b {
                render_inlines(inlines, out);
                continue;
            }
        }
        render_block(b, out);
    }
    out.push_str("</li>");
}

fn render_table(
    alignments: &[Alignment],
    head: &[Vec<Inline<'_>>],
    rows: &[Vec<Vec<Inline<'_>>>],
    out: &mut String,
) {
    out.push_str("<table><thead><tr>");
    for (i, cell) in head.iter().enumerate() {
        render_cell(
            "th",
            alignments.get(i).copied().unwrap_or(Alignment::None),
            cell,
            out,
        );
    }
    out.push_str("</tr></thead><tbody>");
    for row in rows {
        out.push_str("<tr>");
        for (i, cell) in row.iter().enumerate() {
            render_cell(
                "td",
                alignments.get(i).copied().unwrap_or(Alignment::None),
                cell,
                out,
            );
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
}

fn render_cell(tag: &str, align: Alignment, cell: &[Inline<'_>], out: &mut String) {
    out.push('<');
    out.push_str(tag);
    match align {
        Alignment::Left => out.push_str(" style=\"text-align:left\""),
        Alignment::Center => out.push_str(" style=\"text-align:center\""),
        Alignment::Right => out.push_str(" style=\"text-align:right\""),
        Alignment::None => {}
    }
    out.push('>');
    render_inlines(cell, out);
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

fn render_inlines(inlines: &[Inline<'_>], out: &mut String) {
    for n in inlines {
        render_inline(n, out);
    }
}

fn render_inline(node: &Inline<'_>, out: &mut String) {
    match node {
        Inline::Text(t) => escape_text(out, t),
        Inline::Code(t) => {
            out.push_str("<code>");
            escape_text(out, t);
            out.push_str("</code>");
        }
        Inline::Emph(c) => wrap(out, "<em>", "</em>", c),
        Inline::Strong(c) => wrap(out, "<strong>", "</strong>", c),
        Inline::Strike(c) => wrap(out, "<del>", "</del>", c),
        Inline::Link {
            dest,
            title,
            content,
        } => {
            if is_safe_url(dest) {
                out.push_str("<a href=\"");
                escape_attr(out, dest);
                out.push('"');
                if !title.is_empty() {
                    out.push_str(" title=\"");
                    escape_attr(out, title);
                    out.push('"');
                }
                out.push_str(" rel=\"noopener noreferrer\" target=\"_blank\">");
                render_inlines(content, out);
                out.push_str("</a>");
            } else {
                // Unsafe scheme (e.g. `javascript:`) → render the label as text only.
                render_inlines(content, out);
            }
        }
        Inline::Image { dest, title, alt } => {
            if is_safe_url(dest) {
                out.push_str("<img src=\"");
                escape_attr(out, dest);
                out.push_str("\" alt=\"");
                escape_attr(out, alt);
                out.push('"');
                if !title.is_empty() {
                    out.push_str(" title=\"");
                    escape_attr(out, title);
                    out.push('"');
                }
                out.push('>');
            } else {
                escape_text(out, alt);
            }
        }
        Inline::Math { content, display } => {
            out.push_str(&crate::math::to_mathml(content, *display))
        }
        Inline::SoftBreak => out.push('\n'),
        Inline::HardBreak => out.push_str("<br>"),
    }
}

fn wrap(out: &mut String, open: &str, close: &str, content: &[Inline<'_>]) {
    out.push_str(open);
    render_inlines(content, out);
    out.push_str(close);
}
