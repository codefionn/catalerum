//! Block parsing: source text → a tree of [`Block`]s.
//!
//! Line-oriented recursive descent. Containers (block quotes, list items) strip
//! their prefix and recurse, so nesting falls out naturally. Reference link
//! definitions are collected in a pre-pass so a `[ref]` used *before* its
//! definition still resolves (definitions are document-global).
//!
//! Two passes share one [`RefMap`]: pass one harvests `[label]: dest "title"`
//! lines (outside fenced code); pass two builds the tree, resolving inline
//! references against the complete map.

use std::borrow::Cow;

use crate::ast::{Block, Inline, ListItem};
use crate::event::Alignment;
use crate::parser::inline::{normalize_label, parse_inlines, LinkDef, RefMap};

/// Parse a whole document into blocks + the collected reference map.
pub(crate) fn parse_document(input: &str) -> (Vec<Block<'_>>, RefMap<'_>) {
    let lines: Vec<&str> = split_lines(input);
    let refs = collect_refs(&lines);
    let mut p = BlockParser {
        lines: &lines,
        pos: 0,
        refs: &refs,
    };
    let blocks = p.parse_blocks();
    (blocks, refs)
}

/// Split into lines without allocating per line, dropping a trailing `\r`. A
/// trailing newline does not yield a final empty line.
fn split_lines(input: &str) -> Vec<&str> {
    // `split('\n')` yields `newlines + 1` items; reserve exactly that so the Vec
    // never reallocates while filling (a big win for many-line documents).
    let cap = memchr::memchr_iter(b'\n', input.as_bytes()).count() + 1;
    let mut out = Vec::with_capacity(cap);
    for line in input.split('\n') {
        out.push(line.strip_suffix('\r').unwrap_or(line));
    }
    // `split('\n')` on "a\n" yields ["a", ""]; drop that trailing empty line so it
    // does not read as a spurious blank.
    if input.ends_with('\n') {
        out.pop();
    }
    out
}

fn collect_refs<'a>(lines: &[&'a str]) -> RefMap<'a> {
    let mut refs = RefMap::new();
    let mut in_fence: Option<(u8, usize)> = None;
    for &line in lines {
        let t = line.trim_start();
        if let Some((ch, len)) = in_fence {
            if is_closing_fence(t, ch, len) {
                in_fence = None;
            }
            continue;
        }
        if let Some((ch, len)) = open_fence(t) {
            in_fence = Some((ch, len));
            continue;
        }
        if let Some((label, def)) = parse_link_def(line) {
            // First definition wins (CommonMark).
            refs.entry(label).or_insert(def);
        }
    }
    refs
}

struct BlockParser<'a, 'r> {
    lines: &'r [&'a str],
    pos: usize,
    refs: &'r RefMap<'a>,
}

