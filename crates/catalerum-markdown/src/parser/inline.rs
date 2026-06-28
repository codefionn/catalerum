//! Inline parsing: a span of text → a tree of [`Inline`] nodes.
//!
//! Order of operations follows CommonMark: code spans, autolinks, backslash
//! escapes and line breaks bind tighter than emphasis, and links/images are
//! resolved by bracket matching. Emphasis (`*`, `_`) and GFM strikethrough
//! (`~~`) are resolved by the spec's delimiter-stack procedure
//! ([`process_emphasis`]) so nested and mixed runs (`**a _b_ c**`) come out right.
//! GFM's extended (bare) autolinks — `http(s)://…` / `www.…` URLs and bare
//! `local@domain` emails typed directly in text — are a final post-pass
//! ([`autolink_nodes`]) over the flattened tree, so they cost nothing on spans with
//! no autolink and don't burden the hot byte scanner.
//!
//! Raw inline HTML is **not** honoured — a `<` that is not a valid autolink is
//! literal text — which is what keeps the rendered output injection-safe.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::ast::Inline;
use crate::scan::ByteSet;

/// Bytes that can *begin* inline markup or a line break; the scanner skips straight
/// to the next one. (`&` is absent — we do not decode entities, so it is ordinary
/// text the renderer escapes. `]` is absent — it only ever closes a `[…]`, which
/// `match_brackets` scans for itself; a stray `]` is literal text.) `$` begins
/// LaTeX math.
static INLINE: ByteSet = ByteSet::new(b"\n\r\\`*_[!<~$");

/// A resolved reference link/image definition (`[label]: dest "title"`).
#[derive(Debug, Clone)]
pub(crate) struct LinkDef<'a> {
    pub(crate) dest: Cow<'a, str>,
    pub(crate) title: Cow<'a, str>,
}

/// Map from normalised reference label → definition.
pub(crate) type RefMap<'a> = HashMap<String, LinkDef<'a>>;

/// Normalise a reference label: trim, l-casefold, collapse internal whitespace.
pub(crate) fn normalize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_space = false;
    for ch in label.trim().chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            prev_space = false;
            out.extend(ch.to_lowercase());
        }
    }
    out
}

/// A working token: either a finished inline node or an unresolved emphasis run.
enum Tok<'a> {
    Node(Inline<'a>),
    Delim(Delim),
}

/// An unresolved run of emphasis delimiters (`*`, `_`, or a `~~` strike run).
struct Delim {
    ch: u8,
    count: usize,
    can_open: bool,
    can_close: bool,
}

/// Parse `text` into inline nodes, resolving references against `refs`.
pub(crate) fn parse_inlines<'a>(text: &'a str, refs: &RefMap<'a>) -> Vec<Inline<'a>> {
    // Fast path: a span with no inline-significant byte (no `*_`[`]`<~\\`, no line
    // break) is one borrowed text node — skip the tokenize + flatten Vecs entirely.
    // This is the common case (plain words in paragraphs, headings, list items, cells).
    if text.is_empty() {
        return Vec::new();
    }
    if INLINE.find(text.as_bytes()).is_none() {
        // Plain run: still split out bare URLs (the only inline feature whose
        // trigger bytes aren't in the INLINE set), but allocate only when one hits.
        if has_autolink_signal(text) {
            if let Some(pieces) = scan_autolinks(text) {
                let mut out = Vec::new();
                emit_pieces(
                    &pieces,
                    text,
                    &mut |a, e| Cow::Borrowed(&text[a..e]),
                    &mut out,
                );
                return out;
            }
        }
        return vec![Inline::Text(Cow::Borrowed(text))];
    }
    autolink_nodes(parse_inlines_plain(text, refs))
}

/// Inline parse **without** bare-autolink expansion — for link/image labels, where
/// CommonMark forbids a link inside a link (so a bare URL in the label stays text).
fn parse_inlines_plain<'a>(text: &'a str, refs: &RefMap<'a>) -> Vec<Inline<'a>> {
    if text.is_empty() {
        return Vec::new();
    }
    if INLINE.find(text.as_bytes()).is_none() {
        return vec![Inline::Text(Cow::Borrowed(text))];
    }
    let mut toks = tokenize(text, refs);
    process_emphasis(&mut toks);
    flatten(toks.into_iter())
}

