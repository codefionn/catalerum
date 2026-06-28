//! Sequence diagrams (`sequenceDiagram`) → SVG.
//!
//! Supports participants/actors (with `as` labels, declared or auto-discovered),
//! messages with the usual arrow operators (`->>`, `-->>`, `->`, `-->`, `-x`,
//! `--x`, `-)`, `--)`) including self-messages, `Note left of/right of/over`
//! (one or two participants), `loop`/`alt`/`opt`/`par`/`critical` fragments
//! with `else`/`and` dividers, and `autonumber` (with optional `start`/`step`
//! and `off`) numbered-message badges.

use std::collections::HashMap;

use super::{text_width, MermaidError};
use crate::escape::escape_text;

#[derive(Clone, Copy, PartialEq)]
enum Head {
    Arrow, // >>  filled
    Open,  // >   open
    Cross, // x
    Async, // )
}

#[derive(Clone, Copy)]
enum NoteKind {
    LeftOf,
    RightOf,
    Over,
}

enum Ev {
    Msg {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
        head: Head,
        /// Sequence number when `autonumber` is active at this message, else None.
        num: Option<i64>,
    },
    Note {
        kind: NoteKind,
        a: usize,
        b: usize,
        text: String,
    },
    FragStart {
        kind: String,
        label: String,
    },
    FragElse {
        label: String,
    },
    FragEnd,
    /// `activate <p>` — start an activation bar on participant `p`'s lifeline.
    Activate(usize),
    /// `deactivate <p>` — end the most recent activation bar on `p`.
    Deactivate(usize),
}

struct Seq {
    labels: Vec<String>,
    index: HashMap<String, usize>,
    events: Vec<Ev>,
    // `autonumber` parse state: whether numbering is on, and the next number/step.
    auto_on: bool,
    auto_next: i64,
    auto_step: i64,
}

impl Seq {
    fn pidx(&mut self, id: &str) -> usize {
        let id = id.trim().trim_start_matches(['+', '-']).trim();
        if let Some(&i) = self.index.get(id) {
            return i;
        }
        let i = self.labels.len();
        self.labels.push(id.to_string());
        self.index.insert(id.to_string(), i);
        i
    }

    fn declare(&mut self, id: &str, label: &str) {
        let i = self.pidx(id);
        if !label.is_empty() {
            self.labels[i] = label.to_string();
        }
    }
}

/// `->>`, `-->>`, … operators, longest/most-specific first.
const OPS: &[(&str, bool, Head)] = &[
    ("-->>", true, Head::Arrow),
    ("->>", false, Head::Arrow),
    ("--x", true, Head::Cross),
    ("-x", false, Head::Cross),
    ("--)", true, Head::Async),
    ("-)", false, Head::Async),
    ("-->", true, Head::Open),
    ("->", false, Head::Open),
];

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let mut seq = Seq {
        labels: Vec::new(),
        index: HashMap::new(),
        events: Vec::new(),
        auto_on: false,
        auto_next: 1,
        auto_step: 1,
    };
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header `sequenceDiagram`

    for line in lines {
        parse_line(line, &mut seq);
    }
    if seq.labels.is_empty() {
        return Err(MermaidError("no participants"));
    }
    Ok(render(&seq))
}

