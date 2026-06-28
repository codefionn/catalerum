//! Class diagrams (`classDiagram`) → SVG. Each class is a three-compartment box
//! (name/stereotype, attributes, methods); relationships connect them with the UML
//! arrowheads — inheritance/realization (hollow triangle), composition (filled
//! diamond), aggregation (hollow diamond), association/dependency (open arrow) —
//! with `--` solid vs `..` dashed lines and optional `"card"` multiplicities and
//! `: label` text. The shared layered layout ([`super::flow::layout_with_sizes`])
//! positions the boxes; this module draws the compartments and typed edges itself.

use std::collections::HashMap;

use super::{flow, MermaidError};
use crate::escape::escape_text;

const ROW: f64 = 18.0; // line height for a compartment text row
const PADX: f64 = 12.0; // horizontal text inset
const CHAR_W: f64 = 8.0; // approximate glyph width (sizing only)

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let model = parse(src);
    if model.classes.is_empty() {
        return Err(MermaidError("no classes"));
    }
    Ok(render(&model))
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Marker {
    None,
    Triangle,      // `<|` / `|>` — inheritance / realization
    DiamondFilled, // `*` — composition
    DiamondHollow, // `o` — aggregation
    Arrow,         // `<` / `>` — association / dependency
}

struct Class {
    name: String,    // id used for relationship lookup
    display: String, // shown name (generics `~T~` rendered as `<T>`)
    stereotype: Option<String>,
    attrs: Vec<String>,
    methods: Vec<String>,
}

struct Rel {
    from: usize,
    to: usize,
    start: Marker, // marker drawn at the `from` end
    end: Marker,   // marker drawn at the `to` end
    dashed: bool,
    label: String,
    from_card: String,
    to_card: String,
}

struct Model {
    classes: Vec<Class>,
    index: HashMap<String, usize>,
    rels: Vec<Rel>,
}

impl Model {
    /// Reference a class by name, creating an empty one if it's new.
    fn ensure(&mut self, name: &str) -> usize {
        let name = name.trim();
        if let Some(&i) = self.index.get(name) {
            return i;
        }
        let i = self.classes.len();
        self.classes.push(Class {
            name: name.to_string(),
            display: degeneric(name),
            stereotype: None,
            attrs: Vec::new(),
            methods: Vec::new(),
        });
        self.index.insert(name.to_string(), i);
        i
    }

    /// Add a member line to a class, routing it to the attributes or methods
    /// compartment (a `(` makes it a method, matching Mermaid's split).
    fn add_member(&mut self, ci: usize, raw: &str) {
        let raw = raw.trim();
        if raw.is_empty() {
            return;
        }
        if let Some(s) = stereotype_text(raw) {
            self.classes[ci].stereotype = Some(s);
            return;
        }
        let (is_method, text) = format_member(raw);
        if is_method {
            self.classes[ci].methods.push(text);
        } else {
            self.classes[ci].attrs.push(text);
        }
    }
}

// ---- parsing -------------------------------------------------------------------