/// The flattened plain-text of an inline span (used for image `alt` and for
/// reference labels derived from link text).
pub(crate) fn inline_text(nodes: &[Inline<'_>]) -> String {
    let mut out = String::new();
    collect_text(nodes, &mut out);
    out
}

/// A GitHub-style anchor slug for a heading: the plain text lowercased, with
/// punctuation dropped, whitespace runs collapsed to a single `-`, and `-`/`_`
/// kept. Stateless (no `-1`/`-2` de-duplication) so batch and streaming renders
/// agree byte-for-byte. Returns an empty string for a heading with no slug chars.
pub(crate) fn heading_slug(content: &[Inline<'_>]) -> String {
    let text = inline_text(content);
    let mut slug = String::with_capacity(text.len());
    let mut pending_dash = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.extend(ch.to_lowercase());
        } else if ch == '-' || ch == '_' {
            if pending_dash {
                slug.push('-');
                pending_dash = false;
            }
            slug.push(ch);
        } else if ch.is_whitespace() {
            // Defer the separator so trailing whitespace/punctuation leaves no `-`.
            pending_dash = !slug.is_empty();
        }
        // Other punctuation is dropped entirely.
    }
    slug
}

fn collect_text(nodes: &[Inline<'_>], out: &mut String) {
    for n in nodes {
        match n {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Emph(c) | Inline::Strong(c) | Inline::Strike(c) => collect_text(c, out),
            Inline::Link { content, .. } => collect_text(content, out),
            Inline::Image { alt, .. } => out.push_str(alt),
            Inline::Math { content, .. } => out.push_str(content),
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
}

fn tokenize<'a>(text: &'a str, refs: &RefMap<'a>) -> Vec<Tok<'a>> {
    let bytes = text.as_bytes();
    let mut toks: Vec<Tok<'a>> = Vec::new();
    let mut i = 0usize;
    // Start of the current pending plain-text run (flushed lazily).
    let mut run_start = 0usize;

    let flush = |toks: &mut Vec<Tok<'a>>, from: usize, to: usize| {
        if to > from {
            toks.push(Tok::Node(Inline::Text(Cow::Borrowed(&text[from..to]))));
        }
    };

    while i < bytes.len() {
        let rel = match INLINE.find(&bytes[i..]) {
            Some(r) => r,
            None => break,
        };
        let at = i + rel;
        let b = bytes[at];
        match b {
            b'\n' | b'\r' => {
                // A line ending inside an inline span is a soft break — or a *hard*
                // break when the line ended with two+ spaces or a backslash. CR/CRLF
                // are normalised away (paragraph lines arrive `\n`-joined, but be
                // defensive). The trailing spaces / backslash marker are trimmed.
                let bs = backslash_before(bytes, at) && at > run_start;
                let cut = if bs {
                    at - 1
                } else {
                    trim_trailing_spaces(text, run_start, at)
                };
                let hard = bs || ends_with_two_spaces(text, run_start, at);
                flush(&mut toks, run_start, cut);
                toks.push(Tok::Node(if hard {
                    Inline::HardBreak
                } else {
                    Inline::SoftBreak
                }));
                i = at + 1;
                if b == b'\r' && bytes.get(i) == Some(&b'\n') {
                    i += 1;
                }
                run_start = i;
            }
            b'\\' => {
                // Backslash escape: a following ASCII punctuation becomes literal.
                if let Some(&next) = bytes.get(at + 1) {
                    if next.is_ascii_punctuation() {
                        flush(&mut toks, run_start, at);
                        toks.push(Tok::Node(Inline::Text(Cow::Owned(
                            (next as char).to_string(),
                        ))));
                        i = at + 2;
                        run_start = i;
                        continue;
                    }
                    if next == b'\n' {
                        flush(&mut toks, run_start, at);
                        toks.push(Tok::Node(Inline::HardBreak));
                        i = at + 2;
                        run_start = i;
                        continue;
                    }
                }
                // Lone backslash → literal; consume it as text so the scanner moves on.
                flush(&mut toks, run_start, at + 1);
                i = at + 1;
                run_start = i;
            }
            b'`' => {
                let n = backtick_run(bytes, at);
                if let Some((content, end)) = code_span(text, at, n) {
                    flush(&mut toks, run_start, at);
                    toks.push(Tok::Node(Inline::Code(content)));
                    i = end;
                    run_start = i;
                } else {
                    // No closer: the backticks are literal; skip the run.
                    i = at + n;
                }
            }
            b'<' => {
                if let Some((node, end)) = autolink(text, at) {
                    flush(&mut toks, run_start, at);
                    toks.push(Tok::Node(node));
                    i = end;
                    run_start = i;
                } else {
                    i = at + 1;
                }
            }
            b'$' => {
                let display = bytes.get(at + 1) == Some(&b'$');
                let dlen = if display { 2 } else { 1 };
                let cstart = at + dlen;
                // Inline `$…$` must not open on whitespace (avoids "$ x"); display
                // `$$…$$` has no such ambiguity.
                let open_ok =
                    display || matches!(bytes.get(cstart), Some(b) if !b.is_ascii_whitespace());
                let close = if open_ok {
                    find_math_close(bytes, cstart, display)
                } else {
                    None
                };
                if let Some(cend) = close {
                    flush(&mut toks, run_start, at);
                    toks.push(Tok::Node(Inline::Math {
                        content: Cow::Borrowed(&text[cstart..cend]),
                        display,
                    }));
                    i = cend + dlen;
                    run_start = i;
                } else {
                    // Not math (e.g. a currency `$`) — leave it in the text run.
                    i = at + 1;
                }
            }
            b'!' => {
                if bytes.get(at + 1) == Some(&b'[') {
                    if let Some((node, end)) = link_or_image(text, at, true, refs) {
                        flush(&mut toks, run_start, at);
                        toks.push(Tok::Node(node));
                        i = end;
                        run_start = i;
                        continue;
                    }
                }
                i = at + 1;
            }
            b'[' => {
                if let Some((node, end)) = link_or_image(text, at, false, refs) {
                    flush(&mut toks, run_start, at);
                    toks.push(Tok::Node(node));
                    i = end;
                    run_start = i;
                } else {
                    i = at + 1;
                }
            }
            b'*' | b'_' | b'~' => {
                let run = same_run(bytes, at);
                let ch = b;
                if ch == b'~' && run != 2 {
                    // GFM strikethrough is exactly `~~`; any other tilde run is literal.
                    i = at + run;
                    continue;
                }
                flush(&mut toks, run_start, at);
                let before = prev_char(text, at);
                let after = text[at + run..].chars().next();
                let (can_open, can_close) = flanking(ch, before, after);
                toks.push(Tok::Delim(Delim {
                    ch,
                    count: run,
                    can_open,
                    can_close,
                }));
                i = at + run;
                run_start = i;
            }
            _ => unreachable!("scanner only stops on the INLINE set"),
        }
    }
    flush(&mut toks, run_start, bytes.len());
    toks
}

/// CommonMark delimiter-run resolution: walk left→right, and at each closer find
/// the nearest compatible opener, wrapping the span between them in
/// emphasis/strong/strike. Leftover delimiter characters survive as literal text
/// ([`flatten`]).
fn process_emphasis(toks: &mut Vec<Tok<'_>>) {
    let mut closer = 0usize;
    while closer < toks.len() {
        let (cch, c_can_open, ccount) = match &toks[closer] {
            Tok::Delim(d) if d.can_close => (d.ch, d.can_open, d.count),
            _ => {
                closer += 1;
                continue;
            }
        };
        // Nearest opener of the same char before `closer`.
        let mut opener = None;
        let mut k = closer;
        while k > 0 {
            k -= 1;
            if let Tok::Delim(d) = &toks[k] {
                if d.ch == cch
                    && d.can_open
                    && rule_of_three(d.count, ccount, d.can_close, c_can_open)
                {
                    opener = Some(k);
                    break;
                }
            }
        }
        let Some(opener) = opener else {
            closer += 1;
            continue;
        };
        let ocount = match &toks[opener] {
            Tok::Delim(d) => d.count,
            _ => unreachable!(),
        };
        // Strikethrough always consumes the `~~` pair; `*`/`_` form strong when both
        // runs have ≥2 left, else emphasis.
        let used = if cch == b'~' || (ocount >= 2 && ccount >= 2) {
            2
        } else {
            1
        };
        // Wrap the inner tokens, flattening straight from the drain (no intermediate
        // Vec). The drain removes `opener+1..closer`, so the closer slides to
        // `opener+1` afterwards.
        let content = flatten(toks.drain(opener + 1..closer));
        let wrapped = match (cch, used) {
            (b'~', _) => Inline::Strike(content),
            (_, 2) => Inline::Strong(content),
            (_, _) => Inline::Emph(content),
        };
        // After draining, the closer sits right after the opener.
        let closer_idx = opener + 1;
        // Reduce / remove the opener delimiter.
        if let Tok::Delim(d) = &mut toks[opener] {
            d.count -= used;
        }
        let opener_empty = matches!(&toks[opener], Tok::Delim(d) if d.count == 0);
        if let Tok::Delim(d) = &mut toks[closer_idx] {
            d.count -= used;
        }
        let closer_empty = matches!(&toks[closer_idx], Tok::Delim(d) if d.count == 0);

        // Rebuild: [opener?] [wrapped] [closer?]
        let node = Tok::Node(wrapped);
        match (opener_empty, closer_empty) {
            (true, true) => {
                toks.splice(opener..=closer_idx, std::iter::once(node));
                closer = opener; // re-examine from here
            }
            (true, false) => {
                toks.splice(opener..closer_idx, std::iter::once(node));
                // opener replaced by node; closer still has leftover → revisit it.
                closer = opener + 1;
            }
            (false, true) => {
                toks.splice(opener + 1..=closer_idx, std::iter::once(node));
                closer = opener + 1;
            }
            (false, false) => {
                toks.splice(opener + 1..closer_idx, std::iter::once(node));
                closer = opener + 2;
            }
        }
    }
}

/// CommonMark "rule of three": when a delimiter can both open and close, the sum
/// of the two run lengths must not be a multiple of 3 unless both are.
fn rule_of_three(
    open_len: usize,
    close_len: usize,
    opener_can_close: bool,
    closer_can_open: bool,
) -> bool {
    if (opener_can_close || closer_can_open) && (open_len + close_len) % 3 == 0 {
        return open_len % 3 == 0 && close_len % 3 == 0;
    }
    true
}

fn flatten<'a>(toks: impl ExactSizeIterator<Item = Tok<'a>>) -> Vec<Inline<'a>> {
    let mut out: Vec<Inline> = Vec::with_capacity(toks.len());
    for t in toks {
        match t {
            Tok::Node(n) => push_inline(&mut out, n),
            Tok::Delim(d) => {
                let s = (d.ch as char).to_string().repeat(d.count);
                push_inline(&mut out, Inline::Text(Cow::Owned(s)));
            }
        }
    }
    out
}