fn parse_line(line: &str, seq: &mut Seq) {
    if let Some(rest) = line
        .strip_prefix("participant ")
        .or_else(|| line.strip_prefix("actor "))
    {
        let (id, label) = match rest.split_once(" as ") {
            Some((id, label)) => (id.trim(), label.trim()),
            None => (rest.trim(), ""),
        };
        seq.declare(id, label);
        return;
    }
    if let Some(rest) = line
        .strip_prefix("Note ")
        .or_else(|| line.strip_prefix("note "))
    {
        parse_note(rest, seq);
        return;
    }
    if let Some((kw, label)) = frag_start(line) {
        seq.events.push(Ev::FragStart {
            kind: kw.to_string(),
            label: label.to_string(),
        });
        return;
    }
    if line == "end" {
        seq.events.push(Ev::FragEnd);
        return;
    }
    if let Some(label) = line
        .strip_prefix("else")
        .or_else(|| line.strip_prefix("and"))
    {
        seq.events.push(Ev::FragElse {
            label: label.trim().to_string(),
        });
        return;
    }
    // `autonumber` / `autonumber <start> [step]` / `autonumber off`.
    if let Some(rest) = line.strip_prefix("autonumber") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            set_autonumber(rest.trim(), seq);
            return;
        }
    }
    // Activation bars: `activate <p>` / `deactivate <p>`.
    if let Some(p) = line.strip_prefix("activate ") {
        let idx = seq.pidx(p.trim());
        seq.events.push(Ev::Activate(idx));
        return;
    }
    if let Some(p) = line.strip_prefix("deactivate ") {
        let idx = seq.pidx(p.trim());
        seq.events.push(Ev::Deactivate(idx));
        return;
    }
    // Skip directives we don't render.
    if line.starts_with("title") || line.starts_with("box") {
        return;
    }
    parse_message(line, seq);
}

/// Apply an `autonumber` directive's args: `off` disables; otherwise an optional
/// start and step (defaulting to 1/1) enable numbering from `start`.
fn set_autonumber(args: &str, seq: &mut Seq) {
    if args == "off" {
        seq.auto_on = false;
        return;
    }
    seq.auto_on = true;
    let mut it = args.split_whitespace();
    seq.auto_next = it.next().and_then(|t| t.parse().ok()).unwrap_or(1);
    seq.auto_step = it
        .next()
        .and_then(|t| t.parse().ok())
        .filter(|&s| s != 0)
        .unwrap_or(1);
}

fn frag_start(line: &str) -> Option<(&str, &str)> {
    for kw in ["loop", "alt", "opt", "par", "critical", "break", "rect"] {
        if let Some(rest) = line.strip_prefix(kw) {
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                return Some((kw, rest.trim()));
            }
        }
    }
    None
}

fn parse_note(rest: &str, seq: &mut Seq) {
    let (spec, text) = match rest.split_once(':') {
        Some((s, t)) => (s.trim(), t.trim()),
        None => (rest.trim(), ""),
    };
    let (kind, who) = if let Some(w) = spec.strip_prefix("left of") {
        (NoteKind::LeftOf, w)
    } else if let Some(w) = spec.strip_prefix("right of") {
        (NoteKind::RightOf, w)
    } else if let Some(w) = spec.strip_prefix("over") {
        (NoteKind::Over, w)
    } else {
        return;
    };
    let mut parts = who.split(',');
    let a = seq.pidx(parts.next().unwrap_or("").trim());
    let b = parts.next().map(|p| seq.pidx(p.trim())).unwrap_or(a);
    seq.events.push(Ev::Note {
        kind,
        a,
        b,
        text: text.to_string(),
    });
}

fn parse_message(line: &str, seq: &mut Seq) {
    let (left, text) = match line.split_once(':') {
        Some((l, t)) => (l, t.trim()),
        None => (line, ""),
    };
    let lc: Vec<char> = left.chars().collect();
    let Some((opi, dashed, head, ope)) = find_op(&lc) else {
        return;
    };
    let from: String = lc[..opi].iter().collect();
    let to: String = lc[ope..].iter().collect();
    if from.trim().is_empty() || to.trim().is_empty() {
        return;
    }
    // Mermaid activation shorthand: a `+`/`-` right after the arrow (a prefix of the
    // target token) means "activate the target" / "deactivate the source". `pidx`
    // strips the sign, so capture the intent first.
    let to_sign = to.trim().chars().next();
    let from = seq.pidx(&from);
    let to = seq.pidx(&to);
    let num = seq.auto_on.then(|| {
        let n = seq.auto_next;
        seq.auto_next += seq.auto_step;
        n
    });
    seq.events.push(Ev::Msg {
        from,
        to,
        text: text.to_string(),
        dashed,
        head,
        num,
    });
    match to_sign {
        Some('+') => seq.events.push(Ev::Activate(to)),
        Some('-') => seq.events.push(Ev::Deactivate(from)),
        _ => {}
    }
}

