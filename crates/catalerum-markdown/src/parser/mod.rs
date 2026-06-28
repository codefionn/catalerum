//! The parser: source text → [`Block`] tree → flat [`Event`] stream.
//!
//! [`parse`] returns a [`Parser`] that yields `pulldown-cmark`-style [`Event`]s.
//! The block/inline tree is built eagerly (it is needed whole to resolve forward
//! references and list tightness), then flattened lazily on iteration.

pub(crate) mod block;
pub(crate) mod inline;

use std::borrow::Cow;

use crate::ast::{Block, Inline, ListItem};
use crate::event::{CodeBlockKind, Event, Tag, TagEnd};

/// An iterator over a document's parse [`Event`]s. See [`parse`].
pub struct Parser<'a> {
    events: std::vec::IntoIter<Event<'a>>,
}

impl<'a> Iterator for Parser<'a> {
    type Item = Event<'a>;

    #[inline]
    fn next(&mut self) -> Option<Event<'a>> {
        self.events.next()
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.events.size_hint()
    }
}

/// Parse `input` into an [`Event`] stream.
pub fn parse(input: &str) -> Parser<'_> {
    Parser {
        events: parse_events(input).into_iter(),
    }
}

/// Parse `input` into its event vector (shared by [`parse`] and the renderers,
/// which walk events directly).
pub(crate) fn parse_events(input: &str) -> Vec<Event<'_>> {
    let (blocks, _refs) = block::parse_document(input);
    let mut out = Vec::new();
    emit_blocks(&blocks, &mut out);
    out
}

fn emit_blocks<'a>(blocks: &[Block<'a>], out: &mut Vec<Event<'a>>) {
    for b in blocks {
        emit_block(b, out);
    }
}

fn emit_block<'a>(block: &Block<'a>, out: &mut Vec<Event<'a>>) {
    match block {
        Block::Paragraph(inlines) => {
            out.push(Event::Start(Tag::Paragraph));
            emit_inlines(inlines, out);
            out.push(Event::End(TagEnd::Paragraph));
        }
        Block::Heading { level, content } => {
            out.push(Event::Start(Tag::Heading(*level)));
            emit_inlines(content, out);
            out.push(Event::End(TagEnd::Heading(*level)));
        }
        Block::CodeBlock {
            info,
            indented,
            literal,
        } => {
            let kind = if *indented {
                CodeBlockKind::Indented
            } else {
                CodeBlockKind::Fenced(info.clone())
            };
            out.push(Event::Start(Tag::CodeBlock(kind)));
            out.push(Event::Code(Cow::Owned(literal.clone())));
            out.push(Event::End(TagEnd::CodeBlock));
        }
        Block::Quote(blocks) => {
            out.push(Event::Start(Tag::BlockQuote));
            emit_blocks(blocks, out);
            out.push(Event::End(TagEnd::BlockQuote));
        }
        Block::List {
            start,
            tight,
            items,
        } => {
            out.push(Event::Start(Tag::List(*start)));
            for item in items {
                emit_item(item, *tight, out);
            }
            out.push(Event::End(TagEnd::List(start.is_some())));
        }
        Block::Table {
            alignments,
            head,
            rows,
        } => emit_table(alignments, head, rows, out),
        Block::Rule => out.push(Event::Rule),
    }
}

fn emit_item<'a>(item: &ListItem<'a>, tight: bool, out: &mut Vec<Event<'a>>) {
    out.push(Event::Start(Tag::Item));
    if let Some(checked) = item.task {
        out.push(Event::TaskListMarker(checked));
    }
    for b in &item.blocks {
        // In a tight list, a paragraph's wrapping `<p>` is suppressed — emit its
        // inlines bare (this is how `pulldown-cmark` models tight lists too).
        if tight {
            if let Block::Paragraph(inlines) = b {
                emit_inlines(inlines, out);
                continue;
            }
        }
        emit_block(b, out);
    }
    out.push(Event::End(TagEnd::Item));
}

fn emit_table<'a>(
    alignments: &[crate::event::Alignment],
    head: &[Vec<Inline<'a>>],
    rows: &[Vec<Vec<Inline<'a>>>],
    out: &mut Vec<Event<'a>>,
) {
    out.push(Event::Start(Tag::Table(alignments.to_vec())));
    out.push(Event::Start(Tag::TableHead));
    for cell in head {
        out.push(Event::Start(Tag::TableCell));
        emit_inlines(cell, out);
        out.push(Event::End(TagEnd::TableCell));
    }
    out.push(Event::End(TagEnd::TableHead));
    for row in rows {
        out.push(Event::Start(Tag::TableRow));
        for cell in row {
            out.push(Event::Start(Tag::TableCell));
            emit_inlines(cell, out);
            out.push(Event::End(TagEnd::TableCell));
        }
        out.push(Event::End(TagEnd::TableRow));
    }
    out.push(Event::End(TagEnd::Table));
}

fn emit_inlines<'a>(inlines: &[Inline<'a>], out: &mut Vec<Event<'a>>) {
    for n in inlines {
        emit_inline(n, out);
    }
}

fn emit_inline<'a>(node: &Inline<'a>, out: &mut Vec<Event<'a>>) {
    match node {
        Inline::Text(t) => out.push(Event::Text(t.clone())),
        Inline::Code(t) => out.push(Event::Code(t.clone())),
        Inline::Emph(c) => {
            out.push(Event::Start(Tag::Emphasis));
            emit_inlines(c, out);
            out.push(Event::End(TagEnd::Emphasis));
        }
        Inline::Strong(c) => {
            out.push(Event::Start(Tag::Strong));
            emit_inlines(c, out);
            out.push(Event::End(TagEnd::Strong));
        }
        Inline::Strike(c) => {
            out.push(Event::Start(Tag::Strikethrough));
            emit_inlines(c, out);
            out.push(Event::End(TagEnd::Strikethrough));
        }
        Inline::Link {
            dest,
            title,
            content,
        } => {
            out.push(Event::Start(Tag::Link {
                dest: dest.clone(),
                title: title.clone(),
            }));
            emit_inlines(content, out);
            out.push(Event::End(TagEnd::Link));
        }
        Inline::Image { dest, title, alt } => {
            out.push(Event::Start(Tag::Image {
                dest: dest.clone(),
                title: title.clone(),
            }));
            out.push(Event::Text(Cow::Owned(alt.clone())));
            out.push(Event::End(TagEnd::Image));
        }
        Inline::Math { content, display } => out.push(if *display {
            Event::DisplayMath(content.clone())
        } else {
            Event::InlineMath(content.clone())
        }),
        Inline::SoftBreak => out.push(Event::SoftBreak),
        Inline::HardBreak => out.push(Event::HardBreak),
    }
}
