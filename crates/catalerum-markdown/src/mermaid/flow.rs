//! Flowcharts (`graph`/`flowchart`) → SVG, via a layered (longest-path rank)
//! layout. Directions `TD`/`TB`/`BT`/`LR`/`RL`; shapes rect/round/stadium/rhombus/
//! circle/hexagon/parallelogram/trapezoid/subroutine/cylinder; edges
//! `-->`/`---`/`-.->`/`==>` (and bidirectional `<-->`) with `|label|` or inline
//! text, and `&` node groups (`A & B --> C`).

use std::collections::HashMap;

use super::{matches_at, skip_ws, strip_quotes, MermaidError};
use crate::escape::escape_text;

/// Parse a Mermaid flowchart and render it to a standalone SVG string.
pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let graph = parse(src)?;
    if graph.nodes.is_empty() {
        return Err(MermaidError("no nodes"));
    }
    Ok(render_graph(&graph))
}

/// Lay out and render a graph built elsewhere (the layered layout + SVG render
/// reused by sibling diagram types such as [`super::state`]).
pub(super) fn render_graph(graph: &Graph) -> String {
    render(&layout(graph), graph)
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Dir {
    Down,
    Up,
    Right,
    Left,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Shape {
    Rect,
    Round,
    Stadium,
    Rhombus,
    Circle,
    Hexagon,
    /// A small filled dot with no label — the state-diagram `[*]` pseudo-state.
    Point,
    /// `[/…/]` — leans right (input/output).
    Parallelogram,
    /// `[\…\]` — leans left.
    ParallelogramAlt,
    /// `[/…\]` — narrow top, wide bottom.
    Trapezoid,
    /// `[\…/]` — wide top, narrow bottom (manual operation).
    TrapezoidAlt,
    /// `[[…]]` — rectangle with inset side bars (subroutine / predefined process).
    Subroutine,
    /// `[(…)]` — database cylinder.
    Cylinder,
}

struct Node {
    label: String,
    shape: Shape,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Style {
    Solid,
    Dotted,
    Thick,
}

struct Edge {
    from: usize,
    to: usize,
    style: Style,
    /// A back-arrow at the source end (`A <--> B`).
    start_arrow: bool,
    arrow: bool,
    label: String,
}

pub(super) struct Graph {
    dir: Dir,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    index: HashMap<String, usize>,
}

impl Graph {
    /// An empty graph with the given layout direction (used by sibling diagram
    /// types — e.g. [`super::state`] — that build a graph then reuse the layout).
    pub(super) fn new(dir: Dir) -> Self {
        Self {
            dir,
            nodes: Vec::new(),
            edges: Vec::new(),
            index: HashMap::new(),
        }
    }

    /// Whether the graph has no nodes (so the caller can fall back to raw source).
    pub(super) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Add an edge between two node indices (used by sibling diagram types). The
    /// source-end back-arrow is off (sibling types don't produce bidirectional edges).
    pub(super) fn add_edge(
        &mut self,
        from: usize,
        to: usize,
        style: Style,
        arrow: bool,
        label: String,
    ) {
        self.edges.push(Edge {
            from,
            to,
            style,
            start_arrow: false,
            arrow,
            label,
        });
    }

    /// Reference a node by id, creating it (label defaults to the id) if new.
    fn node_ref(&mut self, id: &str) -> usize {
        if let Some(&i) = self.index.get(id) {
            return i;
        }
        let i = self.nodes.len();
        self.nodes.push(Node {
            label: id.to_string(),
            shape: Shape::Rect,
        });
        self.index.insert(id.to_string(), i);
        i
    }

    /// Define a node's shape + label (a later `A[label]` overrides the default).
    pub(super) fn node_def(&mut self, id: &str, label: String, shape: Shape) -> usize {
        let i = self.node_ref(id);
        self.nodes[i].label = label;
        self.nodes[i].shape = shape;
        i
    }
}

// ---- parsing -------------------------------------------------------------------

fn parse(src: &str) -> Result<Graph, MermaidError> {
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));

    let header = lines.next().ok_or(MermaidError("empty diagram"))?;
    let dir = parse_header(header)?;

    let mut graph = Graph {
        dir,
        nodes: Vec::new(),
        edges: Vec::new(),
        index: HashMap::new(),
    };

    // The header may carry inline statements after the direction
    // (`graph TD; A-->B`). Strip the keyword *and* the direction token first, so
    // the direction itself (`LR`) is never mistaken for a node.
    let after_kw = {
        let h = header.trim_start();
        h.strip_prefix("flowchart")
            .or_else(|| h.strip_prefix("graph"))
            .unwrap_or("")
            .trim_start()
    };
    let first_stmts = match after_kw.split_once([' ', ';', '\t']) {
        Some((_dir, rest)) => rest,
        None => "",
    };
    for stmt in first_stmts.split(';') {
        parse_statement(stmt, &mut graph)?;
    }
    for line in lines {
        for stmt in line.split(';') {
            parse_statement(stmt, &mut graph)?;
        }
    }
    Ok(graph)
}

fn parse_header(header: &str) -> Result<Dir, MermaidError> {
    let h = header.trim();
    let kw = if let Some(r) = h.strip_prefix("flowchart") {
        r
    } else if let Some(r) = h.strip_prefix("graph") {
        r
    } else {
        return Err(MermaidError("unsupported diagram type"));
    };
    let dir = kw.trim_start().split([';', ' ']).next().unwrap_or("");
    Ok(match dir {
        "TD" | "TB" | "" => Dir::Down,
        "BT" => Dir::Up,
        "LR" => Dir::Right,
        "RL" => Dir::Left,
        _ => Dir::Down,
    })
}

fn parse_statement(stmt: &str, graph: &mut Graph) -> Result<(), MermaidError> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut i = 0usize;
    skip_ws(&chars, &mut i);
    if i >= chars.len() {
        return Ok(());
    }
    // `subgraph`/`end`/`style`/`classDef`/`click` etc. — ignore (don't fail).
    let word: String = chars[i..]
        .iter()
        .take_while(|c| c.is_alphabetic())
        .collect();
    if matches!(
        word.as_str(),
        "subgraph" | "end" | "style" | "classDef" | "class" | "click" | "linkStyle" | "direction"
    ) {
        return Ok(());
    }

    // The previous edge endpoint is a *group* of nodes: the `&` shorthand
    // (`A & B --> C & D`) connects every node in one group to every node in the
    // next. A plain `A --> B` is just the one-element-group case.
    let mut prev_group: Vec<usize> = Vec::new();
    let mut pending: Option<(Style, bool, bool, String)> = None;
    loop {
        skip_ws(&chars, &mut i);
        if i >= chars.len() {
            break;
        }
        if is_edge_start(&chars, i) {
            let (style, start_arrow, arrow, label, ni) = lex_edge(&chars, i)?;
            i = ni;
            pending = Some((style, start_arrow, arrow, label));
        } else {
            // Parse a node group: `node ('&' node)*`.
            let mut cur_group: Vec<usize> = Vec::new();
            loop {
                skip_ws(&chars, &mut i);
                let (id, label, shape, ni) = lex_node(&chars, i)?;
                i = ni;
                let node = if let Some(lbl) = label {
                    graph.node_def(&id, lbl, shape)
                } else {
                    graph.node_ref(&id)
                };
                cur_group.push(node);
                skip_ws(&chars, &mut i);
                if chars.get(i) == Some(&'&') {
                    i += 1; // consume '&' and parse the next node of the group
                } else {
                    break;
                }
            }
            if let Some((style, start_arrow, arrow, lbl)) = pending.take() {
                for &from in &prev_group {
                    for &to in &cur_group {
                        graph.edges.push(Edge {
                            from,
                            to,
                            style,
                            start_arrow,
                            arrow,
                            label: lbl.clone(),
                        });
                    }
                }
            }
            prev_group = cur_group;
        }
    }
    Ok(())
}