fn find_op(c: &[char]) -> Option<(usize, bool, Head, usize)> {
    for i in 0..c.len() {
        for (pat, dashed, head) in OPS {
            let pat: Vec<char> = pat.chars().collect();
            if super::matches_at(c, i, &pat) {
                return Some((i, *dashed, *head, i + pat.len()));
            }
        }
    }
    None
}

// ---- rendering -----------------------------------------------------------------

const MARGIN: f64 = 14.0;
const PART_H: f64 = 34.0;
const MSG_GAP: f64 = 40.0;
const SELF_H: f64 = 34.0;
const NOTE_H: f64 = 30.0;
/// Width of an activation bar drawn over a participant's lifeline.
const ACT_W: f64 = 10.0;
// Header/divider bands leave room for the `loop`/`alt` tab + `[label]` text so the
// first message inside a fragment (or after an `else`) doesn't overlap it.
const FRAG_HEADER: f64 = 42.0;
const FRAG_ELSE: f64 = 36.0;

struct FragBox {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    kind: String,
    label: String,
    dividers: Vec<(f64, String)>,
}

fn render(seq: &Seq) -> String {
    let n = seq.labels.len();
    // Column geometry.
    let widths: Vec<f64> = seq
        .labels
        .iter()
        .map(|l| (text_width(l) + 24.0).max(70.0))
        .collect();
    let max_w = widths.iter().copied().fold(0.0, f64::max);
    let max_msg = seq
        .events
        .iter()
        .filter_map(|e| match e {
            Ev::Msg { text, .. } => Some(text_width(text)),
            _ => None,
        })
        .fold(0.0, f64::max)
        .min(320.0);
    let col_gap = (max_w + 24.0).max(max_msg + 36.0).max(120.0);
    let x = |i: usize| MARGIN + max_w / 2.0 + i as f64 * col_gap;
    let width = x(n.saturating_sub(1)) + max_w / 2.0 + MARGIN;

    // Walk events, building the message/note body and fragment boxes, tracking y.
    let mut body = String::new();
    let mut frags: Vec<FragBox> = Vec::new();
    let mut stack: Vec<FragBox> = Vec::new();
    // Activation bars: a per-participant stack of start-y (nesting) and the
    // finished bars to draw — (participant, nesting depth, y0, y1).
    let mut active: Vec<Vec<f64>> = vec![Vec::new(); n];
    let mut acts: Vec<(usize, usize, f64, f64)> = Vec::new();
    let lifeline_top = MARGIN + PART_H;
    let mut y = lifeline_top + 28.0;

    let span = |stack_empty_x0: f64, x1: f64| (stack_empty_x0, x1);
    let _ = span;
    let full_x0 = x(0) - max_w / 2.0 - 10.0;
    let full_x1 = x(n.saturating_sub(1)) + max_w / 2.0 + 10.0;

    for ev in &seq.events {
        match ev {
            Ev::Msg {
                from,
                to,
                text,
                dashed,
                head,
                num,
            } => {
                if from == to {
                    render_self_msg(x(*from), y, text, *dashed, *head, *num, &mut body);
                    y += SELF_H + 8.0;
                } else {
                    render_msg(x(*from), x(*to), y, text, *dashed, *head, *num, &mut body);
                    y += MSG_GAP;
                }
            }
            Ev::Note { kind, a, b, text } => {
                render_note(*kind, x(*a), x(*b), y, text, &widths[*a], &mut body);
                y += NOTE_H + 12.0;
            }
            Ev::FragStart { kind, label } => {
                stack.push(FragBox {
                    x0: full_x0,
                    y0: y,
                    x1: full_x1,
                    y1: 0.0,
                    kind: kind.clone(),
                    label: label.clone(),
                    dividers: Vec::new(),
                });
                y += FRAG_HEADER;
            }
            Ev::FragElse { label } => {
                if let Some(top) = stack.last_mut() {
                    top.dividers.push((y, label.clone()));
                }
                y += FRAG_ELSE;
            }
            Ev::FragEnd => {
                if let Some(mut fb) = stack.pop() {
                    fb.y1 = y + 6.0;
                    frags.push(fb);
                    y += 14.0;
                }
            }
            Ev::Activate(p) => active[*p].push(y),
            Ev::Deactivate(p) => {
                if let Some(y0) = active[*p].pop() {
                    acts.push((*p, active[*p].len(), y0, y));
                }
            }
        }
    }
    // Close any activation bar left open at the end (extends to the last event).
    for (p, starts) in active.iter().enumerate() {
        for (depth, &y0) in starts.iter().enumerate() {
            acts.push((p, depth, y0, y));
        }
    }
    // Close any unterminated fragments.
    while let Some(mut fb) = stack.pop() {
        fb.y1 = y + 6.0;
        frags.push(fb);
        y += 14.0;
    }

    let height = y + 12.0 + PART_H + MARGIN;
    let lifeline_bottom = height - MARGIN - PART_H;

    // Assemble: defs, lifelines, fragment boxes (behind), body (front), boxes.
    let mut out = String::with_capacity(640 + body.len() + frags.len() * 200 + n * 160);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-sequence\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"14\">"
    ));
    out.push_str(
        "<defs>\
         <marker id=\"cm-sq-arrow\" markerWidth=\"10\" markerHeight=\"8\" refX=\"9\" refY=\"3.5\" \
         orient=\"auto\" markerUnits=\"userSpaceOnUse\"><path d=\"M0,0 L9,3.5 L0,7 z\" fill=\"#475569\"/></marker>\
         <marker id=\"cm-sq-open\" markerWidth=\"11\" markerHeight=\"8\" refX=\"9\" refY=\"3.5\" \
         orient=\"auto\" markerUnits=\"userSpaceOnUse\"><path d=\"M0,0 L9,3.5 L0,7\" fill=\"none\" \
         stroke=\"#475569\" stroke-width=\"1.3\"/></marker></defs>",
    );

    // Lifelines.
    for i in 0..n {
        out.push_str(&format!(
            "<line x1=\"{0:.1}\" y1=\"{lifeline_top:.1}\" x2=\"{0:.1}\" y2=\"{lifeline_bottom:.1}\" \
             stroke=\"#94a3b8\" stroke-width=\"1\" stroke-dasharray=\"3 3\"/>",
            x(i)
        ));
    }

    // Activation bars on the lifelines (behind messages, so arrows land on them).
    for &(p, depth, y0, y1) in &acts {
        render_activation(x(p), depth, y0, y1, &mut out);
    }

    // Fragment boxes (behind messages).
    for fb in &frags {
        render_frag(fb, &mut out);
    }

    out.push_str(&body);

    // Participant boxes, top and bottom.
    for (i, (w, label)) in widths.iter().zip(&seq.labels).enumerate() {
        render_participant(x(i), MARGIN, *w, label, &mut out);
        render_participant(x(i), lifeline_bottom, *w, label, &mut out);
    }

    out.push_str("</svg>");
    out
}

