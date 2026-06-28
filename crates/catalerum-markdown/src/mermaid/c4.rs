//! C4 model diagrams → SVG. Shares one grammar across the `C4Context`,
//! `C4Container`, `C4Component` and `C4Dynamic` headers. Supported statements:
//!
//! * **Elements** — `Person(alias, label, ?descr)`, `Person_Ext`, `System(…)`,
//!   `System_Ext`, `SystemDb`, `SystemQueue`, `Container(…)`, `ContainerDb`,
//!   `Container_Ext`, `Component(…)` and friends. Any unknown `Xxx(…)` form is a
//!   generic labelled box tagged with its keyword (forward-compatible).
//! * **Boundaries** — `Enterprise_Boundary(alias, label) { … }`,
//!   `System_Boundary`, `Container_Boundary` (and generic `Boundary`), rendered as
//!   nested dashed rectangles. Elements/relations inside belong to the boundary.
//! * **Relations** — `Rel(from, to, label, ?tech)`, `BiRel`, `Rel_Back` and the
//!   directional `Rel_U/D/L/R` (the direction hint is ignored for layout).
//!
//! Layout is a fixed grid of rows within each boundary (C4 tools use grids, not
//! flow layout); relations are straight edges between box borders with the label
//! (and `[tech]`) at the midpoint. External (`_Ext`) elements render muted. All
//! text is escaped, so the result is safe to inject.

use super::MermaidError;
use crate::escape::escape_text;

const MARGIN: f64 = 18.0;
const PAD_X: f64 = 12.0;
const PAD_Y: f64 = 8.0;
const TAG_H: f64 = 13.0;
const LABEL_H: f64 = 16.0;
const EXTRA_H: f64 = 13.0;
const HEAD_SPACE: f64 = 16.0; // room above a Person box for the head glyph
const MIN_W: f64 = 100.0;
const MAX_W: f64 = 200.0;
const H_GAP: f64 = 26.0;
const V_GAP: f64 = 24.0;
const COLS: usize = 3;
const BPAD: f64 = 16.0; // boundary inner padding
const BTITLE: f64 = 26.0; // boundary title strip height
const MIN_BW: f64 = 130.0;
const MAX_DEPTH: usize = 24;

const INT_FILL: &str = "#1168bd";
const INT_STROKE: &str = "#0b5394";
const PERSON_FILL: &str = "#08427b";
const PERSON_STROKE: &str = "#052e56";
const EXT_FILL: &str = "#8a94a6";
const EXT_STROKE: &str = "#6b7280";
const TAG_FILL: &str = "#cbd8ea";

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let m = parse(src);
    if m.nodes.len() <= 1 {
        return Err(MermaidError("empty C4 diagram"));
    }
    Ok(render(m))
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Person,
    Db,
    Queue,
    Boundary,
    Box,
}

struct Node {
    keyword: String,
    alias: String,
    label: String,
    extras: Vec<String>,
    shape: Shape,
    ext: bool,
    is_boundary: bool,
    children: Vec<usize>,
    w: f64,
    h: f64,
    rx: f64, // position relative to the parent boundary's top-left
    ry: f64,
    x: f64, // absolute
    y: f64,
}

#[derive(Clone, Copy, PartialEq)]
enum RelKind {
    Forward,
    Back,
    Bi,
}

struct Rel {
    from: usize,
    to: usize,
    label: String,
    tech: Option<String>,
    kind: RelKind,
}

struct Model {
    nodes: Vec<Node>,
    rels: Vec<Rel>,
}

impl Node {
    fn display(&self) -> &str {
        if !self.label.is_empty() {
            &self.label
        } else {
            &self.alias
        }
    }
    fn tech_first(&self) -> bool {
        self.keyword.contains("Container") || self.keyword.contains("Component")
    }
}

fn shape_of(keyword: &str) -> Shape {
    if keyword.starts_with("Person") {
        Shape::Person
    } else if keyword.contains("Boundary") {
        Shape::Boundary
    } else if keyword.contains("Db") {
        Shape::Db
    } else if keyword.contains("Queue") {
        Shape::Queue
    } else {
        Shape::Box
    }
}