/// Append `n`, merging adjacent borrowed/owned text where cheap (keeps the node
/// count and per-node escaping overhead down).
fn push_inline<'a>(out: &mut Vec<Inline<'a>>, n: Inline<'a>) {
    if let (Some(Inline::Text(prev)), Inline::Text(cur)) = (out.last_mut(), &n) {
        prev.to_mut().push_str(cur);
        return;
    }
    out.push(n);
}

// ---- low-level inline scanners -------------------------------------------------

fn backtick_run(bytes: &[u8], at: usize) -> usize {
    same_run(bytes, at)
}

/// Find the closing math delimiter starting from `from`, returning the index of
/// the closing `$`/`$$`. For inline (`!display`) the close must not be preceded by
/// whitespace nor followed by a digit (so `$5 and $10` stays currency, not math).
fn find_math_close(bytes: &[u8], from: usize, display: bool) -> Option<usize> {
    let mut j = from;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2, // skip an escaped char (`\$`, `\\`)
            b'$' => {
                if display {
                    if bytes.get(j + 1) == Some(&b'$') {
                        return (j > from).then_some(j);
                    }
                    j += 1;
                } else {
                    let before_ok = j > from && !bytes[j - 1].is_ascii_whitespace();
                    let after_ok = !matches!(bytes.get(j + 1), Some(b) if b.is_ascii_digit());
                    if before_ok && after_ok {
                        return Some(j);
                    }
                    j += 1;
                }
            }
            _ => j += 1,
        }
    }
    None
}

