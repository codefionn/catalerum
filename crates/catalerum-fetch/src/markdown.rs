//! HTML → Markdown conversion for cheap AI context (SOUL §27).
//!
//! Raw HTML is hostile to an LLM: it is mostly markup, scripts, styles, and
//! navigation chrome, and it burns context for almost no signal. This module
//! turns a page into clean, structural Markdown — headings, paragraphs, lists,
//! links, code, and tables — after stripping the boilerplate, so the model sees
//! the *content* at a fraction of the token cost.
//!
//! Two levers control how much is dropped:
//! - [`MarkdownOptions::main_content_only`] — pick the page's `<main>` / largest
//!   `<article>` (or, failing that, drop page chrome from `<body>` heuristically)
//!   before converting. This is the big context win.
//! - The structural skip-list — `<script>`, `<style>`, `<svg>`, form controls,
//!   `<head>` metadata, etc. carry no reading content and are always removed.
//!
//! The converter is pure and deterministic (no network, no JS): it parses with
//! `scraper` (html5ever) and walks the DOM. JavaScript-rendered pages are the
//! browser backend's job (`browser.rs`); whatever HTML it snapshots flows through
//! here just the same.

use std::cell::Cell;

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{ElementRef, Html, Selector};
use url::Url;

/// Max DOM/inline recursion depth for the converter walk. Untrusted fetched HTML
/// can nest pathologically deep (`<div>`×100000, nested `<b>`/`<blockquote>`); the
/// recursive walk would otherwise overflow the stack and **abort the worker**.
/// Past this depth we stop descending — adversarial nesting is truncated, never
/// crashed. Real pages are nowhere near it.
const MAX_NESTING: usize = 256;

/// Knobs for [`html_to_markdown`] (SOUL §27).
#[derive(Clone, Debug)]
pub struct MarkdownOptions {
    /// Resolve relative `href`/`src` against this base URL when set.
    pub base_url: Option<String>,
    /// Extract just the main article content, dropping nav/header/footer/aside
    /// and (when no `<main>`/`<article>` is found) page-chrome by class/id.
    pub main_content_only: bool,
    /// Include image `![alt](src)` references. Off trims more context.
    pub include_images: bool,
    /// Include `[text](href)` links. When off, link text is kept but the URL is
    /// dropped — cheaper context when the model only needs to read.
    pub include_links: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            main_content_only: true,
            include_images: true,
            include_links: true,
        }
    }
}

impl MarkdownOptions {
    /// Options with a base URL set (for relative-link resolution).
    #[must_use]
    pub fn with_base(base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..Self::default()
        }
    }
}

/// Convert an HTML document to clean Markdown (SOUL §27).
#[must_use]
pub fn html_to_markdown(html: &str, opts: &MarkdownOptions) -> String {
    let doc = Html::parse_document(html);
    let base = opts.base_url.as_deref().and_then(|b| Url::parse(b).ok());

    let (root, scoped) = pick_root(&doc, opts.main_content_only);
    let aggressive = opts.main_content_only && !scoped;

    let mut conv = Converter {
        base,
        opts,
        aggressive,
        out: String::new(),
        rec_depth: Cell::new(0),
    };
    conv.blocks(*root, 0);
    tidy(&conv.out)
}

/// Convert HTML straight to plain text (the Markdown with syntax stripped).
#[must_use]
pub fn html_to_text(html: &str, opts: &MarkdownOptions) -> String {
    markdown_to_text(&html_to_markdown(html, opts))
}

/// Extract a page title: `<title>`, else the first `<h1>`.
#[must_use]
pub fn extract_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let first_text = |selector: &str| -> Option<String> {
        let sel = Selector::parse(selector).ok()?;
        let el = doc.select(&sel).next()?;
        let s = collapse_ws(&el.text().collect::<String>());
        let s = s.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    first_text("title").or_else(|| first_text("h1"))
}

// ---------------------------------------------------------------------------
// Root selection (main-content extraction)
// ---------------------------------------------------------------------------

/// Choose the element to convert. Returns `(root, scoped)` where `scoped` is true
/// when a real `<main>`/`<article>` container was found (so chrome-stripping by
/// class/id is unnecessary and would risk dropping real content).
fn pick_root(doc: &Html, main_only: bool) -> (ElementRef<'_>, bool) {
    if main_only {
        if let Ok(sel) = Selector::parse("main") {
            if let Some(main) = doc.select(&sel).next() {
                return (main, true);
            }
        }
        // Multiple <article>s can exist (feeds); take the text-densest.
        if let Ok(sel) = Selector::parse("article") {
            if let Some(article) = doc
                .select(&sel)
                .max_by_key(|el| el.text().map(str::len).sum::<usize>())
            {
                return (article, true);
            }
        }
    }
    if let Ok(sel) = Selector::parse("body") {
        if let Some(body) = doc.select(&sel).next() {
            return (body, false);
        }
    }
    (doc.root_element(), false)
}

// ---------------------------------------------------------------------------
// Converter
// ---------------------------------------------------------------------------

struct Converter<'a> {
    base: Option<Url>,
    opts: &'a MarkdownOptions,
    /// Apply class/id chrome heuristics (only when we fell back to `<body>`).
    aggressive: bool,
    out: String,
    /// Current recursion depth of the DOM/inline walk, capped at [`MAX_NESTING`]
    /// so adversarially deep HTML can't overflow the stack. `Cell` so the `&self`
    /// inline walk can update it too.
    rec_depth: Cell<usize>,
}

impl<'a> Converter<'a> {
    /// Enter one recursion level; returns `false` if the [`MAX_NESTING`] cap is
    /// reached (the caller must then stop descending). Pair every `true` with
    /// [`leave`](Self::leave).
    fn enter(&self) -> bool {
        let d = self.rec_depth.get();
        if d >= MAX_NESTING {
            return false;
        }
        self.rec_depth.set(d + 1);
        true
    }

    /// Leave one recursion level (undo a successful [`enter`](Self::enter)).
    fn leave(&self) {
        self.rec_depth.set(self.rec_depth.get().saturating_sub(1));
    }