impl<'a> BlockParser<'a, '_> {
    fn parse_blocks(&mut self) -> Vec<Block<'a>> {
        let mut blocks = Vec::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if line.trim().is_empty() {
                self.pos += 1;
                continue;
            }
            let (indent, rest) = split_indent(line);

            // Indented code block (4+ spaces) — only when not interrupting a
            // paragraph (handled by the paragraph branch's lookahead).
            if indent >= 4 {
                blocks.push(self.parse_indented_code());
                continue;
            }
            if let Some((ch, len)) = open_fence(rest) {
                blocks.push(self.parse_fenced_code(ch, len));
                continue;
            }
            if let Some((level, content)) = atx_heading(rest) {
                blocks.push(Block::Heading {
                    level,
                    content: parse_inlines(content, self.refs),
                });
                self.pos += 1;
                continue;
            }
            if is_thematic_break(rest) {
                blocks.push(Block::Rule);
                self.pos += 1;
                continue;
            }
            if rest.starts_with('>') {
                blocks.push(self.parse_blockquote());
                continue;
            }
            if list_marker(line).is_some() {
                blocks.push(self.parse_list());
                continue;
            }
            if self.table_starts_here() {
                blocks.push(self.parse_table());
                continue;
            }
            // A link-definition line at block start is consumed (already collected).
            if parse_link_def(line).is_some() {
                self.pos += 1;
                continue;
            }
            blocks.push(self.parse_paragraph());
        }
        blocks
    }

    fn parse_indented_code(&mut self) -> Block<'a> {
        let mut literal = String::new();
        let mut pending_blanks = 0usize;
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if line.trim().is_empty() {
                pending_blanks += 1;
                self.pos += 1;
                continue;
            }
            let (indent, _) = split_indent(line);
            if indent < 4 {
                break;
            }
            // Flush blank lines that were *inside* the block.
            for _ in 0..pending_blanks {
                literal.push('\n');
            }
            pending_blanks = 0;
            if !literal.is_empty() {
                literal.push('\n');
            }
            literal.push_str(strip_indent(line, 4));
            self.pos += 1;
        }
        Block::CodeBlock {
            info: Cow::Borrowed(""),
            indented: true,
            literal,
        }
    }

    fn parse_fenced_code(&mut self, ch: u8, len: usize) -> Block<'a> {
        let open = self.lines[self.pos];
        let (indent, rest) = split_indent(open);
        let info_raw = rest[len..].trim();
        // The info string ends at the first space (the rest is ignored); unescape
        // backslash escapes per CommonMark.
        let info = info_raw.split_whitespace().next().unwrap_or("");
        self.pos += 1;
        let mut literal = String::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if is_closing_fence(line.trim_start(), ch, len) {
                self.pos += 1;
                break;
            }
            if !literal.is_empty() {
                literal.push('\n');
            }
            // Strip up to `indent` leading spaces (fence indentation).
            literal.push_str(strip_indent(line, indent));
            self.pos += 1;
        }
        Block::CodeBlock {
            info: unescape_cow(info),
            indented: false,
            literal,
        }
    }

    fn parse_blockquote(&mut self) -> Block<'a> {
        let mut inner: Vec<&'a str> = Vec::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            let (indent, rest) = split_indent(line);
            if indent < 4 && rest.starts_with('>') {
                // Strip `>` and one optional following space.
                let after = &rest[1..];
                inner.push(after.strip_prefix(' ').unwrap_or(after));
                self.pos += 1;
            } else if line.trim().is_empty() {
                break;
            } else if !is_block_start(line) {
                // Lazy continuation of the quote's paragraph.
                inner.push(line);
                self.pos += 1;
            } else {
                break;
            }
        }
        let mut sub = BlockParser {
            lines: &inner,
            pos: 0,
            refs: self.refs,
        };
        Block::Quote(sub.parse_blocks())
    }

    fn parse_list(&mut self) -> Block<'a> {
        let first = list_marker(self.lines[self.pos]).expect("caller checked");
        let start = if first.ordered {
            Some(first.start)
        } else {
            None
        };
        let mut items: Vec<ListItem<'a>> = Vec::new();
        let mut tight = true;
        // One scratch buffer reused for every item: the sub-parser borrows it only
        // for the duration of `parse_blocks` (the resulting `Block`s borrow the
        // original source, not this Vec), so it can be cleared and refilled per item
        // instead of allocating a fresh Vec each time.
        let mut item_lines: Vec<&'a str> = Vec::new();

        while let Some(marker) = list_marker(self.lines.get(self.pos).copied().unwrap_or("")) {
            if marker.ordered != first.ordered || marker.bullet != first.bullet {
                break; // a different list kind starts a new list
            }
            // Gather this item's lines, stripping `marker.pad` leading columns.
            let first_line = self.lines[self.pos];
            item_lines.clear();
            item_lines.push(&first_line[marker.content_byte..]);
            self.pos += 1;
            let mut internal_blank = false;
            let mut trailing_blanks = 0usize;
            while self.pos < self.lines.len() {
                let line = self.lines[self.pos];
                if line.trim().is_empty() {
                    trailing_blanks += 1;
                    item_lines.push("");
                    self.pos += 1;
                    continue;
                }
                let (indent, _) = split_indent(line);
                if indent >= marker.pad {
                    if trailing_blanks > 0 {
                        internal_blank = true;
                    }
                    trailing_blanks = 0;
                    item_lines.push(strip_indent(line, marker.pad));
                    self.pos += 1;
                } else if trailing_blanks == 0 && !is_block_start(line) {
                    // Lazy paragraph continuation.
                    item_lines.push(line);
                    self.pos += 1;
                } else {
                    break;
                }
            }
            // Drop trailing blank lines from the item (they belong between items).
            while item_lines.last().is_some_and(|l| l.is_empty()) {
                item_lines.pop();
            }
            if trailing_blanks > 0
                && list_marker(self.lines.get(self.pos).copied().unwrap_or("")).is_some()
            {
                tight = false; // blank line between items ⇒ loose list
            }
            if internal_blank {
                tight = false;
            }

            let task = split_task_marker(&mut item_lines);
            let mut sub = BlockParser {
                lines: &item_lines,
                pos: 0,
                refs: self.refs,
            };
            items.push(ListItem {
                task,
                blocks: sub.parse_blocks(),
            });
        }
        Block::List {
            start,
            tight,
            items,
        }
    }

    fn table_starts_here(&self) -> bool {
        let header = self.lines[self.pos];
        let Some(delim) = self.lines.get(self.pos + 1) else {
            return false;
        };
        header.contains('|') && is_table_delimiter(delim)
    }

    fn parse_table(&mut self) -> Block<'a> {
        let header = self.lines[self.pos];
        let delim = self.lines[self.pos + 1];
        let alignments = table_alignments(delim);
        let head: Vec<Vec<Inline<'a>>> = split_table_row(header)
            .into_iter()
            .map(|c| parse_inlines(c, self.refs))
            .collect();
        self.pos += 2;
        let mut rows = Vec::new();
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if line.trim().is_empty() || !line.contains('|') {
                break;
            }
            let cells = split_table_row(line)
                .into_iter()
                .map(|c| parse_inlines(c, self.refs))
                .collect();
            rows.push(cells);
            self.pos += 1;
        }
        Block::Table {
            alignments,
            head,
            rows,
        }
    }

    fn parse_paragraph(&mut self) -> Block<'a> {
        // Fast path: a single-line paragraph (the common case) parses its inline
        // content *borrowed* straight from the source — no join buffer, no owning
        // copy. Continuation lines are only joined into an owned buffer if they
        // actually exist (`joined` stays `None` until the second line).
        let first_line = self.lines[self.pos].trim_start();
        self.pos += 1;
        let mut joined: Option<String> = None;
        while self.pos < self.lines.len() {
            let line = self.lines[self.pos];
            if line.trim().is_empty() {
                break;
            }
            // A setext underline turns the paragraph so far into a heading.
            if let Some(level) = setext_underline(line) {
                self.pos += 1;
                return Block::Heading {
                    level,
                    content: self.paragraph_inlines(first_line, &joined),
                };
            }
            // An interrupting block (incl. a GFM table) ends the paragraph.
            if is_paragraph_interrupter(line) || self.table_starts_here() {
                break;
            }
            let buf = joined.get_or_insert_with(|| first_line.to_string());
            buf.push('\n');
            buf.push_str(line.trim_start());
            self.pos += 1;
        }
        Block::Paragraph(self.paragraph_inlines(first_line, &joined))
    }

    /// Inline-parse a paragraph: borrow from the single source line when there were
    /// no continuations, else own the joined buffer (whose slices cannot escape).
    fn paragraph_inlines(&self, first_line: &'a str, joined: &Option<String>) -> Vec<Inline<'a>> {
        match joined {
            Some(text) => parse_inlines_owned(text, self.refs),
            None => parse_inlines(first_line, self.refs),
        }
    }
}