fn same_run(bytes: &[u8], at: usize) -> usize {
    let ch = bytes[at];
    let mut n = 0;
    while bytes.get(at + n) == Some(&ch) {
        n += 1;
    }
    n
}

/// Parse a code span starting at `at` whose opener is `n` backticks. Returns the
/// (content, end-offset-after-closer) or `None` if there is no matching closer.
fn code_span(text: &str, at: usize, n: usize) -> Option<(Cow<'_, str>, usize)> {
    let bytes = text.as_bytes();
    let mut i = at + n;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = same_run(bytes, i);
            if run == n {
                let raw = &text[at + n..i];
                return Some((normalize_code(raw), i + run));
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// CommonMark code-span content normalisation: line endings → spaces, and a single
/// leading+trailing space stripped if the content is not all spaces.
fn normalize_code(raw: &str) -> Cow<'_, str> {
    let needs_nl = raw.bytes().any(|b| b == b'\n' || b == b'\r');
    let collapsed: Cow<str> = if needs_nl {
        // `\r\n` → one space; lone `\n`/`\r` → one space each.
        Cow::Owned(raw.replace("\r\n", " ").replace(['\n', '\r'], " "))
    } else {
        Cow::Borrowed(raw)
    };
    let trimmed = collapsed.starts_with(' ')
        && collapsed.ends_with(' ')
        && collapsed.bytes().any(|b| b != b' ');
    if trimmed {
        Cow::Owned(collapsed[1..collapsed.len() - 1].to_string())
    } else {
        collapsed
    }
}

/// `<scheme:...>` absolute-URI or `<user@host>` email autolink.
fn autolink(text: &str, at: usize) -> Option<(Inline<'_>, usize)> {
    let close = text[at + 1..].find('>')? + at + 1;
    let inner = &text[at + 1..close];
    if inner.is_empty() || inner.bytes().any(|b| b.is_ascii_whitespace() || b == b'<') {
        return None;
    }
    let end = close + 1;
    if is_uri_autolink(inner) {
        return Some((
            Inline::Link {
                dest: Cow::Borrowed(inner),
                title: Cow::Borrowed(""),
                content: vec![Inline::Text(Cow::Borrowed(inner))],
            },
            end,
        ));
    }
    if is_email_autolink(inner) {
        return Some((
            Inline::Link {
                dest: Cow::Owned(format!("mailto:{inner}")),
                title: Cow::Borrowed(""),
                content: vec![Inline::Text(Cow::Borrowed(inner))],
            },
            end,
        ));
    }
    None
}

fn is_uri_autolink(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    let scheme = &s[..colon];
    if scheme.len() < 2 || scheme.len() > 32 {
        return false;
    }
    let mut chars = scheme.chars();
    let first = chars.next().unwrap_or(' ');
    first.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn is_email_autolink(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b))
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

// ---- GFM extended (bare) autolinks ---------------------------------------------

/// A slice of a text node after bare-autolink splitting.
enum Piece {
    Text(usize, usize),
    /// A bare URL span; `bool` is `true` for a `www.` link (dest gets `http://`).
    Url(usize, usize, bool),
    /// A bare email span (dest gets `mailto:`).
    Email(usize, usize),
}

/// Cheap gate: does `s` even hint at a bare URL or email? (avoids the byte scan
/// for the overwhelmingly-common plain text node).
fn has_autolink_signal(s: &str) -> bool {
    s.contains("http") || s.contains("www.") || s.contains('@')
}

/// Rewrite a flattened inline list, turning bare `http(s)://…` / `www.…` URLs and
/// `local@domain` emails in text nodes into [`Inline::Link`]s (GFM's
/// extended-autolink extension). Recurses into emphasis/strong/strike but not link
/// text (no nested autolinks) or code.
fn autolink_nodes<'a>(nodes: Vec<Inline<'a>>) -> Vec<Inline<'a>> {
    // No text node hints at a URL → return the list untouched (no allocation), so
    // the common no-URL span keeps its existing allocation budget.
    if !nodes.iter().any(needs_autolink) {
        return nodes;
    }
    let mut out = Vec::with_capacity(nodes.len());
    for n in nodes {
        match n {
            Inline::Text(cow) => split_text(cow, &mut out),
            Inline::Emph(c) => out.push(Inline::Emph(autolink_nodes(c))),
            Inline::Strong(c) => out.push(Inline::Strong(autolink_nodes(c))),
            Inline::Strike(c) => out.push(Inline::Strike(autolink_nodes(c))),
            other => out.push(other),
        }
    }
    out
}