fn parse(src: &str) -> Model {
    let mut model = Model {
        classes: Vec::new(),
        index: HashMap::new(),
        rels: Vec::new(),
    };
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`classDiagram` / `classDiagram-v2`)

    let mut block: Option<usize> = None; // class index whose `{` is open
    for line in lines {
        if let Some(ci) = block {
            if line == "}" || line.starts_with('}') {
                block = None;
            } else {
                model.add_member(ci, line);
            }
            continue;
        }
        // A `class X { … }` definition (the `{` may open a multi-line block, or the
        // whole body may sit inline on one line).
        if let Some(rest) = line.strip_prefix("class ") {
            let (name, body) = match rest.split_once('{') {
                Some((n, b)) => (n, Some(b)),
                None => (rest, None),
            };
            let ci = model.ensure(class_name(name));
            match body {
                None => {}
                Some(b) => match b.split_once('}') {
                    Some((inner, _)) => model.add_member(ci, inner.trim()), // inline body
                    None => block = Some(ci),                               // block opens
                },
            }
            continue;
        }
        // A stereotype attached by the `<<interface>> Name` form.
        if let Some(rest) = line.strip_prefix("<<") {
            if let Some((stereo, name)) = rest.split_once(">>") {
                let name = name.trim();
                if !name.is_empty() {
                    let ci = model.ensure(name);
                    model.classes[ci].stereotype = Some(degeneric(stereo.trim()));
                }
            }
            continue;
        }
        // Directives that don't define classes.
        let first = line.split_whitespace().next().unwrap_or("");
        if matches!(
            first,
            "direction"
                | "note"
                | "style"
                | "cssClass"
                | "click"
                | "link"
                | "callback"
                | "namespace"
                | "end"
        ) {
            continue;
        }
        // A relationship: its operator token contains `--` or `..`.
        if line
            .split_whitespace()
            .any(|t| t.contains("--") || t.contains(".."))
        {
            if let Some(rel) = parse_rel(line, &mut model) {
                model.rels.push(rel);
            }
            continue;
        }
        // The `Name : member` line form (also `Name : <<stereotype>>`).
        if let Some((name, member)) = line.split_once(':') {
            let name = class_name(name.trim());
            if !name.is_empty() {
                let ci = model.ensure(name);
                model.add_member(ci, member.trim());
            }
            continue;
        }
        // A bare class declaration (`class X` without a body is handled above; a
        // lone identifier here just declares it).
        let id = class_name(line.trim());
        if !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '_') {
            model.ensure(id);
        }
    }
    model
}

/// Strip a trailing `:::cssClass` styling suffix and surrounding whitespace from a
/// class name (generics are kept; [`degeneric`] renders them later).
fn class_name(s: &str) -> &str {
    s.split(":::").next().unwrap_or(s).trim()
}

/// If `s` is exactly a `<<…>>` stereotype, return its rendered inner text.
fn stereotype_text(s: &str) -> Option<String> {
    let inner = s.trim().strip_prefix("<<")?.strip_suffix(">>")?;
    Some(degeneric(inner.trim()))
}

/// Render Mermaid generics: `List~T~` → `List<T>`, `Map~K, V~` → `Map<K, V>` (each
/// `~` toggles between `<` and `>`).
fn degeneric(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut open = true;
    for ch in s.chars() {
        if ch == '~' {
            out.push(if open { '<' } else { '>' });
            open = !open;
        } else {
            out.push(ch);
        }
    }
    out
}

/// Split a member into `(is_method, rendered_text)`, keeping a leading visibility
/// marker (`+`/`-`/`#`/`~`) verbatim and rendering generics in the remainder.
fn format_member(raw: &str) -> (bool, String) {
    let raw = raw.trim();
    let mut chars = raw.chars();
    let (vis, rest) = match raw.chars().next() {
        Some(c @ ('+' | '-' | '#' | '~')) => {
            chars.next();
            (Some(c), chars.as_str())
        }
        _ => (None, raw),
    };
    let body = degeneric(rest.trim());
    let is_method = body.contains('(');
    let text = match vis {
        Some(v) => format!("{v}{body}"),
        None => body,
    };
    (is_method, text)
}