fn is_edge_start(c: &[char], i: usize) -> bool {
    matches!(c.get(i), Some('-' | '=' | '.' | '<' | 'o' | 'x') if {
        match c[i] {
            'o' | 'x' => matches!(c.get(i + 1), Some('-' | '=')),
            _ => true,
        }
    })
}

/// Lex an edge operator, returning `(style, start_arrow, end_arrow, label, next_index)`.
/// A leading `<` (`A <--> B`) is a back-arrow at the source end.
fn lex_edge(c: &[char], start: usize) -> Result<(Style, bool, bool, String, usize), MermaidError> {
    let mut i = start;
    let start_arrow = c.get(i) == Some(&'<');
    if matches!(c.get(i), Some('<' | 'o' | 'x')) {
        i += 1;
    }
    let head_start = i;
    while matches!(c.get(i), Some('-' | '=' | '.')) {
        i += 1;
    }
    if i == head_start {
        return Err(MermaidError("bad edge"));
    }
    let head: String = c[head_start..i].iter().collect();
    let style = if head.contains('=') {
        Style::Thick
    } else if head.contains('.') {
        Style::Dotted
    } else {
        Style::Solid
    };

    let mut label = String::new();
    let mut arrow = matches!(c.get(i), Some('>'));
    if matches!(c.get(i), Some('>')) {
        i += 1;
    } else if !head_is_complete(&head) {
        let lbl_start = i;
        while i < c.len() && !matches!(c.get(i), Some('-' | '=' | '.')) {
            i += 1;
        }
        label = c[lbl_start..i]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        while matches!(c.get(i), Some('-' | '=' | '.')) {
            i += 1;
        }
        if matches!(c.get(i), Some('>')) {
            arrow = true;
            i += 1;
        }
    }

    skip_ws(c, &mut i);
    if matches!(c.get(i), Some('|')) {
        i += 1;
        let lbl_start = i;
        while i < c.len() && c[i] != '|' {
            i += 1;
        }
        label = c[lbl_start..i]
            .iter()
            .collect::<String>()
            .trim()
            .to_string();
        if matches!(c.get(i), Some('|')) {
            i += 1;
        }
    }
    Ok((style, start_arrow, arrow, strip_quotes(&label), i))
}

/// A head run is a complete (label-less) link when it has ≥3 chars (`---`, `===`);
/// `--`/`==` alone start an inline-label edge.
fn head_is_complete(head: &str) -> bool {
    head.len() >= 3
}