/// An activation bar: a thin rectangle on the lifeline from `y0` to `y1`. Nested
/// activations (`depth` > 0) shift right so the stack is visible.
fn render_activation(cx: f64, depth: usize, y0: f64, y1: f64, out: &mut String) {
    let x = cx - ACT_W / 2.0 + depth as f64 * (ACT_W / 2.0);
    let h = (y1 - y0).max(8.0);
    out.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y0:.1}\" width=\"{ACT_W:.1}\" height=\"{h:.1}\" \
         fill=\"#e0e7ff\" stroke=\"#6366f1\" stroke-width=\"1\"/>"
    ));
}

fn render_participant(cx: f64, y: f64, w: f64, label: &str, out: &mut String) {
    out.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{PART_H}\" rx=\"4\" \
         fill=\"#eff6ff\" stroke=\"#3b82f6\" stroke-width=\"1.5\"/>",
        cx - w / 2.0
    ));
    out.push_str(&format!(
        "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#1e293b\" \
         font-weight=\"600\">",
        y + PART_H / 2.0 + 5.0
    ));
    escape_text(out, label);
    out.push_str("</text>");
}

#[allow(clippy::too_many_arguments)]
fn render_msg(
    x0: f64,
    x1: f64,
    y: f64,
    text: &str,
    dashed: bool,
    head: Head,
    num: Option<i64>,
    out: &mut String,
) {
    let dash = if dashed {
        " stroke-dasharray=\"5 4\""
    } else {
        ""
    };
    let marker = match head {
        Head::Arrow => " marker-end=\"url(#cm-sq-arrow)\"",
        Head::Open | Head::Async => " marker-end=\"url(#cm-sq-open)\"",
        Head::Cross => "",
    };
    out.push_str(&format!(
        "<line x1=\"{x0:.1}\" y1=\"{y:.1}\" x2=\"{x1:.1}\" y2=\"{y:.1}\" stroke=\"#475569\" \
         stroke-width=\"1.4\"{dash}{marker}/>"
    ));
    if head == Head::Cross {
        let s = 5.0;
        let xe = x1;
        out.push_str(&format!(
            "<path d=\"M{:.1},{:.1} L{:.1},{:.1} M{:.1},{:.1} L{:.1},{:.1}\" stroke=\"#475569\" \
             stroke-width=\"1.4\"/>",
            xe - s,
            y - s,
            xe + s,
            y + s,
            xe - s,
            y + s,
            xe + s,
            y - s
        ));
    }
    if !text.is_empty() {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#334155\" \
             font-size=\"13\">",
            (x0 + x1) / 2.0,
            y - 6.0
        ));
        escape_text(out, text);
        out.push_str("</text>");
    }
    if let Some(n) = num {
        // Badge sits on the arrow just off its source end (toward the target).
        render_seq_number(x0 + (x1 - x0).signum() * 11.0, y, n, out);
    }
}