    /// Emit the block-level content of `node`'s children. Runs of inline content
    /// (text + inline elements) are grouped into paragraphs.
    fn blocks(&mut self, node: NodeRef<'a, Node>, depth: usize) {
        if !self.enter() {
            return;
        }
        let mut inline_buf = String::new();
        for child in node.children() {
            match child.value() {
                Node::Text(t) => inline_buf.push_str(&collapse_ws(&t.text)),
                Node::Element(el) => {
                    if self.skip(child) {
                        continue;
                    }
                    if is_block(el.name()) {
                        self.flush_paragraph(&mut inline_buf);
                        self.emit_block(child, el.name(), depth);
                    } else {
                        inline_buf.push_str(&self.inline_element(child));
                    }
                }
                _ => {}
            }
        }
        self.flush_paragraph(&mut inline_buf);
        self.leave();
    }

    /// Flush a gathered inline run as a paragraph block.
    fn flush_paragraph(&mut self, buf: &mut String) {
        let text = buf.trim();
        if !text.is_empty() {
            self.sep();
            self.out.push_str(text);
        }
        buf.clear();
    }

    fn emit_block(&mut self, node: NodeRef<'a, Node>, name: &str, depth: usize) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = (name.as_bytes()[1] - b'0') as usize;
                let text = self.inline(node);
                let text = text.trim();
                if !text.is_empty() {
                    self.sep();
                    self.out.push_str(&"#".repeat(level));
                    self.out.push(' ');
                    self.out.push_str(text);
                }
            }
            "p" => {
                let text = self.inline(node);
                let text = text.trim();
                if !text.is_empty() {
                    self.sep();
                    self.out.push_str(text);
                }
            }
            "hr" => {
                self.sep();
                self.out.push_str("---");
            }
            "ul" | "ol" => self.list(node, name == "ol", depth),
            "dl" => self.definition_list(node, depth),
            "blockquote" => self.blockquote(node, depth),
            "pre" => self.code_block(node),
            "table" => self.table(node),
            "img" => {
                if let Some(md) = self.image(node) {
                    self.sep();
                    self.out.push_str(&md);
                }
            }
            // A `<details>`'s `<summary>` is its disclosure title — render it bold so
            // it stays distinct from the collapsible body (which follows as normal
            // blocks, since `<details>` itself is a transparent container). Without
            // this the summary reads as just another paragraph.
            "summary" => {
                let text = self.inline(node);
                let text = text.trim();
                if !text.is_empty() {
                    self.sep();
                    self.out.push_str("**");
                    self.out.push_str(text);
                    self.out.push_str("**");
                }
            }
            // Transparent containers: descend, keeping the block context.
            _ => self.blocks(node, depth),
        }
    }

    /// Render an unordered/ordered list, one item per line, nested by `depth`.
    fn list(&mut self, node: NodeRef<'a, Node>, ordered: bool, depth: usize) {
        if !self.enter() {
            return;
        }
        // Top-level lists get a blank line before them; nested ones hug the
        // parent item line.
        if depth == 0 {
            self.sep();
        } else if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
        let indent = "  ".repeat(depth);
        // An ordered list honours its `start` attribute (e.g. a continued list
        // `<ol start="5">`); a missing/invalid value falls back to 1.
        let mut index: u32 = if ordered {
            match node.value() {
                Node::Element(el) => el
                    .attr("start")
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(1),
                _ => 1,
            }
        } else {
            1
        };
        for li in node.children() {
            let Node::Element(el) = li.value() else {
                continue;
            };
            if el.name() != "li" || self.skip(li) {
                continue;
            }
            let marker = if ordered {
                let m = format!("{index}. ");
                index = index.saturating_add(1);
                m
            } else {
                "- ".to_string()
            };
            // The item's own inline text (nested lists are emitted separately).
            let item = self.inline(li).trim().replace('\n', " ");
            if !self.out.is_empty() && !self.out.ends_with('\n') {
                self.out.push('\n');
            }
            self.out.push_str(&indent);
            self.out.push_str(&marker);
            self.out.push_str(item.trim());
            self.out.push('\n');
            for sub in li.children() {
                if let Node::Element(sub_el) = sub.value() {
                    if matches!(sub_el.name(), "ul" | "ol") && !self.skip(sub) {
                        self.list(sub, sub_el.name() == "ol", depth + 1);
                    }
                }
            }
        }
        self.leave();
    }

    /// A definition list (`<dl>`): each `<dt>` becomes a **bold term** line and each
    /// `<dd>` its definition rendered as normal block content beneath it — so the
    /// term→definition association is kept, instead of flattening to undistinguished
    /// adjacent paragraphs. Handles the HTML5 `<div>`-grouped form by recursing.
    fn definition_list(&mut self, node: NodeRef<'a, Node>, depth: usize) {
        if !self.enter() {
            return;
        }
        for child in node.children() {
            let Node::Element(el) = child.value() else {
                continue;
            };
            if self.skip(child) {
                continue;
            }
            match el.name() {
                "dt" => {
                    let term = self.inline(child);
                    let term = term.trim();
                    if !term.is_empty() {
                        self.sep();
                        self.out.push_str("**");
                        self.out.push_str(term);
                        self.out.push_str("**");
                    }
                }
                // A definition may hold blocks (paragraphs, lists), so render its
                // content as normal blocks beneath the term.
                "dd" => self.blocks(child, depth),
                // HTML5 allows wrapping each dt/dd group in a <div>.
                "div" => self.definition_list(child, depth),
                _ => {}
            }
        }
        self.leave();
    }

    fn blockquote(&mut self, node: NodeRef<'a, Node>, depth: usize) {
        // Render the quote's inner blocks into a sub-converter, then prefix `> `.
        let mut inner = Converter {
            base: self.base.clone(),
            opts: self.opts,
            aggressive: self.aggressive,
            out: String::new(),
            // Inherit the depth so nested `<blockquote>`s stay bounded across the
            // sub-converter boundary.
            rec_depth: Cell::new(self.rec_depth.get()),
        };
        inner.blocks(node, depth);
        let rendered = tidy(&inner.out);
        if rendered.is_empty() {
            return;
        }
        self.sep();
        for (i, line) in rendered.lines().enumerate() {
            if i > 0 {
                self.out.push('\n');
            }
            self.out.push_str("> ");
            self.out.push_str(line);
        }
    }

    /// A fenced code block, preserving whitespace and detecting the language from
    /// a `<code class="language-xxx">` child.
    fn code_block(&mut self, node: NodeRef<'a, Node>) {
        let lang = node
            .children()
            .find_map(|c| match c.value() {
                Node::Element(el) if el.name() == "code" => el
                    .attr("class")
                    .and_then(|cls| cls.split_whitespace().find_map(language_from_class)),
                _ => None,
            })
            // Fallback: some highlighters put the language class on the `<pre>`
            // itself (`<pre class="language-rust">`) rather than the inner `<code>`.
            .or_else(|| match node.value() {
                Node::Element(el) => el
                    .attr("class")
                    .and_then(|cls| cls.split_whitespace().find_map(language_from_class)),
                _ => None,
            })
            .unwrap_or_default();
        let code = raw_text(node);
        let code = code.trim_matches('\n');
        self.sep();
        self.out.push_str("```");
        self.out.push_str(&lang);
        self.out.push('\n');
        self.out.push_str(code);
        self.out.push('\n');
        self.out.push_str("```");
    }

    /// A GitHub-flavoured Markdown table.
    fn table(&mut self, node: NodeRef<'a, Node>) {
        // A `<caption>` describes the table; only `<tr>` cells are collected below, so
        // capture it here (else it's silently dropped) to emit as a line above.
        let caption = node
            .children()
            .find(|c| matches!(c.value(), Node::Element(el) if el.name() == "caption"))
            .filter(|c| !self.skip(*c))
            .map(|c| self.inline(c).trim().to_string())
            .filter(|s| !s.is_empty());

        // Lay cells onto a grid that honours `colspan`/`rowspan`, so a spanning cell
        // no longer shifts every following column out of alignment. GFM has no native
        // span; a spanned region holds the content in its top-left slot and "" in the
        // rest, keeping the grid rectangular so each datum stays under its column.
        let mut grid: Vec<Vec<Option<String>>> = Vec::new();
        // Per-output-column alignment, recovered from the first row that has cells.
        let mut aligns: Vec<Align> = Vec::new();
        let mut aligns_set = false;
        let mut r = 0usize;
        for tr in descendants_named(node, "tr") {
            if grid.len() <= r {
                grid.resize_with(r + 1, Vec::new);
            }
            let mut c = 0usize;
            let mut row_has_cell = false;
            for cell in tr.children() {
                let Node::Element(el) = cell.value() else {
                    continue;
                };
                if !matches!(el.name(), "td" | "th") || self.skip(cell) {
                    continue;
                }
                let text = self
                    .inline(cell)
                    .trim()
                    .replace('\n', " ")
                    .replace('|', "\\|");
                let colspan = span_attr(el, "colspan");
                let rowspan = span_attr(el, "rowspan");
                // Step past columns already claimed by a rowspan coming down from above.
                while c < grid[r].len() && grid[r][c].is_some() {
                    c += 1;
                }
                if grid.len() < r + rowspan {
                    grid.resize_with(r + rowspan, Vec::new);
                }
                for dr in 0..rowspan {
                    let row = &mut grid[r + dr];
                    if row.len() < c + colspan {
                        row.resize(c + colspan, None);
                    }
                    for dc in 0..colspan {
                        if row[c + dc].is_none() {
                            // Content lives in the top-left slot; the rest are blanks.
                            let owns = dr == 0 && dc == 0;
                            row[c + dc] = Some(if owns { text.clone() } else { String::new() });
                        }
                    }
                }
                if !aligns_set {
                    let a = cell_align(el);
                    if aligns.len() < c + colspan {
                        aligns.resize(c + colspan, Align::None);
                    }
                    for dc in 0..colspan {
                        aligns[c + dc] = a;
                    }
                }
                c += colspan;
                row_has_cell = true;
            }
            aligns_set |= row_has_cell;
            r += 1;
        }

        // Flatten to a rectangular string grid, dropping rows that ended up all-empty
        // (e.g. a row consisting only of rowspan continuations, or fully skipped).
        let cols = grid.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        let rows: Vec<Vec<String>> = grid
            .into_iter()
            .map(|row| {
                let mut v: Vec<String> = row.into_iter().map(Option::unwrap_or_default).collect();
                v.resize(cols, String::new());
                v
            })
            .filter(|row| row.iter().any(|cell| !cell.is_empty()))
            .collect();
        if rows.is_empty() {
            return;
        }
        if let Some(cap) = &caption {
            self.sep();
            self.out.push_str(cap);
        }
        self.sep();
        push_table_row(&mut self.out, &rows[0], cols);
        // Delimiter row carries GFM alignment markers (`:---`, `:---:`, `---:`).
        self.out.push_str("\n|");
        for i in 0..cols {
            self.out
                .push_str(aligns.get(i).copied().unwrap_or(Align::None).marker());
        }
        for row in &rows[1..] {
            self.out.push('\n');
            push_table_row(&mut self.out, row, cols);
        }
    }

    /// Collect inline Markdown (text + inline elements) for `node`'s subtree.
    fn inline(&self, node: NodeRef<'a, Node>) -> String {
        if !self.enter() {
            return String::new();
        }
        let mut out = String::new();
        for child in node.children() {
            match child.value() {
                Node::Text(t) => out.push_str(&collapse_ws(&t.text)),
                Node::Element(_) if !self.skip(child) => {
                    out.push_str(&self.inline_element(child));
                }
                _ => {}
            }
        }
        self.leave();
        out
    }

    /// Render a single element in inline context, applying its own Markdown
    /// formatting (so a bare `<a>` at block level still becomes a link).
    fn inline_element(&self, node: NodeRef<'a, Node>) -> String {
        let Node::Element(el) = node.value() else {
            return String::new();
        };
        match el.name() {
            "br" => "\n".to_string(),
            "strong" | "b" => wrapped("**", &self.inline(node)),
            // `<var>` (variable), `<cite>` (work title) and `<dfn>` (defining term)
            // are rendered italic like `<em>`/`<i>`; emphasis preserves that.
            "em" | "i" | "var" | "cite" | "dfn" => wrapped("*", &self.inline(node)),
            "del" | "s" | "strike" => wrapped("~~", &self.inline(node)),
            // `<mark>` (highlighted/relevant text — search hits, editorial emphasis)
            // carries a signal a reader sees; keep it with the `==…==` highlight
            // convention (GitHub/Obsidian/Pandoc), the same "preserve what the reader
            // sees" choice made for `<q>`/sup/sub rather than flattening to plain text.
            "mark" => wrapped("==", &self.inline(node)),
            // Superscript/subscript carry meaning on the general web (`mc^2`,
            // `H~2~O`, footnote markers); preserve it with the Pandoc convention
            // (`^…^` / `~…~`) rather than flattening to ambiguous adjacent text.
            "sup" => wrapped("^", &self.inline(node)),
            "sub" => wrapped("~", &self.inline(node)),
            // Inline quotation: a reader sees quote marks, so keep them (plain
            // Markdown has no `<q>` syntax — flattening would drop the quoting).
            "q" => wrapped("\"", &self.inline(node)),
            // `<kbd>` (keyboard input) and `<samp>` (sample output) are monospace
            // like `<code>`; inline code is their faithful Markdown equivalent
            // (flattening would drop the monospace distinction a reader sees).
            "code" | "kbd" | "samp" => {
                let inner = self.inline(node);
                let inner = inner.trim();
                if inner.is_empty() {
                    String::new()
                } else {
                    format!("`{}`", inner.replace('`', "'"))
                }
            }
            "a" => self.anchor(node),
            "img" => self.image(node).unwrap_or_default(),
            // Block containers never belong in an inline run.
            "ul" | "ol" | "table" | "blockquote" | "pre" | "figure" | "figcaption" | "hr" => {
                String::new()
            }
            // Inline-wrapping or unknown elements: flatten their text.
            _ => self.inline(node),
        }
    }

    fn anchor(&self, node: NodeRef<'a, Node>) -> String {
        let text = self.inline(node);
        let text = text.trim();
        if text.is_empty() {
            return String::new();
        }
        let Node::Element(el) = node.value() else {
            return text.to_string();
        };
        let href = el.attr("href").map(str::trim).unwrap_or("");
        if !self.opts.include_links || href.is_empty() || is_nonnavigational(href) {
            return text.to_string();
        }
        format!("[{text}]({})", self.resolve(href))
    }

    fn image(&self, node: NodeRef<'a, Node>) -> Option<String> {
        if !self.opts.include_images {
            return None;
        }
        let Node::Element(el) = node.value() else {
            return None;
        };
        let src = el.attr("src").or_else(|| el.attr("data-src"))?.trim();
        if src.is_empty() || src.starts_with("data:") {
            return None;
        }
        let alt = el.attr("alt").map(str::trim).unwrap_or("");
        Some(format!("![{alt}]({})", self.resolve(src)))
    }

    /// Resolve a possibly-relative URL against the base.
    fn resolve(&self, href: &str) -> String {
        let url = match &self.base {
            Some(base) => base
                .join(href)
                .map(String::from)
                .unwrap_or_else(|_| href.to_string()),
            None => href.to_string(),
        };
        // A literal `|` is not RFC-3986-valid in a URL, and inside a GFM table cell
        // the per-cell pipe-escaping (`\|`) would otherwise corrupt the href
        // (`[t](…a%7Cb)` not `[t](…a\|b)`). Encode it canonically so the URL is
        // well-formed in every context (tables and prose alike).
        url.replace('|', "%7C")
    }

    /// Ensure the output is separated from the next block by a blank line.
    fn sep(&mut self) {
        if self.out.is_empty() {
            return;
        }
        while self.out.ends_with(['\n', ' ']) {
            self.out.pop();
        }
        self.out.push_str("\n\n");
    }

    /// Should this element (and its subtree) be dropped entirely?
    fn skip(&self, node: NodeRef<'a, Node>) -> bool {
        let Node::Element(el) = node.value() else {
            return false;
        };
        let name = el.name();
        if STRUCTURAL_SKIP.contains(&name) {
            return true;
        }
        if el.attr("hidden").is_some() || el.attr("aria-hidden") == Some("true") {
            return true;
        }
        if self.opts.main_content_only {
            if CHROME_TAGS.contains(&name) {
                return true;
            }
            if let Some(role) = el.attr("role") {
                if CHROME_ROLES.contains(&role) {
                    return true;
                }
            }
        }
        // Class/id chrome heuristics only when we fell back to <body> (no
        // <main>/<article> scoped the content for us).
        self.aggressive
            && (el.id().is_some_and(attr_is_chrome) || el.attr("class").is_some_and(attr_is_chrome))
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Wrap non-empty `inner` in `delim`…`delim`, preserving a leading/trailing space
/// so emphasis doesn't glue to neighbours.
fn wrapped(delim: &str, inner: &str) -> String {
    let trimmed = inner.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    if inner.starts_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(delim);
    out.push_str(trimmed);
    out.push_str(delim);
    if inner.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out
}

/// Push one `| a | b |` table row, padding to `cols` cells.
/// A table column's text alignment, recovered from an HTML cell's `align`
/// attribute or `text-align` style and emitted as the GFM delimiter marker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

impl Align {
    /// The delimiter-row cell for this alignment (with the surrounding ` … |`).
    fn marker(self) -> &'static str {
        match self {
            Align::None => " --- |",
            Align::Left => " :--- |",
            Align::Center => " :---: |",
            Align::Right => " ---: |",
        }
    }

    /// Map an `align`/`text-align` keyword to an alignment (`justify`/unknown → None).
    fn from_keyword(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase();
        if s.starts_with("center") {
            Align::Center
        } else if s.starts_with("right") {
            Align::Right
        } else if s.starts_with("left") {
            Align::Left
        } else {
            Align::None
        }
    }
}