/// Lex a node `id` plus an optional shape/label, returning `(id, label, shape, ni)`.
fn lex_node(
    c: &[char],
    start: usize,
) -> Result<(String, Option<String>, Shape, usize), MermaidError> {
    let mut i = start;
    let id_start = i;
    while matches!(c.get(i), Some(ch) if ch.is_alphanumeric() || *ch == '_' || *ch == '-') {
        if c[i] == '-' && matches!(c.get(i + 1), Some('-' | '.' | '>' | '=')) {
            break;
        }
        i += 1;
    }
    if i == id_start {
        return Err(MermaidError("expected node id"));
    }
    let id: String = c[id_start..i].iter().collect();

    let (shape, close): (Shape, &str) = match (c.get(i), c.get(i + 1)) {
        (Some('('), Some('(')) => (Shape::Circle, "))"),
        (Some('('), Some('[')) => (Shape::Stadium, "])"),
        (Some('('), _) => (Shape::Round, ")"),
        (Some('['), Some('[')) => (Shape::Subroutine, "]]"),
        (Some('['), Some('(')) => (Shape::Cylinder, ")]"),
        // `[/…/]`, `[\…\]`, `[/…\]`, `[\…/]` — parallelogram / trapezoid. The lead
        // slash alone can't pick the shape (the *trailing* slash decides), so this
        // path reads to the closer itself rather than via a fixed `close` token.
        (Some('['), Some('/' | '\\')) => return lex_skewed_node(c, i, id),
        (Some('['), _) => (Shape::Rect, "]"),
        (Some('{'), Some('{')) => (Shape::Hexagon, "}}"),
        (Some('{'), _) => (Shape::Rhombus, "}"),
        _ => return Ok((id, None, Shape::Rect, i)),
    };
    let open_len = close.len();
    i += open_len;
    let (label, ni) = read_label(c, i, close)?;
    Ok((id, Some(label), shape, ni))
}

/// Lex a skewed node opened by `[/` or `[\` at `i`. The lead slash and the
/// trailing slash before `]` together pick the shape: matching slants ⇒
/// parallelogram, opposing ⇒ trapezoid. A `"…"` quote protects inner slashes.
fn lex_skewed_node(
    c: &[char],
    i: usize,
    id: String,
) -> Result<(String, Option<String>, Shape, usize), MermaidError> {
    let lead = c[i + 1];
    let mut k = i + 2;
    if c.get(k) == Some(&'"') {
        k += 1;
        let s = k;
        while k < c.len() && c[k] != '"' {
            k += 1;
        }
        let label: String = c[s..k].iter().collect();
        if c.get(k) == Some(&'"') {
            k += 1;
        }
        while k < c.len() && !is_skew_close(c, k) {
            k += 1;
        }
        let trail = c.get(k).copied().unwrap_or('/');
        if is_skew_close(c, k) {
            k += 2;
        }
        return Ok((id, Some(label), skew_shape(lead, trail), k));
    }
    let s = k;
    while k < c.len() && !is_skew_close(c, k) {
        k += 1;
    }
    if !is_skew_close(c, k) {
        return Err(MermaidError("unterminated node label"));
    }
    let label = c[s..k].iter().collect::<String>().trim().to_string();
    let trail = c[k];
    Ok((id, Some(label), skew_shape(lead, trail), k + 2))
}

/// A skewed-node closer is `/` or `\` immediately followed by `]`.
fn is_skew_close(c: &[char], k: usize) -> bool {
    matches!(c.get(k), Some('/' | '\\')) && c.get(k + 1) == Some(&']')
}

/// Pick the shape from the lead slash (after `[`) and the trailing slash (before
/// `]`): same slant ⇒ parallelogram, opposing ⇒ trapezoid.
fn skew_shape(lead: char, trail: char) -> Shape {
    match (lead, trail) {
        ('/', '/') => Shape::Parallelogram,
        ('\\', '\\') => Shape::ParallelogramAlt,
        ('/', '\\') => Shape::Trapezoid,
        _ => Shape::TrapezoidAlt, // ('\\', '/')
    }
}

/// Read a shape label until the `close` delimiter, honouring a `"…"` quote.
fn read_label(c: &[char], start: usize, close: &str) -> Result<(String, usize), MermaidError> {
    let closing: Vec<char> = close.chars().collect();
    let mut i = start;
    if c.get(i) == Some(&'"') {
        i += 1;
        let s = i;
        while i < c.len() && c[i] != '"' {
            i += 1;
        }
        let label: String = c[s..i].iter().collect();
        if c.get(i) == Some(&'"') {
            i += 1;
        }
        while i < c.len() && !matches_at(c, i, &closing) {
            i += 1;
        }
        if matches_at(c, i, &closing) {
            i += closing.len();
        }
        return Ok((label, i));
    }
    let s = i;
    while i < c.len() && !matches_at(c, i, &closing) {
        i += 1;
    }
    if !matches_at(c, i, &closing) {
        return Err(MermaidError("unterminated node label"));
    }
    let label: String = c[s..i].iter().collect();
    i += closing.len();
    Ok((label.trim().to_string(), i))
}

// ---- layout --------------------------------------------------------------------

pub(super) struct Placed {
    pub(super) cx: f64,
    pub(super) cy: f64,
    pub(super) w: f64,
    pub(super) h: f64,
}

pub(super) struct Layout {
    pub(super) nodes: Vec<Placed>,
    pub(super) width: f64,
    pub(super) height: f64,
}

const MARGIN: f64 = 14.0;
const CHAR_W: f64 = 8.6;
const NODE_H: f64 = 40.0;
/// Baseline-to-baseline spacing for a `<br/>`-split multi-line label.
const LINE_H: f64 = 18.0;
const BREADTH_GAP: f64 = 34.0;
const DEPTH_GAP: f64 = 54.0;