/// Split a comma list at the top level (commas inside `"…"` are kept), dropping
/// the surrounding quotes and any `$tags`/`$link` named arguments.
fn split_args(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for c in s.chars() {
        match c {
            '"' => in_q = !in_q,
            ',' if !in_q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.starts_with('$'))
        .collect()
}

fn parse(src: &str) -> Model {
    let mut nodes: Vec<Node> = vec![Node {
        keyword: String::new(),
        alias: String::new(),
        label: String::new(),
        extras: Vec::new(),
        shape: Shape::Boundary,
        ext: false,
        is_boundary: true,
        children: Vec::new(),
        w: 0.0,
        h: 0.0,
        rx: 0.0,
        ry: 0.0,
        x: 0.0,
        y: 0.0,
    }];
    let mut rels: Vec<Rel> = Vec::new();
    let mut alias_ix: Vec<(String, usize)> = Vec::new();
    let mut stack: Vec<usize> = vec![0]; // boundary nesting; 0 = implicit root
    let mut suppressed = 0usize; // boundaries opened past MAX_DEPTH (kept balanced)

    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`C4Context`, `C4Container`, …)

    for line in lines {
        if line.starts_with('}') {
            if suppressed > 0 {
                suppressed -= 1;
            } else if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let Some(open) = line.find('(') else {
            continue; // `title …`, config, stray `{` — nothing to draw
        };
        let Some(close) = line.rfind(')') else {
            continue;
        };
        if close < open {
            continue;
        }
        let keyword = line[..open].trim();
        if keyword.is_empty() || !keyword.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let args = split_args(&line[open + 1..close]);

        if keyword.contains("Boundary") {
            let alias = args.first().cloned().unwrap_or_default();
            let label = args
                .get(1)
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| alias.clone());
            let cur = *stack.last().unwrap();
            if suppressed > 0 || stack.len() >= MAX_DEPTH {
                // Too deep: keep it as a leaf so brace balance (and recursion) is safe.
                let idx = push_node(&mut nodes, keyword, alias, label, Vec::new());
                nodes[cur].children.push(idx);
                suppressed += 1;
            } else {
                let idx = nodes.len();
                nodes.push(Node {
                    keyword: keyword.to_string(),
                    alias: alias.clone(),
                    label,
                    extras: Vec::new(),
                    shape: Shape::Boundary,
                    ext: keyword.contains("_Ext"),
                    is_boundary: true,
                    children: Vec::new(),
                    w: 0.0,
                    h: 0.0,
                    rx: 0.0,
                    ry: 0.0,
                    x: 0.0,
                    y: 0.0,
                });
                nodes[cur].children.push(idx);
                if !alias.is_empty() {
                    alias_ix.push((alias, idx));
                }
                stack.push(idx);
            }
        } else if keyword.starts_with("Rel") || keyword.starts_with("BiRel") {
            if args.len() < 2 {
                continue;
            }
            let (Some(&from), Some(&to)) = (
                alias_ix.iter().find(|(a, _)| *a == args[0]).map(|(_, i)| i),
                alias_ix.iter().find(|(a, _)| *a == args[1]).map(|(_, i)| i),
            ) else {
                continue;
            };
            let kind = if keyword.starts_with("BiRel") {
                RelKind::Bi
            } else if keyword.starts_with("Rel_Back") {
                RelKind::Back
            } else {
                RelKind::Forward
            };
            rels.push(Rel {
                from,
                to,
                label: args.get(2).cloned().unwrap_or_default(),
                tech: args.get(3).cloned().filter(|s| !s.is_empty()),
                kind,
            });
        } else if keyword.starts_with("Update") {
            // `UpdateElementStyle`, `UpdateRelStyle`, `UpdateLayoutConfig` — styling only.
            continue;
        } else {
            let Some(alias) = args.first().filter(|a| !a.is_empty()).cloned() else {
                continue;
            };
            let label = args
                .get(1)
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| alias.clone());
            let extras: Vec<String> = args.into_iter().skip(2).filter(|s| !s.is_empty()).collect();
            let cur = *stack.last().unwrap();
            let idx = push_node(&mut nodes, keyword, alias.clone(), label, extras);
            nodes[cur].children.push(idx);
            alias_ix.push((alias, idx));
        }
    }
    Model { nodes, rels }
}

/// Push a non-boundary element node, deriving its shape and external flag.
fn push_node(
    nodes: &mut Vec<Node>,
    keyword: &str,
    alias: String,
    label: String,
    extras: Vec<String>,
) -> usize {
    let idx = nodes.len();
    nodes.push(Node {
        shape: shape_of(keyword),
        ext: keyword.contains("_Ext"),
        keyword: keyword.to_string(),
        alias,
        label,
        extras,
        is_boundary: false,
        children: Vec::new(),
        w: 0.0,
        h: 0.0,
        rx: 0.0,
        ry: 0.0,
        x: 0.0,
        y: 0.0,
    });
    idx
}