/// Parse inline content from a transient line-joined buffer. Paragraph (and setext
/// heading) text is built by joining source lines into a local `String`, so any
/// slice borrowed from it would dangle — we parse, then deep-own every node so the
/// result outlives the buffer. (Headings/tables/list text parse the source slice
/// directly and stay zero-copy.)
fn parse_inlines_owned<'a>(text: &str, refs: &RefMap<'a>) -> Vec<Inline<'a>> {
    parse_inlines(text, refs)
        .into_iter()
        .map(own_inline)
        .collect()
}

fn own_inline<'a>(n: Inline<'_>) -> Inline<'a> {
    match n {
        Inline::Text(t) => Inline::Text(Cow::Owned(t.into_owned())),
        Inline::Code(t) => Inline::Code(Cow::Owned(t.into_owned())),
        Inline::Emph(c) => Inline::Emph(c.into_iter().map(own_inline).collect()),
        Inline::Strong(c) => Inline::Strong(c.into_iter().map(own_inline).collect()),
        Inline::Strike(c) => Inline::Strike(c.into_iter().map(own_inline).collect()),
        Inline::Link {
            dest,
            title,
            content,
        } => Inline::Link {
            dest: Cow::Owned(dest.into_owned()),
            title: Cow::Owned(title.into_owned()),
            content: content.into_iter().map(own_inline).collect(),
        },
        Inline::Image { dest, title, alt } => Inline::Image {
            dest: Cow::Owned(dest.into_owned()),
            title: Cow::Owned(title.into_owned()),
            alt,
        },
        Inline::Math { content, display } => Inline::Math {
            content: Cow::Owned(content.into_owned()),
            display,
        },
        Inline::SoftBreak => Inline::SoftBreak,
        Inline::HardBreak => Inline::HardBreak,
    }
}

// ---- line classifiers ----------------------------------------------------------

/// Leading-whitespace width (tabs count as 4) and the rest of the line.
fn split_indent(line: &str) -> (usize, &str) {
    let mut width = 0usize;
    let mut bytes = 0usize;
    for &b in line.as_bytes() {
        match b {
            b' ' => width += 1,
            b'\t' => width += 4 - (width % 4),
            _ => break,
        }
        bytes += 1;
    }
    (width, &line[bytes..])
}

