//! `Markdown → Leptos view` rendering (feature `leptos`).
//!
//! Builds real DOM nodes via the `view!` builder rather than an HTML string, so
//! the workbench renders Markdown **without `inner_html`** — there is no raw-HTML
//! sink at all, which closes the injection surface by construction (text and
//! attribute values are escaped by Leptos when it writes them to the DOM). Only
//! [`crate::escape::is_safe_url`] destinations become anchors/images.
//!
//! A `mermaid` fenced block becomes a `<pre class="mermaid">` node; mount a
//! `mermaid.run()` hook after render to turn it into a diagram.

use leptos::prelude::*;

use crate::ast::{Block, Inline, ListItem};
use crate::escape::is_safe_url;
use crate::event::Alignment;
use crate::parser::block::parse_document;
use crate::parser::inline::heading_slug;

/// Parse `md` and render it to a Leptos [`AnyView`].
pub fn render_markdown(md: &str) -> AnyView {
    let (blocks, _refs) = parse_document(md);
    let children = render_blocks(&blocks);
    view! { {children} }.into_any()
}

fn render_blocks(blocks: &[Block<'_>]) -> Vec<AnyView> {
    blocks.iter().map(render_block).collect()
}

fn render_block(block: &Block<'_>) -> AnyView {
    match block {
        Block::Paragraph(inlines) => {
            let c = render_inlines(inlines);
            view! { <p>{c}</p> }.into_any()
        }
        Block::Heading { level, content } => {
            let c = render_inlines(content);
            // A GitHub-style anchor id (omitted when empty) — deep-linkable / TOC.
            let id = {
                let s = heading_slug(content);
                (!s.is_empty()).then_some(s)
            };
            match level {
                1 => view! { <h1 id=id>{c}</h1> }.into_any(),
                2 => view! { <h2 id=id>{c}</h2> }.into_any(),
                3 => view! { <h3 id=id>{c}</h3> }.into_any(),
                4 => view! { <h4 id=id>{c}</h4> }.into_any(),
                5 => view! { <h5 id=id>{c}</h5> }.into_any(),
                _ => view! { <h6 id=id>{c}</h6> }.into_any(),
            }
        }
        Block::CodeBlock {
            info,
            indented,
            literal,
        } => render_code_block(info, *indented, literal),
        Block::Quote(blocks) => {
            let c = render_blocks(blocks);
            view! { <blockquote>{c}</blockquote> }.into_any()
        }
        Block::List {
            start,
            tight,
            items,
        } => render_list(*start, *tight, items),
        Block::Table {
            alignments,
            head,
            rows,
        } => render_table(alignments, head, rows),
        Block::Rule => view! { <hr/> }.into_any(),
    }
}

fn render_code_block(info: &str, indented: bool, literal: &str) -> AnyView {
    let lang = if indented {
        ""
    } else {
        info.split_whitespace().next().unwrap_or("")
    };
    if lang.eq_ignore_ascii_case("mermaid") {
        // Diagram → inline SVG (pure Rust). The SVG is engine-generated with its
        // text escaped, so `inner_html` of it is safe; fall back to the raw source
        // in a <pre> if it doesn't parse.
        return match crate::mermaid::to_svg(literal) {
            Ok(svg) => {
                view! { <figure class="catalerum-mermaid" inner_html=svg></figure> }.into_any()
            }
            Err(_) => view! { <pre class="mermaid">{format!("{literal}\n")}</pre> }.into_any(),
        };
    }
    if crate::math::is_math_lang(lang) {
        // Display math → MathML (engine-generated, escaped → safe to inner_html).
        let mathml = crate::math::to_mathml(literal, true);
        return view! { <div class="catalerum-math-block" inner_html=mathml></div> }.into_any();
    }
    let body = format!("{literal}\n");
    let class = (!lang.is_empty()).then(|| format!("language-{lang}"));
    view! { <pre><code class=class>{body}</code></pre> }.into_any()
}

fn render_list(start: Option<u64>, tight: bool, items: &[ListItem<'_>]) -> AnyView {
    let children: Vec<AnyView> = items.iter().map(|it| render_item(it, tight)).collect();
    match start {
        Some(n) => {
            let start_attr = (n != 1).then(|| n.to_string());
            view! { <ol start=start_attr>{children}</ol> }.into_any()
        }
        None => view! { <ul>{children}</ul> }.into_any(),
    }
}

fn render_item(item: &ListItem<'_>, tight: bool) -> AnyView {
    let mut children: Vec<AnyView> = Vec::new();
    if let Some(checked) = item.task {
        // A disabled checkbox, like the HTML renderer emits.
        children
            .push(view! { <input type="checkbox" prop:checked=checked disabled=true/> }.into_any());
        children.push(" ".into_any());
    }
    for b in &item.blocks {
        if tight {
            if let Block::Paragraph(inlines) = b {
                children.extend(render_inlines(inlines));
                continue;
            }
        }
        children.push(render_block(b));
    }
    view! { <li>{children}</li> }.into_any()
}

fn render_table(
    alignments: &[Alignment],
    head: &[Vec<Inline<'_>>],
    rows: &[Vec<Vec<Inline<'_>>>],
) -> AnyView {
    let head_cells: Vec<AnyView> = head
        .iter()
        .enumerate()
        .map(|(i, cell)| render_cell(true, align_at(alignments, i), cell))
        .collect();
    let body_rows: Vec<AnyView> = rows
        .iter()
        .map(|row| {
            let cells: Vec<AnyView> = row
                .iter()
                .enumerate()
                .map(|(i, cell)| render_cell(false, align_at(alignments, i), cell))
                .collect();
            view! { <tr>{cells}</tr> }.into_any()
        })
        .collect();
    view! {
        <table>
            <thead><tr>{head_cells}</tr></thead>
            <tbody>{body_rows}</tbody>
        </table>
    }
    .into_any()
}

fn align_at(alignments: &[Alignment], i: usize) -> Option<&'static str> {
    match alignments.get(i).copied().unwrap_or(Alignment::None) {
        Alignment::Left => Some("text-align:left"),
        Alignment::Center => Some("text-align:center"),
        Alignment::Right => Some("text-align:right"),
        Alignment::None => None,
    }
}

fn render_cell(head: bool, style: Option<&'static str>, cell: &[Inline<'_>]) -> AnyView {
    let c = render_inlines(cell);
    if head {
        view! { <th style=style>{c}</th> }.into_any()
    } else {
        view! { <td style=style>{c}</td> }.into_any()
    }
}

fn render_inlines(inlines: &[Inline<'_>]) -> Vec<AnyView> {
    inlines.iter().map(render_inline).collect()
}

fn render_inline(node: &Inline<'_>) -> AnyView {
    match node {
        Inline::Text(t) => t.to_string().into_any(),
        Inline::Code(t) => {
            let s = t.to_string();
            view! { <code>{s}</code> }.into_any()
        }
        Inline::Emph(c) => {
            let c = render_inlines(c);
            view! { <em>{c}</em> }.into_any()
        }
        Inline::Strong(c) => {
            let c = render_inlines(c);
            view! { <strong>{c}</strong> }.into_any()
        }
        Inline::Strike(c) => {
            let c = render_inlines(c);
            view! { <del>{c}</del> }.into_any()
        }
        Inline::Link {
            dest,
            title,
            content,
        } => {
            let c = render_inlines(content);
            if is_safe_url(dest) {
                let href = dest.to_string();
                let title_attr = (!title.is_empty()).then(|| title.to_string());
                view! {
                    <a href=href title=title_attr rel="noopener noreferrer" target="_blank">
                        {c}
                    </a>
                }
                .into_any()
            } else {
                // Unsafe scheme → render the label text only, no anchor.
                view! { {c} }.into_any()
            }
        }
        Inline::Image { dest, title, alt } => {
            if is_safe_url(dest) {
                let src = dest.to_string();
                let alt = alt.clone();
                let title_attr = (!title.is_empty()).then(|| title.to_string());
                view! { <img src=src alt=alt title=title_attr/> }.into_any()
            } else {
                alt.clone().into_any()
            }
        }
        Inline::Math { content, display } => {
            // Engine-generated MathML (text escaped) → safe to inner_html.
            let mathml = crate::math::to_mathml(content, *display);
            view! { <span inner_html=mathml></span> }.into_any()
        }
        Inline::SoftBreak => "\n".into_any(),
        Inline::HardBreak => view! { <br/> }.into_any(),
    }
}
