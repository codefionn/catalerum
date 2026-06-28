//! Mindmaps (`mindmap`) → SVG: an indentation-based hierarchy laid out as a tidy
//! left-to-right tree. The first content line is the root; every deeper-indented
//! line is a child of the nearest shallower line above it. A node may carry a
//! shape marker — `(rounded)`, `((circle))`, `[square]`, `{{hexagon}}`, `)cloud(`,
//! `(-ellipse-)` — with an optional leading id (`id((text))`); unknown markers
//! fall back to a rounded box. `::icon(...)`/`::class` metadata lines are ignored.
//! Each top-level subtree is tinted from the shared palette; the root is neutral.
//! All text is escaped.

use super::{MermaidError, PALETTE};
use crate::escape::escape_text;

const MARGIN: f64 = 16.0;
const CHAR_W: f64 = 7.2;
const NODE_H: f64 = 30.0;
const V_GAP: f64 = 12.0; // vertical gap between sibling rows
const H_GAP: f64 = 46.0; // horizontal gap between depth columns
const TEXT_PAD: f64 = 20.0;
const MIN_W: f64 = 42.0;
const MAX_W: f64 = 240.0;
const ROOT_COLOR: &str = "#475569";

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let nodes = parse(src);
    if nodes.is_empty() {
        return Err(MermaidError("empty mindmap"));
    }
    Ok(render(&nodes))
}

// ---- model ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Shape {
    Rounded, // default + `(rounded)`
    Circle,  // `((circle))`
    Square,  // `[square]`
    Hexagon, // `{{hexagon}}`
    Cloud,   // `)cloud(`
    Ellipse, // `(-ellipse-)`
}

struct Node {
    text: String,
    shape: Shape,
    indent: usize,
    depth: usize,
    children: Vec<usize>,
    color: &'static str,
    w: f64,
}

/// Leading-whitespace columns (tabs count as two) — the hierarchy key.
fn indent_of(line: &str) -> usize {
    let mut n = 0;
    for ch in line.chars() {
        match ch {
            ' ' => n += 1,
            '\t' => n += 2,
            _ => break,
        }
    }
    n
}

/// Split a node body into its shape and label, discarding any leading id. An
/// unrecognised or unbalanced marker falls back to a plain rounded label.
fn parse_node(s: &str) -> (Shape, String) {
    // The label lives inside the first bracket group; anything before it is the id.
    let open = s.find(['(', '[', '{', ')']);
    if let Some(i) = open {
        let rest = &s[i..];
        let inner_shape = |pre: &str, suf: &str, shape: Shape| -> Option<(Shape, String)> {
            let body = rest.strip_prefix(pre)?.strip_suffix(suf)?;
            Some((shape, body.trim().to_string()))
        };
        // Longest / most specific markers first.
        let hit = inner_shape("((", "))", Shape::Circle)
            .or_else(|| inner_shape("(-", "-)", Shape::Ellipse))
            .or_else(|| inner_shape("{{", "}}", Shape::Hexagon))
            .or_else(|| inner_shape(")", "(", Shape::Cloud))
            .or_else(|| inner_shape("(", ")", Shape::Rounded))
            .or_else(|| inner_shape("[", "]", Shape::Square));
        if let Some((shape, body)) = hit {
            if !body.is_empty() {
                return (shape, body);
            }
        }
    }
    // No marker (or a malformed one): the whole line is the label.
    (Shape::Rounded, s.to_string())
}

fn node_width(text: &str) -> f64 {
    (text.chars().count() as f64 * CHAR_W + TEXT_PAD).clamp(MIN_W, MAX_W)
}