/// Whether `n` (or a descendant emphasis span) holds text that hints at a URL.
fn needs_autolink(n: &Inline<'_>) -> bool {
    match n {
        Inline::Text(t) => has_autolink_signal(t),
        Inline::Emph(c) | Inline::Strong(c) | Inline::Strike(c) => c.iter().any(needs_autolink),
        _ => false,
    }
}

/// Split a single text node into text/link pieces (or push it back untouched).
fn split_text<'a>(cow: Cow<'a, str>, out: &mut Vec<Inline<'a>>) {
    if !has_autolink_signal(&cow) {
        out.push(Inline::Text(cow));
        return;
    }
    let Some(pieces) = scan_autolinks(&cow) else {
        out.push(Inline::Text(cow));
        return;
    };
    // Emit borrowed slices when the source is borrowed, owned copies otherwise.
    match cow {
        Cow::Borrowed(b) => emit_pieces(&pieces, b, &mut |a, e| Cow::Borrowed(&b[a..e]), out),
        Cow::Owned(o) => emit_pieces(
            &pieces,
            &o,
            &mut |a, e| Cow::Owned(o[a..e].to_string()),
            out,
        ),
    }
}

fn emit_pieces<'a>(
    pieces: &[Piece],
    raw: &str,
    mk: &mut dyn FnMut(usize, usize) -> Cow<'a, str>,
    out: &mut Vec<Inline<'a>>,
) {
    for p in pieces {
        match *p {
            Piece::Text(a, e) => out.push(Inline::Text(mk(a, e))),
            Piece::Url(a, e, www) => {
                let dest = if www {
                    Cow::Owned(format!("http://{}", &raw[a..e]))
                } else {
                    mk(a, e)
                };
                out.push(Inline::Link {
                    dest,
                    title: Cow::Borrowed(""),
                    content: vec![Inline::Text(mk(a, e))],
                });
            }
            Piece::Email(a, e) => out.push(Inline::Link {
                dest: Cow::Owned(format!("mailto:{}", &raw[a..e])),
                title: Cow::Borrowed(""),
                content: vec![Inline::Text(mk(a, e))],
            }),
        }
    }
}