/// A cell's alignment from its `align` attribute, else its `text-align` style.
fn cell_align(el: &scraper::node::Element) -> Align {
    if let Some(a) = el.attr("align") {
        let al = Align::from_keyword(a);
        if al != Align::None {
            return al;
        }
    }
    if let Some(style) = el.attr("style") {
        let s = style.to_ascii_lowercase();
        if let Some(idx) = s.find("text-align") {
            if let Some(rest) = s[idx..].split_once(':') {
                return Align::from_keyword(rest.1);
            }
        }
    }
    Align::None
}

/// A `colspan`/`rowspan` value, clamped to `1..=1000` — defaults to 1 when absent
/// or unparsable, and the cap defends against an absurd span inflating the grid.
fn span_attr(el: &scraper::node::Element, name: &str) -> usize {
    el.attr(name)
        .and_then(|s| s.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 1000)
}

fn push_table_row(out: &mut String, cells: &[String], cols: usize) {
    out.push('|');
    for i in 0..cols {
        out.push(' ');
        out.push_str(cells.get(i).map(String::as_str).unwrap_or(""));
        out.push_str(" |");
    }
}

/// Collapse any run of whitespace to a single space, preserving at most one
/// leading and one trailing space (so inline concatenation stays word-separated).
fn collapse_ws(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let leading = s.starts_with(char::is_whitespace);
    let trailing = s.ends_with(char::is_whitespace);
    let mut out = String::with_capacity(s.len());
    if leading {
        out.push(' ');
    }
    for (i, word) in s.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    if trailing && !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
    out
}