/// Parse `From ["card"] OP ["card"] To [: label]` into a relationship. The operator
/// token (the one carrying `--`/`..`) decides the line style and both end markers.
fn parse_rel(line: &str, model: &mut Model) -> Option<Rel> {
    let (decl, label) = match line.split_once(':') {
        Some((d, l)) => (d.trim(), l.trim()),
        None => (line.trim(), ""),
    };
    let tokens: Vec<&str> = decl.split_whitespace().collect();
    let op_idx = tokens
        .iter()
        .position(|t| t.contains("--") || t.contains(".."))?;
    if op_idx == 0 || op_idx + 1 >= tokens.len() {
        return None; // need a class on each side
    }
    let (start, dashed, end) = parse_operator(tokens[op_idx]);

    let from_name = tokens[0];
    let to_name = *tokens.last().unwrap();
    // A quoted token directly flanking the operator is a multiplicity.
    let from_card = if op_idx >= 2 && is_quoted(tokens[op_idx - 1]) {
        unquote(tokens[op_idx - 1])
    } else {
        String::new()
    };
    let to_card = if tokens.len() - op_idx >= 3 && is_quoted(tokens[op_idx + 1]) {
        unquote(tokens[op_idx + 1])
    } else {
        String::new()
    };

    let from = model.ensure(class_name(from_name));
    let to = model.ensure(class_name(to_name));
    Some(Rel {
        from,
        to,
        start,
        end,
        dashed,
        label: label.to_string(),
        from_card,
        to_card,
    })
}

/// Decode an operator like `<|--`, `*--`, `o..`, `-->`, `..|>` into
/// `(start_marker, dashed, end_marker)`. The `--`/`..` run splits the leading and
/// trailing marker glyphs; `.` ⇒ a dashed line.
fn parse_operator(op: &str) -> (Marker, bool, Marker) {
    let dashed = op.contains('.');
    let lead: String = op.chars().take_while(|c| *c != '-' && *c != '.').collect();
    let trail: String = op
        .chars()
        .rev()
        .take_while(|c| *c != '-' && *c != '.')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    (start_marker(&lead), dashed, end_marker(&trail))
}

fn start_marker(s: &str) -> Marker {
    match s {
        "<|" => Marker::Triangle,
        "*" => Marker::DiamondFilled,
        "o" => Marker::DiamondHollow,
        "<" => Marker::Arrow,
        _ => Marker::None,
    }
}

fn end_marker(s: &str) -> Marker {
    match s {
        "|>" => Marker::Triangle,
        "*" => Marker::DiamondFilled,
        "o" => Marker::DiamondHollow,
        ">" => Marker::Arrow,
        _ => Marker::None,
    }
}

fn is_quoted(s: &str) -> bool {
    s.starts_with('"') && s.ends_with('"') && s.len() >= 2
}

fn unquote(s: &str) -> String {
    s.trim_matches('"').to_string()
}

// ---- sizing + rendering --------------------------------------------------------

fn class_size(c: &Class) -> (f64, f64) {
    let mut maxch = c.display.chars().count();
    if let Some(s) = &c.stereotype {
        maxch = maxch.max(s.chars().count() + 4); // guillemets + spaces
    }
    for m in c.attrs.iter().chain(&c.methods) {
        maxch = maxch.max(m.chars().count());
    }
    let w = (maxch as f64 * CHAR_W + 2.0 * PADX).max(90.0);
    let stereo_rows = if c.stereotype.is_some() { 1.0 } else { 0.0 };
    let header_h = (stereo_rows + 1.0) * ROW + 12.0;
    let h = if has_members(c) {
        let attr_h = (c.attrs.len() as f64 * ROW).max(ROW) + 6.0;
        let meth_h = (c.methods.len() as f64 * ROW).max(ROW) + 6.0;
        header_h + attr_h + meth_h
    } else {
        header_h
    };
    (w, h)
}

fn has_members(c: &Class) -> bool {
    !c.attrs.is_empty() || !c.methods.is_empty()
}

fn render(model: &Model) -> String {
    // Build a flow graph (boxes + ranking edges) and lay it out with our sizes.
    let mut g = flow::Graph::new(flow::Dir::Down);
    for c in &model.classes {
        g.node_def(&c.name, c.name.clone(), flow::Shape::Rect);
    }
    for r in &model.rels {
        g.add_edge(r.from, r.to, flow::Style::Solid, false, String::new());
    }
    let sizes: Vec<(f64, f64)> = model.classes.iter().map(class_size).collect();
    let l = flow::layout_with_sizes(&g, &sizes);

    let mut out = String::with_capacity(640 + model.classes.len() * 220 + model.rels.len() * 140);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {:.1} {:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"14\">",
        l.width.max(1.0),
        l.height.max(1.0)
    ));
    out.push_str(MARKER_DEFS);

    for r in &model.rels {
        render_rel(r, &l, &mut out);
    }
    for (i, c) in model.classes.iter().enumerate() {
        render_class(&l.nodes[i], c, &mut out);
    }
    out.push_str("</svg>");
    out
}