fn parse(src: &str) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut seen_header = false;

    for raw in src.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with("%%") {
            continue;
        }
        if !seen_header {
            seen_header = true; // the `mindmap` keyword line
            continue;
        }
        // `::icon(...)` / `::class ...` decorate the previous node — skip them.
        if trimmed.starts_with("::") {
            continue;
        }
        let indent = indent_of(raw);
        let (shape, text) = parse_node(trimmed);
        if text.is_empty() {
            continue;
        }
        // Pop back to the nearest strictly-shallower node: that is the parent.
        while let Some(&top) = stack.last() {
            if nodes[top].indent >= indent {
                stack.pop();
            } else {
                break;
            }
        }
        // A dedent past the root still hangs off the root, keeping one tree.
        let parent = stack
            .last()
            .copied()
            .or(if nodes.is_empty() { None } else { Some(0) });
        let depth = parent.map_or(0, |p| nodes[p].depth + 1);
        let color = match depth {
            0 => ROOT_COLOR,
            1 => PALETTE[nodes[0].children.len() % PALETTE.len()],
            _ => nodes[parent.unwrap()].color,
        };
        let idx = nodes.len();
        nodes.push(Node {
            w: node_width(&text),
            text,
            shape,
            indent,
            depth,
            children: Vec::new(),
            color,
        });
        if let Some(p) = parent {
            nodes[p].children.push(idx);
        }
        stack.push(idx);
    }
    nodes
}

// ---- rendering -----------------------------------------------------------------

fn render(nodes: &[Node]) -> String {
    let n = nodes.len();
    let max_depth = nodes.iter().map(|nd| nd.depth).max().unwrap_or(0);

    // Column x per depth: accumulate the widest box at each level.
    let mut colw = vec![0.0f64; max_depth + 1];
    for nd in nodes {
        colw[nd.depth] = colw[nd.depth].max(nd.w);
    }
    let mut colx = vec![0.0f64; max_depth + 1];
    let mut x = MARGIN;
    for d in 0..=max_depth {
        colx[d] = x;
        x += colw[d] + H_GAP;
    }
    let width = (x - H_GAP + MARGIN).max(MIN_W + 2.0 * MARGIN);

    // Tidy vertical placement: iterative post-order so deep trees never recurse.
    // Leaves stack top-to-bottom; a parent centres on its first & last child.
    let row_h = NODE_H + V_GAP;
    let mut cy = vec![0.0f64; n];
    let mut next_y = MARGIN + NODE_H / 2.0;
    let mut work: Vec<(usize, bool)> = vec![(0, false)];
    while let Some((idx, expanded)) = work.pop() {
        if expanded {
            if nodes[idx].children.is_empty() {
                cy[idx] = next_y;
                next_y += row_h;
            } else {
                let first = nodes[idx].children[0];
                let last = *nodes[idx].children.last().unwrap();
                cy[idx] = (cy[first] + cy[last]) / 2.0;
            }
        } else {
            work.push((idx, true));
            for &c in nodes[idx].children.iter().rev() {
                work.push((c, false));
            }
        }
    }
    let max_cy = cy.iter().cloned().fold(0.0, f64::max);
    let height = max_cy + NODE_H / 2.0 + MARGIN;

    let mut out = String::with_capacity(512 + n * 160);
    out.push_str(&format!(
        "<svg class=\"catalerum-mermaid catalerum-mindmap\" xmlns=\"http://www.w3.org/2000/svg\" \
         viewBox=\"0 0 {width:.1} {height:.1}\" role=\"img\" font-family=\"system-ui,sans-serif\" \
         font-size=\"13\">"
    ));

    // Branch edges (parent right → child left), tinted by the child's subtree.
    for (i, nd) in nodes.iter().enumerate() {
        for &c in &nd.children {
            let px = colx[nd.depth] + nd.w;
            let py = cy[i];
            let qx = colx[nodes[c].depth];
            let qy = cy[c];
            let mid = (px + qx) / 2.0;
            out.push_str(&format!(
                "<path d=\"M{px:.1},{py:.1} C{mid:.1},{py:.1} {mid:.1},{qy:.1} {qx:.1},{qy:.1}\" \
                 fill=\"none\" stroke=\"{}\" stroke-width=\"1.6\"/>",
                nodes[c].color
            ));
        }
    }

    // Node shapes + labels on top of the edges.
    for (i, nd) in nodes.iter().enumerate() {
        let left = colx[nd.depth];
        let centre_x = left + nd.w / 2.0;
        let centre_y = cy[i];
        emit_shape(&mut out, nd, left, centre_x, centre_y);
        let weight = if nd.depth == 0 { "bold" } else { "normal" };
        out.push_str(&format!(
            "<text x=\"{centre_x:.1}\" y=\"{:.1}\" text-anchor=\"middle\" fill=\"#1e293b\" \
             font-weight=\"{weight}\">",
            centre_y + 4.5
        ));
        escape_text(&mut out, &nd.text);
        out.push_str("</text>");
    }

    out.push_str("</svg>");
    out
}