/// Concatenate raw text descendants verbatim (for `<pre>`; preserves newlines).
fn raw_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    for d in node.descendants() {
        if let Node::Text(t) = d.value() {
            out.push_str(&t.text);
        }
    }
    out
}

/// All descendant elements with the given tag name.
fn descendants_named<'a>(
    node: NodeRef<'a, Node>,
    name: &'a str,
) -> impl Iterator<Item = NodeRef<'a, Node>> {
    node.descendants().filter(move |d| match d.value() {
        Node::Element(el) => el.name() == name,
        _ => false,
    })
}

/// Map a `class` token like `language-rust` / `lang-py` to a fence language.
fn language_from_class(class: &str) -> Option<String> {
    class
        .strip_prefix("language-")
        .or_else(|| class.strip_prefix("lang-"))
        .map(str::to_string)
}

/// True for hrefs that aren't real navigations (anchors, `javascript:`, etc.).
fn is_nonnavigational(href: &str) -> bool {
    href.starts_with('#') || href.starts_with("javascript:") || href.starts_with("data:")
}

/// True when a class/id attribute value marks page chrome.
fn attr_is_chrome(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if lower.contains("cookie") || lower.contains("newsletter") {
        return true;
    }
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| !tok.is_empty() && CHROME_TOKENS.contains(&tok.to_ascii_lowercase().as_str()))
}