/// Marker definitions reused by every edge. `auto-start-reverse` lets one def serve
/// either end (a `from`-end marker is drawn flipped).
const MARKER_DEFS: &str = "<defs>\
    <marker id=\"cl-arrow\" markerWidth=\"12\" markerHeight=\"12\" refX=\"10\" refY=\"5\" \
     orient=\"auto-start-reverse\" markerUnits=\"userSpaceOnUse\">\
     <path d=\"M0,0 L10,5 L0,10\" fill=\"none\" stroke=\"#64748b\" stroke-width=\"1.4\"/></marker>\
    <marker id=\"cl-tri\" markerWidth=\"15\" markerHeight=\"12\" refX=\"12\" refY=\"5\" \
     orient=\"auto-start-reverse\" markerUnits=\"userSpaceOnUse\">\
     <path d=\"M0,0 L12,5 L0,10 z\" fill=\"#ffffff\" stroke=\"#64748b\" stroke-width=\"1.2\"/></marker>\
    <marker id=\"cl-dia-f\" markerWidth=\"20\" markerHeight=\"12\" refX=\"16\" refY=\"5\" \
     orient=\"auto-start-reverse\" markerUnits=\"userSpaceOnUse\">\
     <path d=\"M0,5 L8,0 L16,5 L8,10 z\" fill=\"#64748b\" stroke=\"#64748b\" stroke-width=\"1.2\"/></marker>\
    <marker id=\"cl-dia-h\" markerWidth=\"20\" markerHeight=\"12\" refX=\"16\" refY=\"5\" \
     orient=\"auto-start-reverse\" markerUnits=\"userSpaceOnUse\">\
     <path d=\"M0,5 L8,0 L16,5 L8,10 z\" fill=\"#ffffff\" stroke=\"#64748b\" stroke-width=\"1.2\"/></marker>\
    </defs>";

fn marker_url(m: Marker) -> Option<&'static str> {
    match m {
        Marker::None => None,
        Marker::Triangle => Some("url(#cl-tri)"),
        Marker::DiamondFilled => Some("url(#cl-dia-f)"),
        Marker::DiamondHollow => Some("url(#cl-dia-h)"),
        Marker::Arrow => Some("url(#cl-arrow)"),
    }
}

fn render_rel(r: &Rel, l: &flow::Layout, out: &mut String) {
    let (a, b) = (&l.nodes[r.from], &l.nodes[r.to]);
    let (x1, y1) = flow::box_exit(a, b.cx, b.cy);
    let (x2, y2) = flow::box_exit(b, a.cx, a.cy);
    let dash = if r.dashed {
        " stroke-dasharray=\"5 4\""
    } else {
        ""
    };
    let mut markers = String::new();
    if let Some(u) = marker_url(r.start) {
        markers.push_str(&format!(" marker-start=\"{u}\""));
    }
    if let Some(u) = marker_url(r.end) {
        markers.push_str(&format!(" marker-end=\"{u}\""));
    }
    out.push_str(&format!(
        "<line x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\" \
         stroke=\"#64748b\" stroke-width=\"1.5\"{dash}{markers}/>"
    ));
    if !r.label.is_empty() {
        edge_label(out, (x1 + x2) / 2.0, (y1 + y2) / 2.0, &r.label);
    }
    // Multiplicities sit just inside each endpoint, offset along the line.
    if !r.from_card.is_empty() {
        card_label(out, x1, y1, x2, y2, &r.from_card);
    }
    if !r.to_card.is_empty() {
        card_label(out, x2, y2, x1, y1, &r.to_card);
    }
}