fn render_self_msg(
    cx: f64,
    y: f64,
    text: &str,
    dashed: bool,
    head: Head,
    num: Option<i64>,
    out: &mut String,
) {
    let dash = if dashed {
        " stroke-dasharray=\"5 4\""
    } else {
        ""
    };
    let marker = match head {
        Head::Arrow => " marker-end=\"url(#cm-sq-arrow)\"",
        Head::Cross => "",
        _ => " marker-end=\"url(#cm-sq-open)\"",
    };
    out.push_str(&format!(
        "<path d=\"M{cx:.1},{y:.1} h34 v18 h-34\" fill=\"none\" stroke=\"#475569\" \
         stroke-width=\"1.4\"{dash}{marker}/>"
    ));
    if !text.is_empty() {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#334155\" font-size=\"13\">",
            cx + 42.0,
            y + 6.0
        ));
        escape_text(out, text);
        out.push_str("</text>");
    }
    if let Some(n) = num {
        render_seq_number(cx + 11.0, y, n, out);
    }
}

/// A small numbered circle marking a message's `autonumber` index.
fn render_seq_number(cx: f64, cy: f64, n: i64, out: &mut String) {
    out.push_str(&format!(
        "<circle cx=\"{cx:.1}\" cy=\"{cy:.1}\" r=\"9\" fill=\"#e0e7ff\" stroke=\"#6366f1\" \
         stroke-width=\"1\"/>"
    ));
    // `n` is an integer — safe to embed directly, no escaping needed.
    out.push_str(&format!(
        "<text x=\"{cx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#3730a3\" font-size=\"11\" \
         font-weight=\"600\">{n}</text>",
        cy + 4.0
    ));
}

fn render_note(kind: NoteKind, xa: f64, xb: f64, y: f64, text: &str, wa: &f64, out: &mut String) {
    let tw = text_width(text) + 22.0;
    let (x, w) = match kind {
        NoteKind::Over => {
            if (xa - xb).abs() < 1.0 {
                (xa - tw.max(*wa) / 2.0, tw.max(*wa))
            } else {
                let left = xa.min(xb) - 24.0;
                let right = xa.max(xb) + 24.0;
                (left, (right - left).max(tw))
            }
        }
        NoteKind::RightOf => (xa + 10.0, tw),
        NoteKind::LeftOf => (xa - 10.0 - tw, tw),
    };
    out.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{NOTE_H}\" rx=\"2\" \
         fill=\"#fef9c3\" stroke=\"#eab308\" stroke-width=\"1\"/>"
    ));
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#713f12\" font-size=\"13\">",
        x + w / 2.0,
        y + NOTE_H / 2.0 + 4.0
    ));
    escape_text(out, text);
    out.push_str("</text>");
}