// ---- layout --------------------------------------------------------------------

fn label_w(s: &str) -> f64 {
    s.chars().count() as f64 * 7.2
}

fn small_w(s: &str) -> f64 {
    s.chars().count() as f64 * 6.0
}

fn element_size(n: &Node) -> (f64, f64) {
    let tag = format!("«{}»", n.keyword);
    let mut content_w = small_w(&tag).max(label_w(n.display()));
    for e in &n.extras {
        content_w = content_w.max(small_w(e));
    }
    let w = (content_w + 2.0 * PAD_X).clamp(MIN_W, MAX_W);
    let mut h = 2.0 * PAD_Y + TAG_H + LABEL_H + n.extras.len() as f64 * EXTRA_H;
    if n.shape == Shape::Person {
        h += HEAD_SPACE;
    }
    (w, h)
}

/// Grid-pack `kids` into rows of `COLS`, setting each child's `rx`/`ry` and
/// returning the content's `(right, bottom)` extent within the boundary.
fn pack(nodes: &mut [Node], kids: &[usize], ox: f64, oy: f64) -> (f64, f64) {
    let mut y = oy;
    let mut max_right = ox;
    let mut i = 0;
    while i < kids.len() {
        let end = (i + COLS).min(kids.len());
        let row_h = kids[i..end].iter().map(|&c| nodes[c].h).fold(0.0, f64::max);
        let mut x = ox;
        for &c in &kids[i..end] {
            nodes[c].rx = x;
            nodes[c].ry = y;
            x += nodes[c].w + H_GAP;
            max_right = max_right.max(nodes[c].rx + nodes[c].w);
        }
        y += row_h + V_GAP;
        i = end;
    }
    let bottom = if kids.is_empty() { oy } else { y - V_GAP };
    (max_right, bottom)
}

fn layout(nodes: &mut Vec<Node>, idx: usize) {
    let kids = nodes[idx].children.clone();
    for &c in &kids {
        if nodes[c].is_boundary {
            layout(nodes, c);
        } else {
            let (w, h) = element_size(&nodes[c]);
            nodes[c].w = w;
            nodes[c].h = h;
        }
    }
    let is_root = idx == 0;
    let (pad, title_h) = if is_root { (0.0, 0.0) } else { (BPAD, BTITLE) };
    let (right, bottom) = pack(nodes, &kids, pad, pad + title_h);
    if is_root {
        nodes[idx].w = right;
        nodes[idx].h = bottom;
    } else {
        let title_w = label_w(nodes[idx].display()) + 24.0;
        nodes[idx].w = (right + pad).max(title_w).max(MIN_BW);
        nodes[idx].h = (bottom + pad).max(title_h + 24.0);
    }
}

/// Resolve absolute positions from the relative `rx`/`ry` set during packing.
fn place(nodes: &mut [Node]) {
    nodes[0].x = MARGIN;
    nodes[0].y = MARGIN;
    let mut stack = vec![0usize];
    while let Some(u) = stack.pop() {
        let (ux, uy) = (nodes[u].x, nodes[u].y);
        let kids = nodes[u].children.clone();
        for &c in &kids {
            nodes[c].x = ux + nodes[c].rx;
            nodes[c].y = uy + nodes[c].ry;
            stack.push(c);
        }
    }
}

// ---- rendering -----------------------------------------------------------------

fn render(mut m: Model) -> String {
    layout(&mut m.nodes, 0);
    place(&mut m.nodes);

    let width = 2.0 * MARGIN + m.nodes[0].w;
    let height = 2.0 * MARGIN + m.nodes[0].h;

    let mut out = String::with_capacity(512 + m.nodes.len() * 220);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-c4\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"12\">"
    ));

    // Boundaries (outer first — parent nodes precede their children in the arena).
    for n in m.nodes.iter().skip(1).filter(|n| n.is_boundary) {
        out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"6\" fill=\"none\" \
             stroke=\"#64748b\" stroke-width=\"1.4\" stroke-dasharray=\"6 4\"/>",
            n.x, n.y, n.w, n.h
        ));
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" font-size=\"12\" font-weight=\"bold\" fill=\"#475569\">",
            n.x + 10.0,
            n.y + 17.0
        ));
        escape_text(&mut out, n.display());
        out.push_str("</text>");
    }

    // Element boxes on top of the boundaries.
    for n in m.nodes.iter().skip(1).filter(|n| !n.is_boundary) {
        draw_element(&mut out, n);
    }

    // Relations on top of everything.
    for r in &m.rels {
        draw_rel(&mut out, &m.nodes[r.from], &m.nodes[r.to], r);
    }

    out.push_str("</svg>");
    out
}