fn edge_label(out: &mut String, mx: f64, my: f64, text: &str) {
    let half = text.chars().count() as f64 * 7.0 / 2.0 + 4.0;
    out.push_str(&format!(
        "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"17\" fill=\"#ffffff\" opacity=\"0.85\"/>",
        mx - half,
        my - 8.5,
        half * 2.0
    ));
    out.push_str(&format!(
        "<text x=\"{mx:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#475569\" font-size=\"13\">",
        my + 4.0
    ));
    escape_text(out, text);
    out.push_str("</text>");
}

/// A multiplicity label near `(ex,ey)`, nudged ~20px toward `(ox,oy)` (the other
/// endpoint) so it clears the marker and box.
fn card_label(out: &mut String, ex: f64, ey: f64, ox: f64, oy: f64, text: &str) {
    let (dx, dy) = (ox - ex, oy - ey);
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let (px, py) = (ex + dx / len * 20.0, ey + dy / len * 20.0 - 4.0);
    out.push_str(&format!(
        "<text x=\"{px:.1}\" y=\"{py:.1}\" text-anchor=\"middle\" fill=\"#475569\" font-size=\"12\">"
    ));
    escape_text(out, text);
    out.push_str("</text>");
}

fn render_class(p: &flow::Placed, c: &Class, out: &mut String) {
    let (x, y) = (p.cx - p.w / 2.0, p.cy - p.h / 2.0);
    out.push_str(&format!(
        "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"4\" \
         fill=\"#eff6ff\" stroke=\"#3b82f6\" stroke-width=\"1.5\"/>",
        p.w, p.h
    ));

    // Header: optional «stereotype» then the (bold) class name, centred.
    let stereo_rows = if c.stereotype.is_some() { 1.0 } else { 0.0 };
    let header_h = (stereo_rows + 1.0) * ROW + 12.0;
    if let Some(s) = &c.stereotype {
        out.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#475569\" \
             font-size=\"12\" font-style=\"italic\">",
            p.cx,
            y + 6.0 + 11.0
        ));
        escape_text(out, &format!("\u{00ab}{s}\u{00bb}"));
        out.push_str("</text>");
    }
    out.push_str(&format!(
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#1e293b\" font-weight=\"bold\">",
        p.cx,
        y + stereo_rows * ROW + 6.0 + 14.0
    ));
    escape_text(out, &c.display);
    out.push_str("</text>");

    if !has_members(c) {
        return;
    }

    // Divider under the header, then the left-aligned attribute rows.
    let hdr_bottom = y + header_h;
    divider(out, x, hdr_bottom, p.w);
    let mut baseline = hdr_bottom + 6.0 + 12.0;
    for a in &c.attrs {
        member_row(out, x + PADX, baseline, a);
        baseline += ROW;
    }

    // Divider under the attributes, then the method rows.
    let attr_h = (c.attrs.len() as f64 * ROW).max(ROW) + 6.0;
    let attr_bottom = hdr_bottom + attr_h;
    divider(out, x, attr_bottom, p.w);
    baseline = attr_bottom + 6.0 + 12.0;
    for m in &c.methods {
        member_row(out, x + PADX, baseline, m);
        baseline += ROW;
    }
}

fn divider(out: &mut String, x: f64, y: f64, w: f64) {
    out.push_str(&format!(
        "<line x1=\"{x:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\" stroke=\"#3b82f6\" stroke-width=\"1\"/>",
        x + w
    ));
}

