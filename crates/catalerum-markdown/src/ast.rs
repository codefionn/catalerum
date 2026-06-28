//! The internal block/inline tree the parser builds and the renderers walk.
//!
//! Kept separate from the public [`crate::event`] vocabulary: the tree is the
//! natural shape for the two-phase parse (blocks first, then inline within each
//! text span) and for recursive rendering, while [`crate::parse`] flattens it to
//! an event stream for `pulldown`-style consumers.

use std::borrow::Cow;

use crate::event::Alignment;

/// A block-level node.
// `CodeBlock` is the standard term (and matches `pulldown-cmark`); keep it despite
// the suffix matching the enum name.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block<'a> {
    Paragraph(Vec<Inline<'a>>),
    Heading {
        level: u8,
        content: Vec<Inline<'a>>,
    },
    CodeBlock {
        /// Fenced info string (language); empty for indented blocks.
        info: Cow<'a, str>,
        /// `true` for an indented (4-space) code block.
        indented: bool,
        literal: String,
    },
    Quote(Vec<Block<'a>>),
    List {
        /// `Some(start)` ordered, `None` bullet.
        start: Option<u64>,
        /// Tight lists render `<li>` content without wrapping `<p>`.
        tight: bool,
        items: Vec<ListItem<'a>>,
    },
    Table {
        alignments: Vec<Alignment>,
        head: Vec<Vec<Inline<'a>>>,
        rows: Vec<Vec<Vec<Inline<'a>>>>,
    },
    Rule,
}

/// A single list item: optional task checkbox + its child blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ListItem<'a> {
    /// `Some(checked)` if this is a GFM task-list item.
    pub(crate) task: Option<bool>,
    pub(crate) blocks: Vec<Block<'a>>,
}

/// An inline node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inline<'a> {
    Text(Cow<'a, str>),
    Code(Cow<'a, str>),
    Emph(Vec<Inline<'a>>),
    Strong(Vec<Inline<'a>>),
    Strike(Vec<Inline<'a>>),
    Link {
        dest: Cow<'a, str>,
        title: Cow<'a, str>,
        content: Vec<Inline<'a>>,
    },
    Image {
        dest: Cow<'a, str>,
        title: Cow<'a, str>,
        alt: String,
    },
    /// LaTeX math from `$…$` (inline) or `$$…$$` (display). Rendered to MathML.
    Math {
        content: Cow<'a, str>,
        display: bool,
    },
    SoftBreak,
    HardBreak,
}
