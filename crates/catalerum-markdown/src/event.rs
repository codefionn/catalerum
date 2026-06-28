//! The public, `pulldown-cmark`-style event vocabulary.
//!
//! [`crate::parse`] yields a flat stream of [`Event`]s; renderers consume it. The
//! stream is well-formed: every [`Event::Start`] is matched by a later
//! [`Event::End`] with the corresponding [`TagEnd`], properly nested.
//!
//! Text payloads are [`Cow`] so the common case (a run of plain text with no
//! escapes) borrows straight from the source with no allocation.

use std::borrow::Cow;

/// Column alignment for a GFM table, from the delimiter row (`:---`, `:--:`, `--:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    None,
    Left,
    Center,
    Right,
}

/// How a code block was written: indented (4 spaces) or fenced (with its info
/// string, e.g. `rust` or `mermaid`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeBlockKind<'a> {
    Indented,
    Fenced(Cow<'a, str>),
}

/// The opening of a container or leaf span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tag<'a> {
    Paragraph,
    /// `level` is `1..=6`.
    Heading(u8),
    BlockQuote,
    CodeBlock(CodeBlockKind<'a>),
    /// A list; `Some(n)` is an ordered list starting at `n`, `None` is a bullet list.
    List(Option<u64>),
    Item,
    Emphasis,
    Strong,
    Strikethrough,
    Link {
        dest: Cow<'a, str>,
        title: Cow<'a, str>,
    },
    Image {
        dest: Cow<'a, str>,
        title: Cow<'a, str>,
    },
    Table(Vec<Alignment>),
    TableHead,
    TableRow,
    TableCell,
}

/// The closing counterpart of a [`Tag`]. Carries only what a renderer needs to
/// emit the closing markup (e.g. the heading level, or the list kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagEnd {
    Paragraph,
    Heading(u8),
    BlockQuote,
    CodeBlock,
    List(bool),
    Item,
    Emphasis,
    Strong,
    Strikethrough,
    Link,
    Image,
    Table,
    TableHead,
    TableRow,
    TableCell,
}

/// A single parse event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<'a> {
    Start(Tag<'a>),
    End(TagEnd),
    /// Plain text (already unescaped; the renderer is responsible for HTML-escaping).
    Text(Cow<'a, str>),
    /// The literal contents of an inline code span or code block.
    Code(Cow<'a, str>),
    /// LaTeX source of inline `$…$` math.
    InlineMath(Cow<'a, str>),
    /// LaTeX source of display `$$…$$` math.
    DisplayMath(Cow<'a, str>),
    /// `![alt]` text inside an image — emitted between `Start(Image)`/`End(Image)`.
    SoftBreak,
    HardBreak,
    /// A thematic break (`<hr>`).
    Rule,
    /// A GFM task-list checkbox; `true` is checked. Emitted as the first event of
    /// its list item's content.
    TaskListMarker(bool),
}