/// Normalise the assembled Markdown: cap blank-line runs at one, trim trailing
/// spaces per line, and trim the whole document.
fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Strip Markdown syntax to plain text (for `FetchFormat::Text`). Blank lines are
/// dropped so the result is compact.
#[must_use]
pub fn markdown_to_text(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let stripped = line.trim_start_matches(['#', '>', '-', ' ']);
        // Strip a leading ordered-list marker too (the renderer emits `1. `, `2. `);
        // unordered `* ` is removed below. Keeps plain text free of list scaffolding.
        let stripped = strip_ordered_marker(stripped);
        let mut line = strip_links(stripped);
        line = line.replace("**", "").replace("~~", "").replace('`', "");
        let cleaned = line.replace("* ", "").trim().to_string();
        if cleaned.is_empty() {
            continue;
        }
        out.push_str(&cleaned);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Strip a leading ordered-list marker (`N. ` or `N) `) from a line. Conservative:
/// only a run of ASCII digits at the very start followed by `. `/`) ` counts, so a
/// decimal like `3.14` (rest `.14`, no following space) or prose mid-line is left
/// intact. Line-start only, by construction (operates on the already-left-trimmed
/// line).
fn strip_ordered_marker(line: &str) -> &str {
    let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
    if rest.len() == line.len() {
        return line; // no leading digits → not a marker
    }
    rest.strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))
        .unwrap_or(line)
}