/// Split a node label on `<br>`-style tags (`<br>`, `<br/>`, `<br />`,
/// case-insensitive, whitespace-tolerant), yielding the trimmed text of each
/// line. Always returns at least one element; other markup is left untouched and
/// escaped at render time. Blank lines at the very start/end are dropped — a
/// leading, trailing, or lone `<br>` shouldn't add an empty row — but an interior
/// blank is kept, since `a<br><br>b` is a deliberate blank middle line.
fn split_br(label: &str) -> Vec<&str> {
    let bytes = label.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if let Some(end) = match_br(bytes, i) {
            // `i` sits on `<` and `end` just past `>`, both ASCII char boundaries.
            lines.push(label[start..i].trim());
            start = end;
            i = end;
        } else {
            i += 1;
        }
    }
    lines.push(label[start..].trim());
    // Trim blank lines off both ends (keeping interior ones); fall back to a
    // single blank line for an all-empty label so callers still get ≥1 element.
    match (
        lines.iter().position(|l| !l.is_empty()),
        lines.iter().rposition(|l| !l.is_empty()),
    ) {
        (Some(lo), Some(hi)) => lines[lo..=hi].to_vec(),
        _ => vec![""],
    }
}

/// If a `<br>`-style tag begins at byte `i`, return the index just past its `>`.
fn match_br(b: &[u8], i: usize) -> Option<usize> {
    let r = b.get(i..)?;
    if !(r.first()? == &b'<'
        && r.get(1).is_some_and(|c| c.eq_ignore_ascii_case(&b'b'))
        && r.get(2).is_some_and(|c| c.eq_ignore_ascii_case(&b'r')))
    {
        return None;
    }
    let mut j = 3;
    while r.get(j).is_some_and(u8::is_ascii_whitespace) {
        j += 1;
    }
    if r.get(j) == Some(&b'/') {
        j += 1;
        while r.get(j).is_some_and(u8::is_ascii_whitespace) {
            j += 1;
        }
    }
    (r.get(j) == Some(&b'>')).then_some(i + j + 1)
}

fn node_size(n: &Node) -> (f64, f64) {
    let lines = split_br(&n.label);
    let longest = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let extra_lines = lines.len().saturating_sub(1) as f64;
    let mut w = (longest * CHAR_W + 28.0).max(58.0);
    let mut h = NODE_H + extra_lines * LINE_H;
    match n.shape {
        Shape::Circle => {
            let d = w.max(h + 16.0);
            w = d;
            h = d;
        }
        Shape::Rhombus | Shape::Hexagon => {
            w += 18.0;
            h += 6.0;
        }
        Shape::Parallelogram | Shape::ParallelogramAlt | Shape::Trapezoid | Shape::TrapezoidAlt => {
            // Both slanted sides eat into the text box; widen by the total skew
            // (2 × the per-side `SKEW` used at render time, = 0.7·h).
            w += h * 0.7;
        }
        // Room for the two inset side bars / the top & bottom cylinder caps.
        Shape::Subroutine => w += 16.0,
        Shape::Cylinder => h += 18.0,
        // A pseudo-state dot: a small fixed slot regardless of (empty) label.
        Shape::Point => {
            w = 18.0;
            h = 18.0;
        }
        _ => {}
    }
    (w, h)
}

fn layout(g: &Graph) -> Layout {
    let sizes: Vec<(f64, f64)> = g.nodes.iter().map(node_size).collect();
    layout_with_sizes(g, &sizes)
}