fn render_frag(fb: &FragBox, out: &mut String) {
    out.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" fill=\"#6366f1\" \
         fill-opacity=\"0.04\" stroke=\"#6366f1\" stroke-width=\"1\" stroke-opacity=\"0.5\"/>",
        fb.x0,
        fb.y0,
        fb.x1 - fb.x0,
        fb.y1 - fb.y0
    ));
    // Label tab.
    let tab_w = text_width(&fb.kind) + 16.0;
    out.push_str(&format!(
        "<path d=\"M{:.1},{:.1} h{:.1} v12 l-8,8 h-{:.1} z\" fill=\"#6366f1\" fill-opacity=\"0.85\"/>",
        fb.x0,
        fb.y0,
        tab_w,
        tab_w - 8.0
    ));
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#ffffff\" font-size=\"12\" font-weight=\"600\">",
        fb.x0 + 7.0,
        fb.y0 + 14.0
    ));
    escape_text(out, &fb.kind);
    out.push_str("</text>");
    if !fb.label.is_empty() {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#4338ca\" font-size=\"12\">",
            fb.x0 + tab_w + 8.0,
            fb.y0 + 14.0
        ));
        escape_text(out, &format!("[{}]", fb.label));
        out.push_str("</text>");
    }
    // `else`/`and` dividers.
    for (dy, label) in &fb.dividers {
        out.push_str(&format!(
            "<line x1=\"{:.1}\" y1=\"{dy:.1}\" x2=\"{:.1}\" y2=\"{dy:.1}\" stroke=\"#6366f1\" \
             stroke-width=\"1\" stroke-dasharray=\"3 3\" stroke-opacity=\"0.6\"/>",
            fb.x0, fb.x1
        ));
        if !label.is_empty() {
            out.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#4338ca\" \
                 font-size=\"12\">",
                (fb.x0 + fb.x1) / 2.0,
                dy + 13.0
            ));
            escape_text(out, &format!("[{label}]"));
            out.push_str("</text>");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn messages_and_participants() {
        let svg = to_svg(
            "sequenceDiagram\n participant A as Alice\n participant B as Bob\n A->>B: Hello\n B-->>A: Hi back",
        )
        .unwrap();
        assert!(svg.contains("<svg"));
        assert!(svg.contains(">Alice</text>"));
        assert!(svg.contains(">Bob</text>"));
        assert!(svg.contains(">Hello</text>"));
        assert!(svg.contains("stroke-dasharray=\"5 4\"")); // the dashed reply
        assert!(svg.contains("marker-end")); // arrows
        assert!(svg.contains("stroke-dasharray=\"3 3\"")); // lifelines
    }

    #[test]
    fn auto_declares_participants_in_order() {
        let svg = to_svg("sequenceDiagram\n Client->>Server: req\n Server->>DB: query").unwrap();
        for who in [">Client</text>", ">Server</text>", ">DB</text>"] {
            assert!(svg.contains(who), "{who} missing: {svg}");
        }
    }

    #[test]
    fn self_message_and_notes() {
        let svg = to_svg(
            "sequenceDiagram\n A->>A: think\n Note right of A: a note\n Note over A,B: spanning",
        )
        .unwrap();
        assert!(svg.contains(">think</text>"));
        assert!(svg.contains(">a note</text>"));
        assert!(svg.contains(">spanning</text>"));
        assert!(svg.contains("#fef9c3")); // note fill
    }

    #[test]
    fn fragments_with_else() {
        let svg = to_svg(
            "sequenceDiagram\n A->>B: try\n alt ok\n B->>A: yes\n else fail\n B->>A: no\n end",
        )
        .unwrap();
        assert!(svg.contains(">alt</text>"), "{svg}");
        assert!(svg.contains("[ok]"), "{svg}");
        assert!(svg.contains("[fail]"), "{svg}");
    }

    #[test]
    fn autonumber_numbers_each_message() {
        let svg = to_svg("sequenceDiagram\n autonumber\n A->>B: one\n B-->>A: two\n A->>A: three")
            .unwrap();
        // One badge per message (incl. the self-message), numbered 1,2,3.
        assert_eq!(svg.matches("<circle").count(), 3, "{svg}");
        for n in [">1</text>", ">2</text>", ">3</text>"] {
            assert!(svg.contains(n), "missing {n}: {svg}");
        }
        assert!(svg.contains("#e0e7ff"), "badge fill missing: {svg}");
    }

    #[test]
    fn autonumber_honours_start_and_step() {
        let svg = to_svg("sequenceDiagram\n autonumber 10 5\n A->>B: a\n B->>A: b").unwrap();
        assert!(svg.contains(">10</text>"), "{svg}");
        assert!(svg.contains(">15</text>"), "{svg}");
    }

    #[test]
    fn autonumber_off_stops_numbering() {
        let svg =
            to_svg("sequenceDiagram\n autonumber\n A->>B: a\n autonumber off\n B->>A: b").unwrap();
        // Only the first message is numbered.
        assert_eq!(svg.matches("<circle").count(), 1, "{svg}");
        assert!(svg.contains(">1</text>"), "{svg}");
    }

    #[test]
    fn no_autonumber_means_no_badges() {
        let svg = to_svg("sequenceDiagram\n A->>B: a\n B->>A: b").unwrap();
        assert_eq!(svg.matches("<circle").count(), 0, "{svg}");
    }

    #[test]
    fn activation_bars_render_for_activate_deactivate() {
        let svg = to_svg("sequenceDiagram\n A->>B: req\n activate B\n B-->>A: res\n deactivate B")
            .unwrap();
        // One activation bar — the ACT_W=10 rect (participant/note/frag boxes are wider).
        assert_eq!(svg.matches("width=\"10.0\"").count(), 1, "{svg}");
    }

    #[test]
    fn nested_activations_stack_and_unclosed_extends_to_end() {
        // Inner activation closes; the outer is left open and is closed implicitly
        // at the end of the diagram — both bars render.
        let svg = to_svg(
            "sequenceDiagram\n A->>B: a\n activate B\n B->>B: b\n activate B\n \
             B-->>A: c\n deactivate B",
        )
        .unwrap();
        assert_eq!(svg.matches("width=\"10.0\"").count(), 2, "{svg}");
    }

    #[test]
    fn activation_shorthand_plus_minus_on_messages() {
        // `A->>+B` activates B; `B-->>-A` deactivates B (the reply's source) — one bar.
        let svg = to_svg("sequenceDiagram\n A->>+B: req\n B-->>-A: res").unwrap();
        assert_eq!(svg.matches("width=\"10.0\"").count(), 1, "{svg}");
        // The signs are stripped from the participant labels (B, not +B / -…).
        assert!(
            svg.contains(">B</text>") && !svg.contains(">+B</text>"),
            "{svg}"
        );
    }

    #[test]
    fn labels_escaped() {
        let svg = to_svg("sequenceDiagram\n A->>B: <script>x</script>").unwrap();
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;"));
    }

    #[test]
    fn never_panics() {
        for s in [
            "sequenceDiagram",
            "sequenceDiagram\n A->>",
            "sequenceDiagram\n ->>B: x",
            "sequenceDiagram\n A->>B",
            "sequenceDiagram\n Note over",
            "sequenceDiagram\n alt\n else\n end\n end",
            "sequenceDiagram\n loop\n A->>A: x",
            "sequenceDiagram\n autonumber abc xyz\n A->>B: x",
            "sequenceDiagram\n autonumber off",
            "sequenceDiagram\n autonumber 5 0\n A->>B: x\n A->>B: y",
            "sequenceDiagram\n deactivate A\n A->>B: x", // deactivate with nothing active
            "sequenceDiagram\n activate A",              // activate, never closed, no msgs
            "sequenceDiagram\n A->>B: x\n activate B",   // activate at the very end
            "sequenceDiagram\n A-->>-B: x",              // shorthand deactivate, nothing active
            "sequenceDiagram\n A->>+: x",                // shorthand activate, empty target
        ] {
            let _ = to_svg(s);
        }
    }
}