fn draw_element(out: &mut String, n: &Node) {
    let (fill, stroke) = if n.ext {
        (EXT_FILL, EXT_STROKE)
    } else if n.shape == Shape::Person {
        (PERSON_FILL, PERSON_STROKE)
    } else {
        (INT_FILL, INT_STROKE)
    };
    let style = format!("fill=\"{fill}\" stroke=\"{stroke}\" stroke-width=\"1.2\"");
    let box_top = if n.shape == Shape::Person {
        n.y + HEAD_SPACE
    } else {
        n.y
    };
    let box_h = n.h - (box_top - n.y);

    match n.shape {
        Shape::Person => {
            let cx = n.x + n.w / 2.0;
            let hr = 7.0;
            out.push_str(&format!(
                "<circle cx=\"{cx:.1}\" cy=\"{:.1}\" r=\"{hr:.1}\" {style}/>",
                n.y + hr + 2.0
            ));
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{box_top:.1}\" width=\"{:.1}\" height=\"{box_h:.1}\" rx=\"5\" {style}/>",
                n.x, n.w
            ));
        }
        Shape::Queue => out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{box_top:.1}\" width=\"{:.1}\" height=\"{box_h:.1}\" rx=\"{:.1}\" {style}/>",
            n.x,
            n.w,
            box_h / 2.0
        )),
        Shape::Db => {
            out.push_str(&format!(
                "<rect x=\"{:.1}\" y=\"{box_top:.1}\" width=\"{:.1}\" height=\"{box_h:.1}\" rx=\"4\" {style}/>",
                n.x, n.w
            ));
            // A faint ellipse near the top hints at a database cylinder.
            out.push_str(&format!(
                "<path d=\"M{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"none\" \
                 stroke=\"#ffffff\" stroke-opacity=\"0.45\"/>",
                n.x + 4.0,
                box_top + 9.0,
                n.x + 4.0,
                box_top + 16.0,
                n.x + n.w - 4.0,
                box_top + 16.0,
                n.x + n.w - 4.0,
                box_top + 9.0
            ));
        }
        _ => out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{box_top:.1}\" width=\"{:.1}\" height=\"{box_h:.1}\" rx=\"4\" {style}/>",
            n.x, n.w
        )),
    }

    let cx = n.x + n.w / 2.0;
    let mut ty = box_top + PAD_Y;
    text_mid(
        out,
        cx,
        ty + 9.0,
        9.5,
        TAG_FILL,
        false,
        &format!("«{}»", n.keyword),
    );
    ty += TAG_H;
    text_mid(out, cx, ty + 11.0, 12.5, "#ffffff", true, n.display());
    ty += LABEL_H;
    for (i, e) in n.extras.iter().enumerate() {
        let s = if n.tech_first() && i == 0 {
            format!("[{e}]")
        } else {
            e.clone()
        };
        text_mid(out, cx, ty + 9.0, 9.5, TAG_FILL, false, &s);
        ty += EXTRA_H;
    }
}

fn draw_rel(out: &mut String, a: &Node, b: &Node, r: &Rel) {
    let (acx, acy) = (a.x + a.w / 2.0, a.y + a.h / 2.0);
    let (bcx, bcy) = (b.x + b.w / 2.0, b.y + b.h / 2.0);
    let (sx, sy) = border(a, bcx, bcy);
    let (ex, ey) = border(b, acx, acy);
    out.push_str(&format!(
        "<line x1=\"{sx:.1}\" y1=\"{sy:.1}\" x2=\"{ex:.1}\" y2=\"{ey:.1}\" stroke=\"#64748b\" \
         stroke-width=\"1.4\"/>"
    ));
    match r.kind {
        RelKind::Forward => arrow(out, sx, sy, ex, ey, "#64748b"),
        RelKind::Back => arrow(out, ex, ey, sx, sy, "#64748b"),
        RelKind::Bi => {
            arrow(out, sx, sy, ex, ey, "#64748b");
            arrow(out, ex, ey, sx, sy, "#64748b");
        }
    }

    let (mx, my) = ((sx + ex) / 2.0, (sy + ey) / 2.0);
    if !r.label.is_empty() {
        let w = label_w(&r.label) + 8.0;
        out.push_str(&format!(
            "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{w:.1}\" height=\"15\" rx=\"3\" fill=\"#ffffff\" \
             fill-opacity=\"0.85\"/>",
            mx - w / 2.0,
            my - 8.0
        ));
        text_mid(out, mx, my + 3.5, 11.0, "#334155", false, &r.label);
    }
    if let Some(tech) = &r.tech {
        text_mid(
            out,
            mx,
            my + 16.0,
            9.0,
            "#64748b",
            false,
            &format!("[{tech}]"),
        );
    }
}