/// Lay out `graph` using caller-supplied node sizes `(w, h)` (parallel to
/// `graph.nodes`) instead of flow's text-based [`node_size`], so a sibling diagram
/// type that draws its own node bodies — e.g. [`super::class`]'s compartment boxes
/// — can reuse the shared layered layout. `graph`'s edges drive only the ranking.
pub(super) fn layout_with_sizes(g: &Graph, sizes: &[(f64, f64)]) -> Layout {
    let n = g.nodes.len();
    let mut rank = vec![0usize; n];
    // Longest-path ranking. A back-edge in a cycle (`A-->B-->A`, common in state
    // diagrams) would otherwise keep pushing ranks up every pass; cap each rank at
    // `n-1` — the most a DAG of `n` nodes can need — so a cycle can't inflate the
    // layout (a true DAG is unaffected: its ranks are already ≤ n-1).
    let max_allowed = n.saturating_sub(1);
    for _ in 0..n {
        let mut changed = false;
        for e in &g.edges {
            if e.from == e.to {
                continue;
            }
            let new = (rank[e.from] + 1).min(max_allowed);
            if rank[e.to] < new {
                rank[e.to] = new;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let max_rank = rank.iter().copied().max().unwrap_or(0);
    let mut ranks: Vec<Vec<usize>> = vec![Vec::new(); max_rank + 1];
    for (i, &r) in rank.iter().enumerate() {
        ranks[r].push(i);
    }

    let horizontal = matches!(g.dir, Dir::Right | Dir::Left);
    let depth_of = |i: usize| if horizontal { sizes[i].0 } else { sizes[i].1 };
    let breadth_of = |i: usize| if horizontal { sizes[i].1 } else { sizes[i].0 };

    let mut depth_center = vec![0.0f64; ranks.len()];
    let mut acc = MARGIN;
    for (r, slab) in ranks.iter().enumerate() {
        let slab_depth = slab
            .iter()
            .map(|&i| depth_of(i))
            .fold(0.0, f64::max)
            .max(NODE_H);
        depth_center[r] = acc + slab_depth / 2.0;
        acc += slab_depth + DEPTH_GAP;
    }
    let total_depth = acc - DEPTH_GAP + MARGIN;

    let mut breadth_center = vec![0.0f64; n];
    let mut row_breadth = vec![0.0f64; ranks.len()];
    for (r, slab) in ranks.iter().enumerate() {
        let mut b = 0.0f64;
        for &i in slab {
            breadth_center[i] = b + breadth_of(i) / 2.0;
            b += breadth_of(i) + BREADTH_GAP;
        }
        row_breadth[r] = (b - BREADTH_GAP).max(0.0);
    }
    let max_breadth = row_breadth.iter().copied().fold(0.0, f64::max);
    for (r, slab) in ranks.iter().enumerate() {
        let off = MARGIN + (max_breadth - row_breadth[r]) / 2.0;
        for &i in slab {
            breadth_center[i] += off;
        }
    }
    let total_breadth = max_breadth + 2.0 * MARGIN;

    let mut placed = Vec::with_capacity(n);
    for i in 0..n {
        let mut d = depth_center[rank[i]];
        if matches!(g.dir, Dir::Up | Dir::Left) {
            d = total_depth - d;
        }
        let b = breadth_center[i];
        let (cx, cy) = if horizontal { (d, b) } else { (b, d) };
        placed.push(Placed {
            cx,
            cy,
            w: sizes[i].0,
            h: sizes[i].1,
        });
    }
    let (width, height) = if horizontal {
        (total_depth, total_breadth)
    } else {
        (total_breadth, total_depth)
    };
    Layout {
        nodes: placed,
        width,
        height,
    }
}

// ---- rendering -----------------------------------------------------------------

fn render(l: &Layout, g: &Graph) -> String {
    let mut out = String::with_capacity(512 + g.nodes.len() * 160 + g.edges.len() * 120);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {:.1} {:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"15\">",
        l.width.max(1.0),
        l.height.max(1.0)
    ));
    out.push_str(
        // `auto-start-reverse` lets the one marker serve both ends: forward at
        // `marker-end`, flipped at `marker-start` (a `<-->` back-arrow).
        "<defs><marker id=\"cm-arrow\" markerWidth=\"9\" markerHeight=\"9\" refX=\"8\" refY=\"3\" \
         orient=\"auto-start-reverse\" markerUnits=\"userSpaceOnUse\">\
         <path d=\"M0,0 L8,3 L0,6 z\" fill=\"#64748b\"/></marker></defs>",
    );

    for e in &g.edges {
        render_edge(e, l, &mut out);
    }
    for (i, node) in g.nodes.iter().enumerate() {
        render_node(&l.nodes[i], node, &mut out);
    }
    out.push_str("</svg>");
    out
}

fn render_edge(e: &Edge, l: &Layout, out: &mut String) {
    let (a, b) = (&l.nodes[e.from], &l.nodes[e.to]);
    let (x1, y1) = box_exit(a, b.cx, b.cy);
    let (x2, y2) = box_exit(b, a.cx, a.cy);
    let dash = match e.style {
        Style::Dotted => " stroke-dasharray=\"4 4\"",
        _ => "",
    };
    let width = if e.style == Style::Thick { 3.0 } else { 1.6 };
    let mut marker = String::new();
    if e.start_arrow {
        marker.push_str(" marker-start=\"url(#cm-arrow)\"");
    }
    if e.arrow {
        marker.push_str(" marker-end=\"url(#cm-arrow)\"");
    }
    out.push_str(&format!(
        "<line x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\" \
         stroke=\"#64748b\" stroke-width=\"{width}\"{dash}{marker}/>"
    ));
    if !e.label.is_empty() {
        let mx = (x1 + x2) / 2.0;
        let my = (y1 + y2) / 2.0;
        let half = e.label.chars().count() as f64 * CHAR_W / 2.0 + 4.0;
        out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"18\" fill=\"#ffffff\" \
             opacity=\"0.85\"/>",
            mx - half,
            my - 9.0,
            half * 2.0
        ));
        out.push_str(&format!(
            "<text x=\"{mx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#475569\" \
             font-size=\"13\">",
            my + 4.0
        ));
        escape_text(out, &e.label);
        out.push_str("</text>");
    }
}

/// Point where the segment from `n`'s centre toward `(tx,ty)` exits `n`'s box.
pub(super) fn box_exit(n: &Placed, tx: f64, ty: f64) -> (f64, f64) {
    let dx = tx - n.cx;
    let dy = ty - n.cy;
    if dx == 0.0 && dy == 0.0 {
        return (n.cx, n.cy);
    }
    let hw = n.w / 2.0;
    let hh = n.h / 2.0;
    let sx = if dx != 0.0 {
        hw / dx.abs()
    } else {
        f64::INFINITY
    };
    let sy = if dy != 0.0 {
        hh / dy.abs()
    } else {
        f64::INFINITY
    };
    let s = sx.min(sy);
    (n.cx + dx * s, n.cy + dy * s)
}