/// Scan `s` for bare autolinks, returning the cover of text/url pieces, or `None`
/// if there are none (so the caller keeps the node as-is).
fn scan_autolinks(s: &str) -> Option<Vec<Piece>> {
    let b = s.as_bytes();
    let mut pieces = Vec::new();
    let mut i = 0;
    let mut text_from = 0;
    while i < s.len() {
        // GFM: an autolink starts at line start, after whitespace, or after `*_~(`.
        let boundary = i == 0
            || matches!(
                b[i - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'*' | b'_' | b'~' | b'('
            );
        if boundary {
            if let Some((end, www)) = match_url(s, i) {
                if i > text_from {
                    pieces.push(Piece::Text(text_from, i));
                }
                pieces.push(Piece::Url(i, end, www));
                i = end;
                text_from = end;
                continue;
            }
            if let Some(end) = match_email(s, i) {
                if i > text_from {
                    pieces.push(Piece::Text(text_from, i));
                }
                pieces.push(Piece::Email(i, end));
                i = end;
                text_from = end;
                continue;
            }
        }
        i += 1;
    }
    if pieces.is_empty() {
        return None;
    }
    if text_from < s.len() {
        pieces.push(Piece::Text(text_from, s.len()));
    }
    Some(pieces)
}

/// Match a bare URL beginning at byte `start`, returning `(end, is_www)`. The host
/// must contain a dot; GFM trailing-punctuation/paren/entity trimming is applied.
fn match_url(s: &str, start: usize) -> Option<(usize, bool)> {
    let rest = s.as_bytes().get(start..)?;
    let (plen, www) = if rest
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"https://"))
    {
        (8, false)
    } else if rest
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"http://"))
    {
        (7, false)
    } else if rest
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"www."))
    {
        (4, true)
    } else {
        return None;
    };
    let b = s.as_bytes();
    let mut end = start + plen;
    while end < s.len() && !b[end].is_ascii_whitespace() && b[end] != b'<' {
        end += 1;
    }
    let end = start + trim_url_end(&s[start..end]);
    if end <= start + plen {
        return None;
    }
    // The authority (up to the first `/ ? # :`) must look like a domain.
    let matched = &s[start..end];
    let after = if www { matched } else { &matched[plen..] };
    let host = after.split(['/', '?', '#', ':']).next().unwrap_or("");
    if !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
        return None;
    }
    Some((end, www))
}