/// Strip up to `cols` columns of leading whitespace (spaces/tabs).
fn strip_indent(line: &str, cols: usize) -> &str {
    let mut width = 0usize;
    let mut bytes = 0usize;
    for &b in line.as_bytes() {
        if width >= cols {
            break;
        }
        match b {
            b' ' => width += 1,
            b'\t' => width += 4,
            _ => break,
        }
        bytes += 1;
    }
    &line[bytes..]
}

/// An opening code fence (`` ``` `` or `~~~`, 3+ chars) — returns `(char, len)`.
/// `trimmed` must already have its leading indentation removed.
pub(crate) fn open_fence(trimmed: &str) -> Option<(u8, usize)> {
    let b = trimmed.as_bytes().first().copied()?;
    if b != b'`' && b != b'~' {
        return None;
    }
    let len = trimmed.bytes().take_while(|&c| c == b).count();
    if len < 3 {
        return None;
    }
    // A ``` fence's info string may not contain a backtick.
    if b == b'`' && trimmed[len..].contains('`') {
        return None;
    }
    Some((b, len))
}

pub(crate) fn is_closing_fence(trimmed: &str, ch: u8, len: usize) -> bool {
    let run = trimmed.bytes().take_while(|&c| c == ch).count();
    run >= len && trimmed[run..].trim().is_empty()
}