fn emit_shape(out: &mut String, nd: &Node, left: f64, cx: f64, cy: f64) {
    let w = nd.w;
    let h = NODE_H;
    let top = cy - h / 2.0;
    let color = nd.color;
    let style =
        format!("fill=\"{color}\" fill-opacity=\"0.15\" stroke=\"{color}\" stroke-width=\"1.5\"");
    match nd.shape {
        Shape::Rounded => out.push_str(&format!(
            "<rect x=\"{left:.1}\" y=\"{top:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"8\" {style}/>"
        )),
        Shape::Square => out.push_str(&format!(
            "<rect x=\"{left:.1}\" y=\"{top:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"2\" {style}/>"
        )),
        Shape::Cloud => out.push_str(&format!(
            "<rect x=\"{left:.1}\" y=\"{top:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"{:.1}\" {style}/>",
            h / 2.0
        )),
        Shape::Circle => out.push_str(&format!(
            "<ellipse cx=\"{cx:.1}\" cy=\"{cy:.1}\" rx=\"{:.1}\" ry=\"{:.1}\" {style}/>",
            w / 2.0,
            h / 2.0
        )),
        Shape::Ellipse => out.push_str(&format!(
            "<ellipse cx=\"{cx:.1}\" cy=\"{cy:.1}\" rx=\"{:.1}\" ry=\"{:.1}\" {style}/>",
            w / 2.0 + 6.0,
            h / 2.0
        )),
        Shape::Hexagon => {
            let notch = (h / 2.0).min(w / 2.0);
            let (x0, x1) = (left, left + w);
            let (yt, yb) = (top, top + h);
            out.push_str(&format!(
                "<polygon points=\"{:.1},{cy:.1} {:.1},{yt:.1} {:.1},{yt:.1} {x1:.1},{cy:.1} \
                 {:.1},{yb:.1} {:.1},{yb:.1}\" {style}/>",
                x0,
                x0 + notch,
                x1 - notch,
                x1 - notch,
                x0 + notch
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Shape};
    use crate::mermaid::to_svg;

    #[test]
    fn builds_a_hierarchy_from_indentation() {
        let nodes = parse(
            "mindmap\n  root((Root))\n    Origins\n      Long history\n      Popular\n    Tools\n      Mermaid",
        );
        // 6 nodes total: root, Origins, Long history, Popular, Tools, Mermaid.
        assert_eq!(nodes.len(), 6);
        // Root has two children (Origins, Tools).
        assert_eq!(nodes[0].children.len(), 2);
        // Origins (index 1) has two children (Long history, Popular).
        assert_eq!(nodes[1].children.len(), 2);
        // Depths climb by indentation.
        assert_eq!(nodes[0].depth, 0);
        assert_eq!(nodes[2].depth, 2, "Long history is a grandchild");
    }

    #[test]
    fn renders_shapes_edges_and_labels() {
        let svg = to_svg(
            "mindmap\n  root((Root))\n    A[Square]\n    B{{Hex}}\n    C)Cloud(\n    D(-Ellipse-)",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(svg.contains("catalerum-mindmap"), "class: {svg}");
        // Circle/ellipse root + ellipse leaf → at least two <ellipse>.
        assert!(
            svg.matches("<ellipse").count() >= 2,
            "circle/ellipse: {svg}"
        );
        assert!(svg.contains("<polygon"), "hexagon polygon: {svg}");
        assert!(svg.contains("<rect"), "square/cloud rect: {svg}");
        // One branch edge per non-root node (4 children → 4 paths).
        assert_eq!(svg.matches("<path").count(), 4, "one edge per child: {svg}");
        for label in ["Root", "Square", "Hex", "Cloud", "Ellipse"] {
            assert!(
                svg.contains(&format!(">{label}</text>")),
                "label {label}: {svg}"
            );
        }
    }

    #[test]
    fn shape_markers_parse_to_the_right_shape() {
        let cases = [
            ("id((c))", Shape::Circle),
            ("id(r)", Shape::Rounded),
            ("id[s]", Shape::Square),
            ("id{{h}}", Shape::Hexagon),
            (")cl(", Shape::Cloud),
            ("(-el-)", Shape::Ellipse),
            ("plain", Shape::Rounded),
        ];
        for (body, want) in cases {
            let src = format!("mindmap\n root\n  {body}");
            let nodes = parse(&src);
            let leaf = nodes.last().unwrap();
            assert!(leaf.shape == want, "{body} should parse to its shape");
        }
    }

    #[test]
    fn single_root_renders_one_node() {
        let svg = to_svg("mindmap\n  JustRoot").unwrap();
        assert!(svg.contains(">JustRoot</text>"), "{svg}");
        // No children → no edges.
        assert_eq!(svg.matches("<path").count(), 0, "{svg}");
    }

    #[test]
    fn deep_nesting_does_not_overflow_and_colours_by_subtree() {
        let mut src = String::from("mindmap\n root((R))\n");
        // One long spine plus a second top-level branch to check palette tinting.
        for d in 1..40 {
            src.push_str(&" ".repeat(d + 1));
            src.push_str(&format!("n{d}\n"));
        }
        src.push_str("  Second\n");
        let svg = to_svg(&src).unwrap();
        assert!(svg.contains(">n39</text>"), "deep leaf rendered: {svg}");
        // Two distinct top-level subtrees → two palette colours.
        assert!(
            svg.contains("#3b82f6") && svg.contains("#10b981"),
            "subtree tints: {svg}"
        );
    }

    #[test]
    fn unknown_marker_falls_back_to_default() {
        // A stray `<<weird>>` isn't a recognised marker → rounded box, label kept.
        let svg = to_svg("mindmap\n root\n  <<weird>>").unwrap();
        assert!(
            svg.contains("&lt;&lt;weird&gt;&gt;</text>"),
            "escaped label kept: {svg}"
        );
    }

    #[test]
    fn icon_lines_are_ignored() {
        let nodes = parse("mindmap\n root\n  Child\n  ::icon(fa fa-book)\n  Other");
        // root + Child + Other = 3 (the ::icon line is skipped).
        assert_eq!(nodes.len(), 3, "icon metadata skipped");
    }

    #[test]
    fn labels_are_escaped() {
        let svg = to_svg("mindmap\n root((<b>R</b>))\n  <script>x").unwrap();
        assert!(!svg.contains("<b>R") && !svg.contains("<script>x"), "{svg}");
        assert!(
            svg.contains("&lt;b&gt;R") && svg.contains("&lt;script&gt;x"),
            "{svg}"
        );
    }

    #[test]
    fn empty_mindmap_is_unsupported() {
        assert!(to_svg("mindmap").is_err());
        assert!(to_svg("mindmap\n  ::icon(x)").is_err());
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "mindmap",
            "mindmap\n",
            "mindmap\n root((",
            "mindmap\n  ))((",
            "mindmap\n root\n\t\tdeep\n  back",
            "mindmap\n  a\n b\nc\n   d",
            "mindmap\n root(((()))",
            "mindmap\n )(",
        ] {
            let _ = to_svg(s);
        }
    }
}