/// Match a bare email (`local@domain.tld`) beginning at byte `start`, returning the
/// end byte. GFM's extended email rule: a `[a-zA-Z0-9._+-]+` local part, then `@`,
/// then a dotted domain of alphanumerics/`-`/`_`; trailing `.`/`-`/`_` are trimmed
/// and the domain must keep a dot and not end on `-`/`_`. Conservative on purpose —
/// a bare `@handle` (no local part) or dotless host is left as text.
fn match_email(s: &str, start: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = start;
    while i < s.len() && is_email_local(b[i]) {
        i += 1;
    }
    if i == start || b.get(i) != Some(&b'@') {
        return None;
    }
    let domain_start = i + 1;
    let mut end = domain_start;
    while end < s.len() && (b[end].is_ascii_alphanumeric() || matches!(b[end], b'.' | b'-' | b'_'))
    {
        end += 1;
    }
    // Drop trailing separators (`foo@bar.com.` / `…com-`), then validate the domain.
    while end > domain_start && matches!(b[end - 1], b'.' | b'-' | b'_') {
        end -= 1;
    }
    let domain = s.get(domain_start..end)?;
    if !domain.contains('.') || domain.starts_with('.') {
        return None;
    }
    Some(end)
}

/// GFM email local-part bytes (the conservative extended-autolink set).
fn is_email_local(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-')
}

/// GFM trailing trim: drop trailing `?!.,:*_~'"`, unbalanced `)`, and a trailing
/// `&entity;`, repeating until stable. Returns the kept byte length of `u`.
fn trim_url_end(u: &str) -> usize {
    let b = u.as_bytes();
    let mut end = u.len();
    loop {
        let before = end;
        while end > 0
            && matches!(
                b[end - 1],
                b'?' | b'!' | b'.' | b',' | b':' | b'*' | b'_' | b'~' | b'\'' | b'"'
            )
        {
            end -= 1;
        }
        if end > 0 && b[end - 1] == b')' {
            let opens = b[..end].iter().filter(|&&c| c == b'(').count();
            let closes = b[..end].iter().filter(|&&c| c == b')').count();
            if closes > opens {
                end -= 1;
            }
        }
        if end > 0 && b[end - 1] == b';' {
            if let Some(amp) = u[..end].rfind('&') {
                let ent = &u[amp + 1..end - 1];
                if !ent.is_empty() && ent.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'#') {
                    end = amp;
                }
            }
        }
        if end == before {
            return end;
        }
    }
}

/// Parse `[text](dest)` / `[text][ref]` / `[ref]` (and image `![…]` variants)
/// starting at `at` (the `[` or `!`). Returns the node + end offset, or `None`.
fn link_or_image<'a>(
    text: &'a str,
    at: usize,
    image: bool,
    refs: &RefMap<'a>,
) -> Option<(Inline<'a>, usize)> {
    let bracket = if image { at + 1 } else { at };
    let (inner, after_label) = match_brackets(text, bracket)?;
    let bytes = text.as_bytes();

    // Inline destination: `(dest "title")`.
    if bytes.get(after_label) == Some(&b'(') {
        if let Some((dest, title, end)) = inline_destination(text, after_label) {
            return Some((build(image, inner, dest, title, refs), end));
        }
    }
    // Full reference: `[label][ref]`.
    if bytes.get(after_label) == Some(&b'[') {
        if let Some((reflabel, end)) = match_brackets(text, after_label) {
            let key = if text[after_label + 1..end - 1].trim().is_empty() {
                normalize_label(inner) // collapsed `[text][]`
            } else {
                normalize_label(&text[after_label + 1..end - 1])
            };
            let _ = reflabel;
            if let Some(def) = refs.get(&key) {
                return Some((
                    build(image, inner, def.dest.clone(), def.title.clone(), refs),
                    end,
                ));
            }
            return None;
        }
    }
    // Shortcut reference: `[ref]`.
    let key = normalize_label(inner);
    if let Some(def) = refs.get(&key) {
        return Some((
            build(image, inner, def.dest.clone(), def.title.clone(), refs),
            after_label,
        ));
    }
    None
}