fn render_node(p: &Placed, n: &Node, out: &mut String) {
    let (x, y) = (p.cx - p.w / 2.0, p.cy - p.h / 2.0);
    let fill = "#eff6ff";
    let stroke = "#3b82f6";
    match n.shape {
        Shape::Rect => {
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"3\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.w, p.h
            ));
        }
        Shape::Round | Shape::Stadium => {
            let r = if n.shape == Shape::Stadium {
                p.h / 2.0
            } else {
                10.0
            };
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"{r:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.w, p.h
            ));
        }
        Shape::Circle => {
            out.push_str(&format!(
                "<ellipse cx=\"{:.1}\" cy=\"{:.1}\" rx=\"{:.1}\" ry=\"{:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.cx,
                p.cy,
                p.w / 2.0,
                p.h / 2.0
            ));
        }
        Shape::Rhombus => {
            out.push_str(&format!(
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.cx,
                p.cy - p.h / 2.0,
                p.cx + p.w / 2.0,
                p.cy,
                p.cx,
                p.cy + p.h / 2.0,
                p.cx - p.w / 2.0,
                p.cy
            ));
        }
        Shape::Hexagon => {
            let q = p.w * 0.22;
            out.push_str(&format!(
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                x + q, y,
                x + p.w - q, y,
                x + p.w, p.cy,
                x + p.w - q, y + p.h,
                x + q, y + p.h,
                x, p.cy
            ));
        }
        Shape::Parallelogram | Shape::ParallelogramAlt | Shape::Trapezoid | Shape::TrapezoidAlt => {
            // Per-side horizontal skew; bounded so a very flat box can't self-cross.
            let s = (p.h * 0.35).min(p.w * 0.45);
            let (x0, x1, y0, y1) = (x, x + p.w, y, y + p.h);
            // Four corners clockwise from top-left, slants applied per shape.
            let pts = match n.shape {
                Shape::Parallelogram => [(x0 + s, y0), (x1, y0), (x1 - s, y1), (x0, y1)],
                Shape::ParallelogramAlt => [(x0, y0), (x1 - s, y0), (x1, y1), (x0 + s, y1)],
                Shape::Trapezoid => [(x0 + s, y0), (x1 - s, y0), (x1, y1), (x0, y1)],
                _ => [(x0, y0), (x1, y0), (x1 - s, y1), (x0 + s, y1)], // TrapezoidAlt
            };
            out.push_str(&format!(
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                pts[0].0, pts[0].1, pts[1].0, pts[1].1, pts[2].0, pts[2].1, pts[3].0, pts[3].1
            ));
        }
        Shape::Subroutine => {
            out.push_str(&format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"2\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.w, p.h
            ));
            // Two vertical bars inset from each side mark the subroutine frame.
            let inset = 7.0;
            for bx in [x + inset, x + p.w - inset] {
                out.push_str(&format!(
                    "<line x1=\"{bx:.1}\" y1=\"{y:.1}\" x2=\"{bx:.1}\" y2=\"{:.1}\" \
                     stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                    y + p.h
                ));
            }
        }
        Shape::Cylinder => {
            let rx = p.w / 2.0;
            let ry = (p.h * 0.14).clamp(5.0, 12.0);
            let (top, bot, xr) = (y + ry, y + p.h - ry, x + p.w);
            // Body: down the left side, front bottom arc (bulges down), up the
            // right side, straight back across the top (hidden by the rim ellipse).
            out.push_str(&format!(
                "<path d=\"M {x:.1},{top:.1} L {x:.1},{bot:.1} \
                 A {rx:.1},{ry:.1} 0 0 0 {xr:.1},{bot:.1} L {xr:.1},{top:.1} Z\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>"
            ));
            // Full top-rim ellipse drawn over the body.
            out.push_str(&format!(
                "<ellipse cx=\"{:.1}\" cy=\"{top:.1}\" rx=\"{rx:.1}\" ry=\"{ry:.1}\" \
                 fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.5\"/>",
                p.cx
            ));
        }
        Shape::Point => {
            // A small filled dot (state-diagram `[*]`); no label.
            out.push_str(&format!(
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"6\" fill=\"{stroke}\" stroke=\"{stroke}\"/>",
                p.cx, p.cy
            ));
            return;
        }
    }
    let lines = split_br(&n.label);
    // Vertically centre the stack of baselines around the single-line offset.
    let top = p.cy + 5.0 - (lines.len() as f64 - 1.0) * LINE_H / 2.0;
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{top:.1}\" text-anchor=\"middle\" fill=\"#1e293b\">",
        p.cx
    ));
    for (k, line) in lines.iter().enumerate() {
        // First line rides the `<text>` element; the rest get an *absolute* `y`.
        // A relative `dy` would be dropped on a blank interior line's empty
        // `<tspan>` (the SVG text model applies dx/dy to the first rendered glyph,
        // of which an empty tspan has none), collapsing `a<br><br>b`'s middle gap.
        if k > 0 {
            let ly = top + k as f64 * LINE_H;
            out.push_str(&format!("<tspan x=\"{:.1}\" y=\"{ly:.1}\">", p.cx));
        }
        escape_text(out, line);
        if k > 0 {
            out.push_str("</tspan>");
        }
    }
    out.push_str("</text>");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_flowchart_renders_svg() {
        let svg =
            to_svg("graph TD\n  A[Start] --> B{OK?}\n  B -->|yes| C[Done]\n  B -->|no| A").unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("</svg>"));
        assert!(svg.contains(">Start</text>"));
        assert!(svg.contains(">OK?</text>"));
        assert!(svg.contains(">yes</text>"));
        assert!(svg.contains("marker-end"));
        assert!(svg.contains("<polygon"));
    }

    #[test]
    fn one_line_header_form() {
        let svg = to_svg("graph LR; A-->B-->C").unwrap();
        assert!(svg.contains(">A</text>"));
        assert!(svg.contains(">B</text>"));
        assert!(svg.contains(">C</text>"));
        assert_eq!(svg.matches("marker-end").count(), 2);
    }

    #[test]
    fn direction_token_is_not_a_node() {
        let svg = to_svg("graph LR\n  A[Input] --> B[End]").unwrap();
        assert_eq!(svg.matches(">LR</text>").count(), 0, "{svg}");
        assert!(
            svg.contains(">Input</text>") && svg.contains(">End</text>"),
            "{svg}"
        );
        for d in ["graph TD", "flowchart RL", "graph BT", "flowchart LR"] {
            let svg = to_svg(&format!("{d}\n X --> Y")).unwrap();
            assert_eq!(svg.matches("<rect").count(), 2, "{d}: {svg}");
        }
    }

    #[test]
    fn shapes_and_styles() {
        let svg = to_svg("flowchart TD\nA((circle)) --- B([stadium])\nA -.-> C(round)").unwrap();
        assert!(svg.contains("<ellipse"));
        assert!(svg.contains("stroke-dasharray"));
    }

    #[test]
    fn parallelogram_and_trapezoid_render_as_polygons() {
        // Two parallelograms (lean right / left) and two trapezoids (down / up).
        let svg =
            to_svg("flowchart TD\n A[/io/] --> B[\\alt\\]\n B --> C[/trap\\]\n C --> D[\\up/]")
                .unwrap();
        // Each skewed node is a 4-point polygon (no rhombus/hexagon here).
        assert_eq!(svg.matches("<polygon").count(), 4, "{svg}");
        // Labels render as clean text — the slashes are delimiters, not content.
        for label in ["io", "alt", "trap", "up"] {
            assert!(
                svg.contains(&format!(">{label}</text>")),
                "missing {label}: {svg}"
            );
        }
        assert!(
            !svg.contains("/io/") && !svg.contains("trap\\"),
            "slashes leaked: {svg}"
        );
        // No node is a <rect> — all four are skewed polygons.
        assert_eq!(svg.matches("<rect").count(), 0, "{svg}");
    }

    #[test]
    fn subroutine_and_cylinder_render_their_own_shapes() {
        let svg = to_svg("flowchart LR\n A[[Subroutine]] --> B[(Database)]").unwrap();
        // Subroutine: a rect plus its two inset side bars (two extra <line>s on top
        // of any edge lines); cylinder: a <path> body + a top-rim <ellipse>.
        assert!(svg.contains("<path "), "cylinder body path missing: {svg}");
        assert!(svg.contains("<ellipse"), "cylinder rim missing: {svg}");
        // Labels are clean — the doubled brackets / parens are delimiters, not text.
        assert!(svg.contains(">Subroutine</text>"), "{svg}");
        assert!(svg.contains(">Database</text>"), "{svg}");
        assert!(
            !svg.contains("(Database)") && !svg.contains("[Subroutine"),
            "delims leaked: {svg}"
        );
        // The subroutine's two inset bars are vertical lines spanning the box top→bottom;
        // there are more <line>s than the single connecting edge would produce alone.
        assert!(
            svg.matches("<line").count() >= 3,
            "subroutine bars missing: {svg}"
        );
    }

    #[test]
    fn skewed_node_honours_quoted_slashes() {
        // A quote protects inner slashes; only the trailing `/]` closes the node.
        let svg = to_svg("flowchart LR\n A[/\"a/b/c\"/] --> B").unwrap();
        assert!(svg.contains(">a/b/c</text>"), "{svg}");
        assert_eq!(svg.matches("<polygon").count(), 1, "{svg}");
    }

    #[test]
    fn ampersand_node_groups_fan_in_and_out() {
        // `A & B --> C` fans in (A→C, B→C); `A --> B & C` fans out (A→B, A→C).
        let svg = to_svg("flowchart LR\n A & B --> C").unwrap();
        assert_eq!(svg.matches("marker-end").count(), 2, "fan-in: {svg}");
        let svg2 = to_svg("flowchart LR\n A --> B & C").unwrap();
        assert_eq!(svg2.matches("marker-end").count(), 2, "fan-out: {svg2}");
        // Group × group is the cartesian product: 2 & 2 → 4 edges, all 4 nodes present.
        let svg3 = to_svg("flowchart LR\n A & B --> C & D").unwrap();
        assert_eq!(svg3.matches("marker-end").count(), 4, "{svg3}");
        for n in ["A", "B", "C", "D"] {
            assert!(
                svg3.contains(&format!(">{n}</text>")),
                "{n} missing: {svg3}"
            );
        }
        // A shape/label on a grouped node still parses.
        let svg4 = to_svg("flowchart TD\n A[Start] & B[Init] --> C{ok?}").unwrap();
        assert!(
            svg4.contains(">Start</text>") && svg4.contains(">ok?</text>"),
            "{svg4}"
        );
    }

    #[test]
    fn bidirectional_edge_has_arrows_at_both_ends() {
        let svg = to_svg("flowchart LR\n A <--> B").unwrap();
        assert_eq!(svg.matches("marker-start").count(), 1, "back-arrow: {svg}");
        assert_eq!(svg.matches("marker-end").count(), 1, "forward arrow: {svg}");
        // A plain `-->` has only the forward arrow (no back-arrow).
        let one = to_svg("flowchart LR\n A --> B").unwrap();
        assert_eq!(one.matches("marker-start").count(), 0, "{one}");
        assert_eq!(one.matches("marker-end").count(), 1, "{one}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("graph TD\n A[\"<script>x</script>\"] --> B").unwrap();
        assert!(!svg.contains("<script>"));
        assert!(svg.contains("&lt;script&gt;"));
    }

    #[test]
    fn br_tags_split_node_label_into_lines() {
        for src in [
            "graph TD\n A[\"First<br/>Second\"]",
            "graph TD\n A[First<br>Second]",
            "graph TD\n A[First<br />Second]",
            "graph TD\n A[First<br   />Second]",
            "graph TD\n A[\"First<BR/>Second\"]",
        ] {
            let svg = to_svg(src).unwrap();
            assert!(svg.contains(">First<tspan"), "{src}: {svg}");
            assert!(svg.contains(">Second</tspan>"), "{src}: {svg}");
            // The tag is consumed, not emitted literally or as escaped text.
            assert!(
                !svg.contains("br&gt;") && !svg.contains("<br"),
                "{src}: {svg}"
            );
        }
    }

    #[test]
    fn single_line_label_emits_no_tspan() {
        let svg = to_svg("graph TD\n A[Plain]").unwrap();
        assert!(svg.contains(">Plain</text>"), "{svg}");
        assert!(!svg.contains("<tspan"), "{svg}");
    }

    #[test]
    fn br_lines_are_individually_escaped() {
        let svg = to_svg("graph TD\n A[\"<b>x</b><br/><i>y</i>\"]").unwrap();
        assert!(!svg.contains("<b>") && !svg.contains("<i>"), "{svg}");
        assert!(svg.contains("&lt;b&gt;x&lt;/b&gt;"), "{svg}");
        assert!(svg.contains("&lt;i&gt;y&lt;/i&gt;"), "{svg}");
    }

    #[test]
    fn consecutive_br_keeps_blank_middle_line_spaced() {
        // `a<br><br>b` is three lines (a, blank, b). The blank middle must still
        // advance the baseline stack: each later line carries an absolute `y`, so
        // the "b" baseline sits a full two line-heights below "a" (a relative `dy`
        // on the empty middle tspan would be dropped and collapse the gap).
        let svg = to_svg("graph TD\n A[\"a<br/><br/>b\"]").unwrap();
        // Two non-first lines ⇒ two tspans (the middle one empty for the blank).
        assert_eq!(svg.matches("<tspan").count(), 2, "{svg}");
        // Baselines, in order, from the `<text>`…`</text>` block: [top, top+H, top+2H].
        let block = {
            let s = svg.find("<text ").unwrap();
            let e = svg[s..].find("</text>").unwrap() + s;
            &svg[s..e]
        };
        let ys: Vec<f64> = block
            .split("y=\"")
            .skip(1)
            .map(|p| p.split('"').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(ys.len(), 3, "expected 3 baselines: {svg}");
        for w in ys.windows(2) {
            assert!(
                (w[1] - w[0] - LINE_H).abs() < 0.01,
                "uneven baselines {ys:?}: {svg}"
            );
        }
    }

    #[test]
    fn edge_br_does_not_add_blank_lines() {
        let vb_h = |s: &str| -> f64 {
            s.split("viewBox=\"0 0 ")
                .nth(1)
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .trim_end_matches('"')
                .parse()
                .unwrap()
        };
        let bare = to_svg("graph TD\n A[a]").unwrap();
        // A leading or trailing `<br>` is not an extra (blank) line: single-line
        // box, same height as the bare label, no tspan.
        for src in ["graph TD\n A[\"a<br/>\"]", "graph TD\n A[\"<br/>a\"]"] {
            let svg = to_svg(src).unwrap();
            assert!(
                !svg.contains("<tspan"),
                "{src} should be single-line: {svg}"
            );
            assert!((vb_h(&svg) - vb_h(&bare)).abs() < 0.01, "{src}: {svg}");
        }
        // A lone `<br>` collapses to a single blank line — a normal-height box.
        let lone = to_svg("graph TD\n A[\"<br/>\"]").unwrap();
        assert!(!lone.contains("<tspan"), "{lone}");
    }

    #[test]
    fn multiline_label_grows_node_height() {
        let one = to_svg("graph TD\n A[One]").unwrap();
        let two = to_svg("graph TD\n A[\"One<br/>Two\"]").unwrap();
        // Taller box ⇒ larger viewBox height than the single-line case.
        let h = |s: &str| {
            s.split("viewBox=\"0 0 ")
                .nth(1)
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .trim_end_matches('"')
                .parse::<f64>()
                .unwrap()
        };
        assert!(
            h(&two) > h(&one),
            "two-line {} should exceed one-line {}",
            h(&two),
            h(&one)
        );
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "graph",
            "graph TD",
            "graph TD\nA[",
            "graph TD\nA --> ",
            "graph TD\n-->",
            "graph TD\nA --> A",
            "graph LR\nA-->B-->C-->A",
            "flowchart\n{{{{",
            "graph TD\nA{{{{x",
            "graph TD\nA[/unterminated",
            "graph TD\nA[\\also/",
            "graph TD\nA[/\"unterminated quote",
            "graph TD\nA[/]",
            "graph TD\nA[[unterminated",
            "graph TD\nA[(unterminated",
            "graph TD\nA[(])",
            "graph TD\nA & ",      // trailing `&` with no node
            "graph TD\nA & & B",   // doubled `&`
            "graph TD\n& A",       // leading `&`
            "graph TD\nA & --> B", // `&` then an edge
        ] {
            let _ = to_svg(s);
        }
    }
}