/// Where the segment from `n`'s centre toward `(px, py)` crosses `n`'s border.
fn border(n: &Node, px: f64, py: f64) -> (f64, f64) {
    let (cx, cy) = (n.x + n.w / 2.0, n.y + n.h / 2.0);
    let (dx, dy) = (px - cx, py - cy);
    if dx.abs() < 1e-6 && dy.abs() < 1e-6 {
        return (cx, cy);
    }
    let sx = if dx.abs() > 1e-6 {
        (n.w / 2.0) / dx.abs()
    } else {
        f64::INFINITY
    };
    let sy = if dy.abs() > 1e-6 {
        (n.h / 2.0) / dy.abs()
    } else {
        f64::INFINITY
    };
    let s = sx.min(sy);
    (cx + dx * s, cy + dy * s)
}

fn arrow(out: &mut String, fx: f64, fy: f64, tx: f64, ty: f64, color: &str) {
    let (dx, dy) = (tx - fx, ty - fy);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (size, half) = (9.0, 4.0);
    let (bx, by) = (tx - ux * size, ty - uy * size);
    let (px, py) = (-uy, ux);
    out.push_str(&format!(
        "<polygon points=\"{tx:.1},{ty:.1} {:.1},{:.1} {:.1},{:.1}\" fill=\"{color}\"/>",
        bx + px * half,
        by + py * half,
        bx - px * half,
        by - py * half
    ));
}

fn text_mid(out: &mut String, x: f64, y: f64, size: f64, fill: &str, bold: bool, s: &str) {
    let weight = if bold { "bold" } else { "normal" };
    out.push_str(&format!(
        "<text x=\"{x:.1}\" y=\"{y:.1}\" text-anchor=\"middle\" font-size=\"{size}\" fill=\"{fill}\" \
         font-weight=\"{weight}\">"
    ));
    escape_text(out, s);
    out.push_str("</text>");
}

#[cfg(test)]
mod tests {
    use super::{parse, Shape};
    use crate::mermaid::to_svg;

    #[test]
    fn parses_elements_boundaries_and_relations() {
        let m = parse(
            "C4Context\n Person(u, \"User\", \"A person\")\n \
             System_Boundary(sb, \"Sys\") {\n System(s, \"App\")\n }\n Rel(u, s, \"uses\", \"HTTP\")",
        );
        // root + Person + boundary + System = 4 nodes.
        assert_eq!(m.nodes.len(), 4);
        // Person is a top-level child of root; the System sits inside the boundary.
        assert_eq!(m.nodes[0].children.len(), 2, "root has person + boundary");
        let boundary = &m.nodes[2];
        assert!(boundary.is_boundary);
        assert_eq!(boundary.children.len(), 1, "system nested in boundary");
        // One relation with a tech line.
        assert_eq!(m.rels.len(), 1);
        assert_eq!(m.rels[0].label, "uses");
        assert_eq!(m.rels[0].tech.as_deref(), Some("HTTP"));
    }

    #[test]
    fn renders_boxes_person_boundary_and_arrow() {
        let svg = to_svg(
            "C4Context\n Person(u, \"User\")\n System(s, \"App\")\n Rel(u, s, \"uses\", \"HTTPS\")",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains("catalerum-c4"), "class: {svg}");
        // Person head glyph → a <circle>.
        assert!(svg.contains("<circle"), "person head: {svg}");
        // Relation arrowhead → a <polygon>; label + tech present.
        assert!(svg.contains("<polygon"), "arrowhead: {svg}");
        assert!(svg.contains(">uses</text>"), "rel label: {svg}");
        assert!(svg.contains(">[HTTPS]</text>"), "tech bracket: {svg}");
        // Type tags for the elements.
        assert!(
            svg.contains("«Person»") && svg.contains("«System»"),
            "type tags: {svg}"
        );
        // Labels rendered.
        assert!(
            svg.contains(">User</text>") && svg.contains(">App</text>"),
            "labels: {svg}"
        );
    }