fn member_row(out: &mut String, x: f64, baseline: f64, text: &str) {
    out.push_str(&format!(
        "<text x=\"{x:.1}\" y=\"{baseline:.1}\" fill=\"#1e293b\" font-size=\"13\">"
    ));
    escape_text(out, text);
    out.push_str("</text>");
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn class_with_members_splits_attributes_and_methods() {
        let svg = to_svg(
            "classDiagram\n class Animal {\n +String name\n +int age\n +makeSound() void\n }",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains(">Animal</text>"), "{svg}");
        // Attributes (no parens) and the method (parens) all render as rows.
        assert!(svg.contains(">+String name</text>"), "{svg}");
        assert!(svg.contains(">+int age</text>"), "{svg}");
        assert!(svg.contains(">+makeSound() void</text>"), "{svg}");
        // Two compartment dividers (header→attrs, attrs→methods).
        assert!(
            svg.matches("stroke=\"#3b82f6\" stroke-width=\"1\"").count() >= 2,
            "{svg}"
        );
    }

    #[test]
    fn inheritance_draws_a_hollow_triangle() {
        // `Animal <|-- Dog`: triangle at the parent (`<|`, the from end).
        let svg = to_svg("classDiagram\n Animal <|-- Dog").unwrap();
        assert!(svg.contains("marker-start=\"url(#cl-tri)\""), "{svg}");
        assert!(
            svg.contains(">Animal</text>") && svg.contains(">Dog</text>"),
            "{svg}"
        );
        // Solid line — no dash.
        assert!(!svg.contains("stroke-dasharray"), "{svg}");
    }

    #[test]
    fn composition_aggregation_and_dependency_markers() {
        // Filled diamond, hollow diamond, and a dashed dependency arrow.
        let svg =
            to_svg("classDiagram\n Car *-- Engine\n Lake o-- Duck\n Client ..> Service").unwrap();
        assert!(svg.contains("url(#cl-dia-f)"), "composition diamond: {svg}");
        assert!(svg.contains("url(#cl-dia-h)"), "aggregation diamond: {svg}");
        assert!(
            svg.contains("marker-end=\"url(#cl-arrow)\""),
            "dependency arrow: {svg}"
        );
        assert!(
            svg.contains("stroke-dasharray"),
            "dependency is dashed: {svg}"
        );
    }

    #[test]
    fn stereotype_and_line_member_form() {
        // `<<interface>>` renders in guillemets; the `Name : member` form works.
        let svg =
            to_svg("classDiagram\n class Shape\n <<interface>> Shape\n Shape : +area() float")
                .unwrap();
        assert!(svg.contains("\u{00ab}interface\u{00bb}"), "{svg}");
        assert!(svg.contains(">+area() float</text>"), "{svg}");
    }

    #[test]
    fn cardinalities_and_label_render() {
        let svg = to_svg("classDiagram\n Order \"1\" --> \"*\" Item : contains").unwrap();
        assert!(svg.contains(">1</text>"), "from card: {svg}");
        assert!(svg.contains(">*</text>"), "to card: {svg}");
        assert!(svg.contains(">contains</text>"), "label: {svg}");
        assert!(svg.contains("marker-end=\"url(#cl-arrow)\""), "{svg}");
    }

    #[test]
    fn generics_render_as_angle_brackets() {
        let svg = to_svg("classDiagram\n class Box~T~ {\n +T value\n }").unwrap();
        assert!(svg.contains(">Box&lt;T&gt;</text>"), "{svg}");
        assert!(svg.contains(">+T value</text>"), "{svg}");
        assert!(!svg.contains('~'), "tilde should be consumed: {svg}");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("classDiagram\n class A {\n +x<script>y\n }").unwrap();
        assert!(!svg.contains("<script>"), "{svg}");
        assert!(svg.contains("&lt;script&gt;"), "{svg}");
    }

    #[test]
    fn empty_diagram_is_unsupported() {
        assert!(to_svg("classDiagram\n %% nothing").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "classDiagram",
            "classDiagram\n class",
            "classDiagram\n class A {",
            "classDiagram\n A <|--",
            "classDiagram\n <|-- B",
            "classDiagram\n A : ",
            "classDiagram\n <<>> ",
            "classDiagram\n class ~~ {\n }",
            "classDiagram\n A \"1\" --> B : ",
        ] {
            let _ = to_svg(s);
        }
    }
}