fn build<'a>(
    image: bool,
    inner: &'a str,
    dest: Cow<'a, str>,
    title: Cow<'a, str>,
    refs: &RefMap<'a>,
) -> Inline<'a> {
    if image {
        Inline::Image {
            dest,
            title,
            alt: inline_text(&parse_inlines_plain(inner, refs)),
        }
    } else {
        Inline::Link {
            dest,
            title,
            content: parse_inlines_plain(inner, refs),
        }
    }
}

/// Given the index of an opening `[`, return `(label, index-after-closing-`]`)`,
/// respecting backslash escapes and nested brackets.
fn match_brackets(text: &str, open: usize) -> Option<(&str, usize)> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[open], b'[');
    let mut depth = 1usize;
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&text[open + 1..i], i + 1));
                }
                i += 1;
            }
            b'`' => {
                // Skip code spans so brackets inside them don't count.
                let n = same_run(bytes, i);
                if let Some((_, end)) = code_span(text, i, n) {
                    i = end;
                } else {
                    i += n;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// Parse `(dest "title")` beginning at the `(`. Returns `(dest, title, end)`.
fn inline_destination(text: &str, open: usize) -> Option<(Cow<'_, str>, Cow<'_, str>, usize)> {
    let bytes = text.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut i = open + 1;
    // optional whitespace
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let dest;
    if bytes.get(i) == Some(&b'<') {
        // `<...>` destination: no spaces, `>` terminates.
        let close = text[i + 1..].find('>')? + i + 1;
        dest = Cow::Borrowed(&text[i + 1..close]);
        i = close + 1;
    } else {
        let start = i;
        let mut paren = 0i32;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'(' => {
                    paren += 1;
                    i += 1;
                }
                b')' => {
                    if paren == 0 {
                        break;
                    }
                    paren -= 1;
                    i += 1;
                }
                b if b.is_ascii_whitespace() => break,
                _ => i += 1,
            }
        }
        dest = Cow::Borrowed(text.get(start..i)?);
    }
    // optional whitespace before title / close
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let mut title = Cow::Borrowed("");
    if let Some(&q) = bytes.get(i) {
        if matches!(q, b'"' | b'\'' | b'(') {
            let close_ch = if q == b'(' { b')' } else { q };
            let tstart = i + 1;
            let rel = text[tstart..].find(close_ch as char)?;
            title = Cow::Borrowed(&text[tstart..tstart + rel]);
            i = tstart + rel + 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
        }
    }
    if bytes.get(i) == Some(&b')') {
        Some((dest, title, i + 1))
    } else {
        None
    }
}

// ---- flanking + small helpers --------------------------------------------------

fn flanking(ch: u8, before: Option<char>, after: Option<char>) -> (bool, bool) {
    let before_ws = before.is_none_or(char_is_ws);
    let after_ws = after.is_none_or(char_is_ws);
    let before_punct = before.is_some_and(char_is_punct);
    let after_punct = after.is_some_and(char_is_punct);

    let left_flanking = !after_ws && (!after_punct || before_ws || before_punct);
    let right_flanking = !before_ws && (!before_punct || after_ws || after_punct);

    if ch == b'_' {
        let can_open = left_flanking && (!right_flanking || before_punct);
        let can_close = right_flanking && (!left_flanking || after_punct);
        (can_open, can_close)
    } else {
        (left_flanking, right_flanking)
    }
}

fn char_is_ws(c: char) -> bool {
    c.is_whitespace()
}

fn char_is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace())
}

fn prev_char(text: &str, at: usize) -> Option<char> {
    text[..at].chars().next_back()
}

fn trim_trailing_spaces(text: &str, from: usize, to: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = to;
    while end > from && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        end -= 1;
    }
    end
}

fn ends_with_two_spaces(text: &str, from: usize, to: usize) -> bool {
    let bytes = text.as_bytes();
    to >= from + 2 && bytes[to - 1] == b' ' && bytes[to - 2] == b' '
}

fn backslash_before(bytes: &[u8], at: usize) -> bool {
    at > 0 && bytes[at - 1] == b'\\'
}