/// Replace `[text](url)` and `![alt](url)` with just `text`/`alt`.
///
/// UTF-8 safe: non-link text is copied a whole `char` at a time (never a raw
/// byte), and the `(url)` scan tracks parenthesis depth so a URL containing
/// balanced `()` (e.g. a Wikipedia `Foo_(disambiguation)` link) is consumed in
/// full rather than cut at the first `)`.
fn strip_links(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.char_indices().peekable();
    while let Some(&(i, ch)) = chars.peek() {
        // `[text](url)` or `![alt](url)`: `[` and `!` are ASCII, so byte indexing
        // around them stays on char boundaries.
        let is_img = ch == '!' && line[i + 1..].starts_with('[');
        if ch == '[' || is_img {
            let bracket = if is_img { i + 1 } else { i };
            if let Some((text, after)) = parse_bracketed(line, bracket) {
                if let Some(end) = parse_parens(line, after) {
                    out.push_str(text);
                    // Advance the iterator past the consumed link.
                    while chars.peek().is_some_and(|&(j, _)| j < end) {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        out.push(ch);
        chars.next();
    }
    out
}

/// At a `[` (byte `open`), return `(text, index_after_closing_bracket)` if a
/// matching `]` exists on the line.
fn parse_bracketed(line: &str, open: usize) -> Option<(&str, usize)> {
    let close_rel = line[open + 1..].find(']')?;
    let close = open + 1 + close_rel;
    Some((&line[open + 1..close], close + 1))
}

/// If `line[at..]` begins with `(`, consume a balanced-parenthesis run and return
/// the byte index just past the closing `)`.
fn parse_parens(line: &str, at: usize) -> Option<usize> {
    let rest = line.get(at..)?;
    if !rest.starts_with('(') {
        return None;
    }
    let mut depth = 0usize;
    for (j, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at + j + 1);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tag / token classification
// ---------------------------------------------------------------------------

/// Block-level tags that break paragraph runs.
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "details"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hgroup"
            | "hr"
            | "img"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "summary"
            | "table"
            | "ul"
    )
}

/// Tags that never carry reading content — always dropped.
const STRUCTURAL_SKIP: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "object", "embed",
    "head", "link", "meta", "base", "button", "input", "select", "textarea", "option", "dialog",
    "audio", "video", "source", "track", "map", "area",
];

/// Structural chrome dropped under `main_content_only`.
const CHROME_TAGS: &[&str] = &["nav", "header", "footer", "aside", "form"];

/// ARIA landmark roles that mark page chrome.
const CHROME_ROLES: &[&str] = &[
    "navigation",
    "banner",
    "contentinfo",
    "complementary",
    "search",
    "menu",
    "menubar",
];

/// Class/id tokens that mark page chrome (applied only when we fell back to
/// `<body>` with no `<main>`/`<article>` to scope content).
const CHROME_TOKENS: &[&str] = &[
    "nav",
    "navbar",
    "navigation",
    "menu",
    "sidebar",
    "footer",
    "header",
    "masthead",
    "banner",
    "advert",
    "advertisement",
    "ads",
    "social",
    "share",
    "sharing",
    "comments",
    "related",
    "recommend",
    "recommended",
    "promo",
    "subscribe",
    "breadcrumb",
    "breadcrumbs",
    "pagination",
    // Modern interstitials / boilerplate (only dropped in the <body>-fallback case).
    "consent",
    "popup",
    "paywall",
    "toolbar",
    "trending",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        html_to_markdown(html, &MarkdownOptions::default())
    }

    #[test]
    fn headings_and_paragraphs() {
        let out = md("<h1>Title</h1><p>Hello <b>world</b>.</p>");
        assert_eq!(out, "# Title\n\nHello **world**.");
    }

    #[test]
    fn strips_scripts_and_styles() {
        let out = md("<p>keep</p><script>var x=1;</script><style>.a{}</style>");
        assert_eq!(out, "keep");
    }

    #[test]
    fn links_resolve_against_base() {
        let opts = MarkdownOptions::with_base("https://example.com/a/b");
        let out = html_to_markdown("<p>see <a href=\"../docs\">docs</a></p>", &opts);
        assert_eq!(out, "see [docs](https://example.com/docs)");
    }

    #[test]
    fn nonnavigational_links_become_text() {
        let out = md(r##"<p><a href="#">top</a> <a href="javascript:void(0)">x</a></p>"##);
        assert_eq!(out, "top x");
    }

    #[test]
    fn top_level_anchor_keeps_link() {
        let opts = MarkdownOptions::with_base("https://e.com/");
        let out = html_to_markdown(r#"<body><a href="/x">go</a></body>"#, &opts);
        assert_eq!(out, "[go](https://e.com/x)");
    }

    #[test]
    fn unordered_list() {
        let out = md("<ul><li>one</li><li>two</li></ul>");
        assert_eq!(out, "- one\n- two");
    }

    #[test]
    fn nested_ordered_list() {
        let out = md("<ol><li>a<ul><li>a1</li></ul></li><li>b</li></ol>");
        assert_eq!(out, "1. a\n  - a1\n2. b");
    }

    #[test]
    fn ordered_list_honours_start_attribute() {
        // A continued/offset list keeps its numbering.
        assert_eq!(
            md("<ol start=\"5\"><li>a</li><li>b</li></ol>"),
            "5. a\n6. b"
        );
        // An invalid/absent `start` falls back to 1; unordered lists ignore it.
        assert_eq!(md("<ol start=\"x\"><li>a</li></ol>"), "1. a");
        assert_eq!(md("<ul start=\"5\"><li>a</li></ul>"), "- a");
    }

    #[test]
    fn code_block_with_language() {
        let out = md("<pre><code class=\"language-rust\">fn main() {}\n</code></pre>");
        assert_eq!(out, "```rust\nfn main() {}\n```");
    }

    #[test]
    fn code_block_language_on_pre_element() {
        // Some highlighters tag the `<pre>` itself, not the inner `<code>`.
        let out = md("<pre class=\"language-python\"><code>print(1)\n</code></pre>");
        assert_eq!(out, "```python\nprint(1)\n```");
        // The inner `<code>`'s class still wins when both are present.
        let out = md(
            "<pre class=\"language-python\"><code class=\"lang-rust\">fn main(){}\n</code></pre>",
        );
        assert_eq!(out, "```rust\nfn main(){}\n```");
        // A highlighter class with no recognizable language → an unlabelled fence.
        let out = md("<pre class=\"highlight\"><code>plain\n</code></pre>");
        assert_eq!(out, "```\nplain\n```");
    }

    #[test]
    fn inline_code_and_emphasis() {
        let out = md("<p>use <code>cargo</code> and <em>fast</em></p>");
        assert_eq!(out, "use `cargo` and *fast*");
    }

    #[test]
    fn details_summary_renders_summary_as_bold_title() {
        // The summary is the disclosure title (bold); the body follows as blocks.
        let out =
            md("<details><summary>Build steps</summary><p>Run <code>make</code>.</p></details>");
        assert_eq!(out, "**Build steps**\n\nRun `make`.");
        // A details with no summary still renders its body.
        let out = md("<details><p>just body</p></details>");
        assert_eq!(out, "just body");
        // Inline formatting in the summary is preserved.
        let out = md("<details><summary>See <code>--help</code></summary><p>x</p></details>");
        assert_eq!(out, "**See `--help`**\n\nx");
    }

    #[test]
    fn var_cite_dfn_render_as_emphasis() {
        // Browser-italic semantic elements → Markdown emphasis.
        assert_eq!(md("<p>set <var>x</var> = 1</p>"), "set *x* = 1");
        assert_eq!(md("<p>see <cite>The Book</cite></p>"), "see *The Book*");
        assert_eq!(md("<p>a <dfn>widget</dfn> is…</p>"), "a *widget* is…");
    }

    #[test]
    fn mark_renders_as_highlight() {
        // `<mark>` (highlighted text) → the `==…==` highlight convention, preserving
        // the emphasis a reader sees rather than flattening it to plain text.
        assert_eq!(md("<p>the <mark>key</mark> point</p>"), "the ==key== point");
        // Empty highlight collapses (no stray `====`).
        assert_eq!(md("<p>a<mark></mark>b</p>"), "ab");
    }

    #[test]
    fn kbd_and_samp_render_as_inline_code() {
        // Keyboard input / sample output are monospace → inline code.
        assert_eq!(
            md("<p>press <kbd>Ctrl</kbd>+<kbd>C</kbd></p>"),
            "press `Ctrl`+`C`"
        );
        assert_eq!(md("<p>output: <samp>done</samp></p>"), "output: `done`");
        // Empty contributes nothing, like <code>.
        assert_eq!(md("<p>x<kbd></kbd>y</p>"), "xy");
    }

    #[test]
    fn inline_quotation_keeps_quote_marks() {
        // `<q>` renders with quotes in a browser; the conversion must keep them.
        assert_eq!(md("<p>She said <q>hello</q>.</p>"), "She said \"hello\".");
        // Inline formatting inside the quote is preserved; empty <q> adds nothing.
        assert_eq!(md("<p><q>see <em>this</em></q></p>"), "\"see *this*\"");
        assert_eq!(md("<p>x<q></q>y</p>"), "xy");
    }

    #[test]
    fn superscript_and_subscript() {
        // Superscript and subscript keep their meaning instead of flattening
        // into ambiguous adjacent characters (`mc2` / `H2O`).
        assert_eq!(md("<p>E = mc<sup>2</sup></p>"), "E = mc^2^");
        assert_eq!(md("<p>H<sub>2</sub>O</p>"), "H~2~O");
        // A footnote-style superscript link is preserved inside the markers.
        assert_eq!(
            md("<p>claim<sup><a href=\"https://x.test/f\">1</a></sup></p>"),
            "claim^[1](https://x.test/f)^"
        );
        // An empty sup/sub contributes nothing (no stray `^^` / `~~`).
        assert_eq!(md("<p>x<sup></sup>y</p>"), "xy");
    }

    #[test]
    fn definition_list_bolds_terms_and_keeps_definitions() {
        let out =
            md("<dl><dt>Rust</dt><dd>A systems language</dd><dt>Go</dt><dd>Another</dd></dl>");
        assert_eq!(out, "**Rust**\n\nA systems language\n\n**Go**\n\nAnother");
        // Inline formatting in a term is preserved; a definition can carry a link.
        let out = md(
            "<dl><dt><code>--force</code></dt><dd>see <a href=\"https://x.test/d\">docs</a></dd></dl>",
        );
        assert_eq!(out, "**`--force`**\n\nsee [docs](https://x.test/d)");
    }

    #[test]
    fn table_to_gfm() {
        let html = "<table><thead><tr><th>a</th><th>b</th></tr></thead>\
                    <tbody><tr><td>1</td><td>2</td></tr></tbody></table>";
        let out = md(html);
        assert_eq!(out, "| a | b |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn table_preserves_column_alignment() {
        // `align` attributes on the header cells → GFM alignment markers.
        let html = "<table><tr>\
                    <th align=\"left\">A</th><th align=\"center\">B</th><th align=\"right\">C</th>\
                    </tr><tr><td>1</td><td>2</td><td>3</td></tr></table>";
        assert_eq!(
            md(html),
            "| A | B | C |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |"
        );
        // `text-align` in a style attribute is honoured too.
        let styled =
            "<table><tr><th style=\"text-align: right\">N</th></tr><tr><td>5</td></tr></table>";
        assert_eq!(md(styled), "| N |\n| ---: |\n| 5 |");
        // No alignment → the plain delimiter (unchanged behaviour).
        let plain = "<table><tr><th>x</th></tr><tr><td>1</td></tr></table>";
        assert_eq!(md(plain), "| x |\n| --- |\n| 1 |");
    }

    #[test]
    fn table_caption_renders_above_the_table() {
        // A caption would otherwise be dropped; it becomes a line above the table.
        let html = "<table><caption>Quarterly revenue</caption>\
                    <tr><th>Q</th><th>Rev</th></tr><tr><td>1</td><td>100</td></tr></table>";
        assert_eq!(
            md(html),
            "Quarterly revenue\n\n| Q | Rev |\n| --- | --- |\n| 1 | 100 |"
        );
        // Inline formatting in the caption is preserved.
        let styled = "<table><caption>see <a href=\"https://x.test\">x</a></caption>\
                      <tr><th>a</th></tr><tr><td>1</td></tr></table>";
        assert_eq!(
            md(styled),
            "see [x](https://x.test)\n\n| a |\n| --- |\n| 1 |"
        );
    }

    #[test]
    fn table_colspan_pads_columns_to_stay_aligned() {
        // A `colspan=2` header used to emit one cell and shift every data column
        // left; now it occupies two grid columns (content then a blank) so the data
        // stays under its heading.
        let html = "<table><tr><th colspan=\"2\">Span</th><th>C</th></tr>\
                    <tr><td>1</td><td>2</td><td>3</td></tr></table>";
        assert_eq!(
            md(html),
            "| Span |  | C |\n| --- | --- | --- |\n| 1 | 2 | 3 |"
        );
    }

    #[test]
    fn table_rowspan_carries_the_column_down() {
        // A `rowspan=2` first cell reserves its column in the next row, so the
        // following row's lone cell lands in the *second* column, not the first.
        let html = "<table><tr><td rowspan=\"2\">A</td><td>B</td></tr>\
                    <tr><td>C</td></tr></table>";
        assert_eq!(md(html), "| A | B |\n| --- | --- |\n|  | C |");
    }

    #[test]
    fn table_colspan_and_rowspan_together_keep_the_grid_rectangular() {
        let html = "<table>\
            <tr><th>Name</th><th colspan=\"2\">Score</th></tr>\
            <tr><td rowspan=\"2\">Bob</td><td>10</td><td>20</td></tr>\
            <tr><td>30</td><td>40</td></tr></table>";
        assert_eq!(
            md(html),
            "| Name | Score |  |\n| --- | --- | --- |\n| Bob | 10 | 20 |\n|  | 30 | 40 |"
        );
        // An absurd colspan is clamped, not used to inflate the grid unboundedly.
        let huge = "<table><tr><td colspan=\"100000000\">x</td></tr></table>";
        assert!(md(huge).starts_with("| x |"), "{}", md(huge));
    }

    #[test]
    fn blockquote_prefixes() {
        let out = md("<blockquote><p>quoted</p></blockquote>");
        assert_eq!(out, "> quoted");
    }

    #[test]
    fn main_content_extraction_drops_chrome() {
        let html = "<body><nav>menu home about</nav>\
                    <main><h1>Real</h1><p>Body text.</p></main>\
                    <footer>copyright</footer></body>";
        let out = md(html);
        assert_eq!(out, "# Real\n\nBody text.");
    }

    #[test]
    fn body_fallback_strips_chrome_by_class() {
        let html = "<body><div class=\"sidebar\">ads ads</div>\
                    <div class=\"content\"><p>Article body.</p></div></body>";
        let out = md(html);
        assert_eq!(out, "Article body.");
    }

    #[test]
    fn body_fallback_drops_modern_boilerplate() {
        // Each of these is caught only by a newly-added chrome token (not a
        // pre-existing one), confirming the additions work.
        for chrome in [
            "consent-modal",
            "recommended-posts",
            "trending-now",
            "popup-cta",
            "paywall-overlay",
        ] {
            let html = format!(
                "<body><div class=\"{chrome}\">junk junk</div>\
                 <div class=\"content\"><p>Real text.</p></div></body>"
            );
            assert_eq!(md(&html), "Real text.", "`{chrome}` should be dropped");
        }
    }

    #[test]
    fn without_main_only_keeps_nav() {
        let opts = MarkdownOptions {
            main_content_only: false,
            ..MarkdownOptions::default()
        };
        let html = "<body><nav>Home</nav><main><p>Body.</p></main></body>";
        let out = html_to_markdown(html, &opts);
        assert!(out.contains("Home"));
        assert!(out.contains("Body."));
    }

    #[test]
    fn title_from_title_tag_then_h1() {
        assert_eq!(
            extract_title("<html><head><title>  T  </title></head><body><h1>H</h1></body></html>"),
            Some("T".to_string())
        );
        assert_eq!(
            extract_title("<body><h1>Heading</h1></body>"),
            Some("Heading".to_string())
        );
        assert_eq!(extract_title("<body><p>none</p></body>"), None);
    }

    #[test]
    fn plain_text_strips_syntax() {
        let text = html_to_text(
            "<h1>Hi</h1><p>see <a href=\"https://x.com\">link</a> and <b>bold</b></p>",
            &MarkdownOptions::default(),
        );
        assert_eq!(text, "Hi\nsee link and bold");
    }

    #[test]
    fn plain_text_preserves_unicode() {
        // Cyrillic + CJK must survive the Markdown→text strip (no mojibake).
        let text = html_to_text(
            "<p>Привет <a href=\"https://x\">мир</a> 世界</p>",
            &MarkdownOptions::default(),
        );
        assert_eq!(text, "Привет мир 世界");
    }

    #[test]
    fn plain_text_link_with_parens_in_url() {
        // A URL containing balanced parens must be consumed whole.
        let text = html_to_text(
            "<p>see <a href=\"https://en.wikipedia.org/wiki/Rust_(programming_language)\">Rust</a> now</p>",
            &MarkdownOptions::default(),
        );
        assert_eq!(text, "see Rust now");
    }

    #[test]
    fn collapses_whitespace() {
        let out = md("<p>a   b\n\t c</p>");
        assert_eq!(out, "a b c");
    }

    #[test]
    fn images_become_markdown() {
        let opts = MarkdownOptions::with_base("https://example.com/");
        let out = html_to_markdown("<p><img src=\"/logo.png\" alt=\"Logo\"></p>", &opts);
        assert_eq!(out, "![Logo](https://example.com/logo.png)");
    }

    #[test]
    fn context_savings_are_real() {
        // A boilerplate-heavy page should shrink dramatically.
        let html = format!(
            "<html><head><style>{}</style></head><body><nav>{}</nav>\
             <main><p>The single sentence that matters.</p></main></body></html>",
            "x".repeat(2000),
            "menu ".repeat(200),
        );
        let out = md(&html);
        assert_eq!(out, "The single sentence that matters.");
        assert!(out.len() * 10 < html.len());
    }

    #[test]
    fn deeply_nested_html_does_not_overflow_the_stack() {
        // Untrusted fetched HTML may nest pathologically deep; the recursive DOM
        // walk must not stack-overflow (→ abort → worker crash). `n` is well past
        // MAX_NESTING (256) so the depth guard is exercised; the conversion returns
        // cleanly instead of recursing to a crash. (Kept modest so html5ever's own
        // near-O(n²) deep-tree parse stays fast — the guard caps our walk, not the
        // parse; the byte cap in `FetchPolicy` bounds the input that reaches here.)
        let n = 4_000;
        let block = format!(
            "<html><body>{}<p>deep</p>{}</body></html>",
            "<div>".repeat(n),
            "</div>".repeat(n),
        );
        // Reaching the next line (not aborting on a stack overflow) is the assertion.
        let _ = md(&block);
        // The inline path (`<b><i>…`) is the other recursion vector — also bounded.
        let inline = format!("<p>{}x{}</p>", "<b>".repeat(n), "</b>".repeat(n));
        let _ = md(&inline);
    }

    #[test]
    fn url_pipe_is_percent_encoded_in_links_and_images() {
        // A literal `|` in an href/src must become %7C (RFC-3986 + so GFM table-cell
        // pipe-escaping can't corrupt the URL). Exercises `resolve` for both forms.
        let link = md("<p><a href=\"http://x/?q=a|b\">t</a></p>");
        assert!(link.contains("(http://x/?q=a%7Cb)"), "got: {link}");
        let img = md("<p><img src=\"http://x/i?w=1|2\" alt=\"a\"></p>");
        assert!(img.contains("(http://x/i?w=1%7C2)"), "got: {img}");
    }

    #[test]
    fn table_cell_link_url_pipe_is_encoded_not_backslash_escaped() {
        // The real scenario: a link with a piped URL inside a table cell must stay a
        // valid href (`%7C`), never get a stray `\|` injected by cell-pipe escaping.
        let out = md(
            "<table><tr><th>h</th></tr><tr><td><a href=\"http://x.com?a=1|b=2\">link</a></td></tr></table>",
        );
        assert!(
            out.contains("[link](http://x.com?a=1%7Cb=2)"),
            "url should be %7C-encoded, got: {out}"
        );
        assert!(
            !out.contains("\\|"),
            "no backslash-escaped pipe in the href: {out}"
        );
    }

    #[test]
    fn markdown_to_text_strips_ordered_markers_but_keeps_decimals() {
        // Ordered-list markers stripped (the renderer emits `1. `/`2. `)…
        assert_eq!(markdown_to_text("1. alpha\n2. bravo"), "alpha\nbravo");
        assert_eq!(markdown_to_text("3) gamma"), "gamma");
        assert_eq!(markdown_to_text("10. ten"), "ten");
        // …unordered bullets too (existing behaviour preserved)…
        assert_eq!(markdown_to_text("- x\n* y"), "x\ny");
        // …but a decimal / version (no `. ` space after the digits) is NOT a marker.
        assert_eq!(markdown_to_text("3.14 is pi"), "3.14 is pi");
        assert_eq!(markdown_to_text("v1.2.3 released"), "v1.2.3 released");
    }
}