    #[test]
    fn boundary_renders_dashed_rect_with_title() {
        let svg = to_svg("C4Context\n System_Boundary(b, \"Internal\") {\n System(s, \"Svc\")\n }")
            .unwrap();
        assert!(svg.contains("stroke-dasharray"), "dashed boundary: {svg}");
        assert!(svg.contains(">Internal</text>"), "boundary title: {svg}");
    }

    #[test]
    fn external_elements_render_muted() {
        let svg = to_svg("C4Context\n System(s, \"In\")\n System_Ext(e, \"Out\")").unwrap();
        assert!(svg.contains("#8a94a6"), "external muted fill: {svg}");
        assert!(svg.contains("#1168bd"), "internal fill: {svg}");
    }

    #[test]
    fn unknown_element_form_is_a_generic_tagged_box() {
        // A made-up keyword still renders — box with its keyword as the type tag.
        let svg = to_svg("C4Context\n Widget(w, \"Thing\")").unwrap();
        assert!(svg.contains("«Widget»"), "generic type tag: {svg}");
        assert!(svg.contains(">Thing</text>"), "generic label: {svg}");
    }

    #[test]
    fn shape_is_derived_from_keyword() {
        let m = parse(
            "C4Container\n ContainerDb(d, \"DB\")\n SystemQueue(q, \"Q\")\n Person_Ext(p, \"P\")\n System(s, \"S\")",
        );
        let by_alias = |a: &str| m.nodes.iter().find(|n| n.alias == a).unwrap();
        assert!(by_alias("d").shape == Shape::Db);
        assert!(by_alias("q").shape == Shape::Queue);
        assert!(by_alias("p").shape == Shape::Person && by_alias("p").ext);
        assert!(by_alias("s").shape == Shape::Box);
    }

    #[test]
    fn all_four_headers_dispatch() {
        for header in ["C4Context", "C4Container", "C4Component", "C4Dynamic"] {
            let src = format!("{header}\n Person(u, \"U\")\n System(s, \"S\")\n Rel(u, s, \"x\")");
            assert!(
                to_svg(&src).unwrap().contains("<svg"),
                "{header} should render"
            );
        }
    }

    #[test]
    fn birel_and_rel_back_variants_render_arrows() {
        let svg = to_svg(
            "C4Context\n System(a, \"A\")\n System(b, \"B\")\n System(c, \"C\")\n \
             BiRel(a, b, \"peer\")\n Rel_Back(b, c, \"back\")\n Rel_U(a, c, \"up\")",
        )
        .unwrap();
        // Three relations: BiRel draws two arrowheads, the others one each → ≥4.
        assert!(svg.matches("<polygon").count() >= 4, "arrowheads: {svg}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("C4Context\n System(s, \"<b>x</b>\")\n Person(p, \"<i>y\")").unwrap();
        assert!(!svg.contains("<b>x") && !svg.contains("<i>y"), "{svg}");
        assert!(
            svg.contains("&lt;b&gt;x") && svg.contains("&lt;i&gt;y"),
            "{svg}"
        );
    }

    #[test]
    fn empty_c4_is_unsupported() {
        assert!(to_svg("C4Context").is_err());
        assert!(to_svg("C4Context\n title Only a title").is_err());
        assert!(
            to_svg("C4Context\n Rel(a, b, \"x\")").is_err(),
            "unresolved relation only"
        );
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "C4Context",
            "C4Context\n Person(",
            "C4Context\n Person()",
            "C4Context\n System(s, \"unterminated)",
            "C4Context\n System_Boundary(b) {\n System(s, \"x\")",
            "C4Context\n }",
            "C4Context\n } } }",
            "C4Context\n System(s, \"S\")\n Rel(s, s, \"self\")",
            "C4Context\n Rel(ghost, other, \"x\")",
            "C4Context\n (((",
            "C4Context\n Boundary(a, b) {\n Boundary(c, d) {\n System(e, f)\n }\n }",
            "C4Context\n UpdateElementStyle(s, $bgColor=\"red\")",
        ] {
            let _ = to_svg(s);
        }
    }
}
