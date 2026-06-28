//! `catalerum-markdown` — a streaming, SIMD-accelerated Markdown parser that
//! renders to **HTML** or **Leptos views**, on both native and `wasm32`.
//!
//! It exists so the workbench (chat replies, Notes, Skills) and the server share
//! one Markdown engine: small enough for the wasm bundle, fast enough to re-run on
//! every streaming delta, and **injection-safe** — all text is HTML-escaped and
//! only vetted URL schemes survive, so the HTML output is sound to drop into
//! `inner_html` even for fully untrusted input.
//!
//! # What it supports
//!
//! CommonMark blocks (ATX + setext headings, fenced & indented code, block
//! quotes, bullet/ordered lists with tight/loose handling, thematic breaks,
//! paragraphs) and inlines (code spans, links/images incl. reference style,
//! autolinks, emphasis/strong via the spec delimiter-stack), plus GFM extensions:
//! **tables**, **strikethrough** (`~~`), **task lists** (`- [ ]`), and bare
//! autolinks. A fenced block tagged `mermaid` renders as `<pre class="mermaid">`
//! for client-side [mermaid](https://mermaid.js.org) diagramming.
//!
//! # Quick start
//!
//! ```
//! let html = catalerum_markdown::to_html("# Hi\n\nsome **bold** text");
//! assert!(html.contains("<h1 id=\"hi\">Hi</h1>")); // headings get anchor ids
//! assert!(html.contains("<strong>bold</strong>"));
//! ```
//!
//! Render straight into an existing buffer (no intermediate allocation):
//!
//! ```
//! let mut buf = String::new();
//! catalerum_markdown::push_html(&mut buf, "- a\n- b");
//! assert_eq!(buf, "<ul><li>a</li><li>b</li></ul>");
//! ```
//!
//! Or consume the `pulldown-cmark`-style [`Event`] stream directly:
//!
//! ```
//! use catalerum_markdown::{parse, Event, Tag};
//! let mut depth = 0;
//! for ev in parse("> quote") {
//!     if let Event::Start(Tag::BlockQuote) = ev { depth += 1; }
//! }
//! assert_eq!(depth, 1);
//! ```
//!
//! # Streaming
//!
//! [`StreamRenderer`] renders only the part of a growing buffer that can no longer
//! change, returning the unstable tail for the caller to show as plain text — see
//! its docs.
//!
//! # SIMD
//!
//! The scanner ([`crate::scan`], private) vectorises "find the next interesting
//! byte" with a nibble-table membership test on SSSE3/AVX2 (x86-64), NEON
//! (aarch64) and `simd128` (wasm, when enabled), falling back to a scalar table
//! lookup. Every back-end is checked against the scalar oracle by exhaustive
//! byte-pair tests.

mod ast;
mod escape;
mod event;
pub mod math;
pub mod mermaid;
mod parser;
mod render;
mod scan;
mod stream;

pub use event::{Alignment, CodeBlockKind, Event, Tag, TagEnd};
pub use parser::{parse, Parser};
pub use render::html::{push_html, to_html};
pub use stream::{stable_boundary, StreamRenderer};

#[cfg(feature = "leptos")]
pub use render::leptos::render_markdown;