fn atx_heading(rest: &str) -> Option<(u8, &str)> {
    let level = rest.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let after = &rest[level..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    // Strip an optional closing run of `#` (with surrounding spaces).
    let content = after.trim();
    let content = content.trim_end_matches('#').trim_end();
    Some((level as u8, content))
}

fn setext_underline(line: &str) -> Option<u8> {
    let (indent, rest) = split_indent(line);
    if indent >= 4 {
        return None;
    }
    let rest = rest.trim_end();
    if rest.is_empty() {
        return None;
    }
    let b = rest.as_bytes()[0];
    if (b == b'=' || b == b'-') && rest.bytes().all(|c| c == b) {
        Some(if b == b'=' { 1 } else { 2 })
    } else {
        None
    }
}

fn is_thematic_break(rest: &str) -> bool {
    let mut chars = rest.chars().filter(|c| !c.is_whitespace());
    let first = match chars.next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    let mut count = 1usize;
    for c in chars {
        if c != first {
            return false;
        }
        count += 1;
    }
    count >= 3
}

/// A parsed list marker.
struct Marker {
    ordered: bool,
    bullet: u8,
    start: u64,
    /// Column width of `marker + following spaces` — continuation indent.
    pad: usize,
    /// Byte offset in the *original* line where the item content begins.
    content_byte: usize,
}

fn list_marker(line: &str) -> Option<Marker> {
    let (indent, rest) = split_indent(line);
    if indent >= 4 {
        return None;
    }
    let indent_bytes = line.len() - rest.len();
    let bytes = rest.as_bytes();
    // Bullet: `-`/`+`/`*` then a space or end-of-line.
    if let Some(&b) = bytes.first() {
        if matches!(b, b'-' | b'+' | b'*') {
            // `* * *` etc. are thematic breaks, not single-item lists.
            if is_thematic_break(rest) {
                return None;
            }
            let after = &rest[1..];
            let (spaces, content_rel) = leading_spaces(after);
            if spaces == 0 && !after.is_empty() {
                return None; // marker must be followed by a space (or be an empty item)
            }
            let spaces = spaces.max(1);
            return Some(Marker {
                ordered: false,
                bullet: b,
                start: 0,
                pad: indent + 1 + spaces,
                content_byte: indent_bytes + 1 + content_rel,
            });
        }
    }
    // Ordered: 1–9 digits then `.`/`)` then space/EOL.
    let digits = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let delim = *bytes.get(digits)?;
    if delim != b'.' && delim != b')' {
        return None;
    }
    let after = &rest[digits + 1..];
    let (spaces, content_rel) = leading_spaces(after);
    if spaces == 0 && !after.is_empty() {
        return None;
    }
    let spaces = spaces.max(1);
    let start = rest[..digits].parse().unwrap_or(0);
    Some(Marker {
        ordered: true,
        bullet: delim,
        start,
        pad: indent + digits + 1 + spaces,
        content_byte: indent_bytes + digits + 1 + content_rel,
    })
}

/// Count leading spaces (cap at 4 — more is an indented code block inside the item)
/// and return `(count, byte-offset of content)`.
fn leading_spaces(s: &str) -> (usize, usize) {
    let raw = s.bytes().take_while(|&b| b == b' ').count();
    if raw > 4 {
        (1, 1)
    } else {
        (raw, raw)
    }
}

/// If the item's first paragraph begins with a GFM task checkbox, strip it and
/// return `Some(checked)`.
fn split_task_marker(lines: &mut [&str]) -> Option<bool> {
    let first = lines.first_mut()?;
    let t = first.trim_start();
    let (checked, rest) =
        if let Some(r) = t.strip_prefix("[ ] ").or_else(|| t.strip_prefix("[ ]\t")) {
            (false, r)
        } else {
            let r = t.strip_prefix("[x] ").or_else(|| t.strip_prefix("[X] "))?;
            (true, r)
        };
    *first = rest;
    Some(checked)
}

fn is_block_start(line: &str) -> bool {
    let (indent, rest) = split_indent(line);
    if indent >= 4 {
        return false;
    }
    open_fence(rest).is_some()
        || atx_heading(rest).is_some()
        || is_thematic_break(rest)
        || rest.starts_with('>')
        || list_marker(line).is_some()
}

/// Constructs that interrupt an open paragraph. Per CommonMark a list marker
/// interrupts only when its item is non-empty (and, if ordered, starts at 1);
/// tables interrupt via their delimiter row, handled by the caller.
fn is_paragraph_interrupter(line: &str) -> bool {
    let (indent, rest) = split_indent(line);
    if indent >= 4 {
        return false;
    }
    open_fence(rest).is_some()
        || atx_heading(rest).is_some()
        || is_thematic_break(rest)
        || rest.starts_with('>')
        || list_interrupts_paragraph(line)
}

fn list_interrupts_paragraph(line: &str) -> bool {
    let Some(m) = list_marker(line) else {
        return false;
    };
    if m.ordered && m.start != 1 {
        return false;
    }
    !line[m.content_byte..].trim().is_empty()
}

fn parse_link_def(line: &str) -> Option<(String, LinkDef<'static>)> {
    let (indent, rest) = split_indent(line);
    if indent >= 4 || !rest.starts_with('[') {
        return None;
    }
    let close = rest.find("]:")?;
    let label = &rest[1..close];
    if label.is_empty() || label.contains('[') {
        return None;
    }
    let after = rest[close + 2..].trim();
    if after.is_empty() {
        return None;
    }
    let mut parts = after.splitn(2, char::is_whitespace);
    let dest = parts.next()?.trim_matches(|c| c == '<' || c == '>');
    let title = parts
        .next()
        .map(|t| {
            t.trim()
                .trim_matches(|c| c == '"' || c == '\'' || c == '(' || c == ')')
        })
        .unwrap_or("");
    Some((
        normalize_label(label),
        LinkDef {
            dest: Cow::Owned(dest.to_string()),
            title: Cow::Owned(title.to_string()),
        },
    ))
}

// ---- table helpers -------------------------------------------------------------

fn is_table_delimiter(line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() || !t.bytes().all(|b| matches!(b, b'|' | b'-' | b':' | b' ')) {
        return false;
    }
    // Must contain at least one `-` and split into ≥1 column each matching `:?-+:?`.
    let cols = split_table_row(line);
    !cols.is_empty()
        && cols.iter().all(|c| {
            let c = c.trim();
            let body = c.trim_start_matches(':').trim_end_matches(':');
            !body.is_empty() && body.bytes().all(|b| b == b'-')
        })
}

fn table_alignments(delim: &str) -> Vec<Alignment> {
    split_table_row(delim)
        .into_iter()
        .map(|c| {
            let c = c.trim();
            let left = c.starts_with(':');
            let right = c.ends_with(':');
            match (left, right) {
                (true, true) => Alignment::Center,
                (true, false) => Alignment::Left,
                (false, true) => Alignment::Right,
                (false, false) => Alignment::None,
            }
        })
        .collect()
}

/// Split a `|`-delimited table row into trimmed cell sources, honouring `\|`
/// escapes and ignoring leading/trailing pipes.
fn split_table_row(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let bytes = t.as_bytes();
    let mut cells = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'|' => {
                cells.push(t[start..i].trim());
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    cells.push(t[start..].trim());
    cells
}

// ---- misc ----------------------------------------------------------------------

fn unescape_cow(s: &str) -> Cow<'_, str> {
    if s.as_bytes().contains(&b'\\') {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.clone().next() {
                    if n.is_ascii_punctuation() {
                        out.push(n);
                        chars.next();
                        continue;
                    }
                }
            }
            out.push(c);
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(s)
    }
}
