//! Entity-relationship diagrams (`erDiagram`) → SVG, reusing the flowchart graph
//! layout + renderer ([`super::flow`]). Each entity becomes a box; a relationship
//! `A <card>--<card> B : verb` becomes a connecting line whose label carries the
//! verb and both cardinalities as text (e.g. `1 places 0..*`). A `--` line is
//! identifying (solid), `..` non-identifying (dashed). An attribute block
//! (`ENTITY { … }`) stacks its columns under the entity name inside the box
//! (`name: type [PK]`), using flow's `<br/>`-split multi-line label.

use std::collections::HashMap;

use super::flow::{self, Dir, Shape, Style};
use super::MermaidError;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let mut src_lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    src_lines.next(); // header (`erDiagram`)

    // First-seen entity order (drives layout), each entity's attribute rows, and
    // the relationships — collected in one pass, then built into the graph so a
    // relationship that mentions an entity can't clobber its declared columns.
    let mut order: Vec<String> = Vec::new();
    let mut attrs: HashMap<String, Vec<String>> = HashMap::new();
    let mut rels: Vec<Rel> = Vec::new();

    let mut block: Option<String> = None; // entity whose `{ … }` block we're inside
    for line in src_lines {
        if let Some(entity) = &block {
            if line == "}" {
                block = None;
            } else if let Some(col) = parse_attribute(line) {
                attrs.entry(entity.clone()).or_default().push(col);
            }
            continue; // attribute rows only feed the owning entity's box
        }
        if let Some(name) = line.strip_suffix('{') {
            let name = name.trim();
            if !name.is_empty() {
                note_entity(&mut order, name);
                attrs.entry(name.to_string()).or_default();
                block = Some(name.to_string());
            }
            continue;
        }
        if let Some(rel) = parse_relationship(line) {
            note_entity(&mut order, &rel.left);
            note_entity(&mut order, &rel.right);
            rels.push(rel);
        }
    }

    if order.is_empty() {
        return Err(MermaidError("no entities"));
    }

    let mut g = flow::Graph::new(Dir::Down);
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for name in &order {
        let i = g.node_def(name, entity_label(name, attrs.get(name)), Shape::Rect);
        idx.insert(name.as_str(), i);
    }
    for rel in &rels {
        // Both endpoints were noted into `order`, so the lookups always hit.
        let (from, to) = (idx[rel.left.as_str()], idx[rel.right.as_str()]);
        // No arrowhead — an ER relationship is an (undirected) connecting line.
        g.add_edge(from, to, rel.style, false, rel.label.clone());
    }

    Ok(flow::render_graph(&g))
}

/// Record an entity the first time it's seen, preserving source order for layout.
fn note_entity(order: &mut Vec<String>, name: &str) {
    if !order.iter().any(|e| e == name) {
        order.push(name.to_string());
    }
}

/// The entity name, then one `<br/>`-separated row per declared attribute. Flow's
/// renderer splits on `<br/>` and escapes each line, so the box grows into a
/// table-like stack with the name as the header line.
fn entity_label(name: &str, cols: Option<&Vec<String>>) -> String {
    match cols {
        Some(cols) if !cols.is_empty() => {
            let mut s = name.to_string();
            for c in cols {
                s.push_str("<br/>");
                s.push_str(c);
            }
            s
        }
        _ => name.to_string(),
    }
}

/// Parse an attribute row inside an `ENTITY { … }` block into a display line.
/// Mermaid rows are `type name [KEY…] ["comment"]`; render them as `name: type`
/// plus any key markers (`PK`/`FK`/`UK`). A contentless row yields `None`.
fn parse_attribute(line: &str) -> Option<String> {
    // Drop a trailing quoted comment (`… "the customer's name"`).
    let line = match line.split_once('"') {
        Some((head, _)) => head.trim(),
        None => line.trim(),
    };
    let mut toks = line.split_whitespace();
    let ty = toks.next()?;
    let Some(name) = toks.next() else {
        // A lone token isn't valid `type name`, but show it rather than drop it.
        return Some(ty.to_string());
    };
    let keys: Vec<&str> = toks.filter(|t| matches!(*t, "PK" | "FK" | "UK")).collect();
    let mut out = format!("{name}: {ty}");
    if !keys.is_empty() {
        out.push_str(&format!(" ({})", keys.join(", ")));
    }
    Some(out)
}

struct Rel {
    left: String,
    right: String,
    style: Style,
    label: String,
}

/// Parse `ENTITY1 <leftcard><line><rightcard> ENTITY2 : verb` into a relationship.
fn parse_relationship(line: &str) -> Option<Rel> {
    let (decl, verb) = match line.split_once(':') {
        Some((d, v)) => (d.trim(), v.trim()),
        None => (line.trim(), ""),
    };
    let parts: Vec<&str> = decl.split_whitespace().collect();
    if parts.len() != 3 {
        return None;
    }
    let (e1, op, e2) = (parts[0], parts[1], parts[2]);
    // The operator is `<leftcard><line><rightcard>` with `--` (identifying) or
    // `..` (non-identifying) as the line.
    let (lc, rc, style) = if let Some((l, r)) = op.split_once("--") {
        (l, r, Style::Solid)
    } else if let Some((l, r)) = op.split_once("..") {
        (l, r, Style::Dotted)
    } else {
        return None;
    };
    let verb = strip_quotes_simple(verb);
    let label = if verb.is_empty() {
        format!("{} \u{2014} {}", cardinality(lc), cardinality(rc))
    } else {
        format!("{} {verb} {}", cardinality(lc), cardinality(rc))
    };
    Some(Rel {
        left: e1.to_string(),
        right: e2.to_string(),
        style,
        label,
    })
}

/// Map a crow's-foot cardinality token to readable text: a `{`/`}` means *many*,
/// an `o` means *optional* (zero allowed). Orientation (`|o` vs `o|`) is ignored.
fn cardinality(tok: &str) -> &'static str {
    let many = tok.contains('{') || tok.contains('}');
    let optional = tok.contains('o');
    match (many, optional) {
        (true, true) => "0..*",
        (true, false) => "1..*",
        (false, true) => "0..1",
        (false, false) => "1",
    }
}

fn strip_quotes_simple(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn renders_entities_and_a_cardinality_labelled_relationship() {
        let svg = to_svg("erDiagram\n CUSTOMER ||--o{ ORDER : places").unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        assert!(
            svg.contains(">CUSTOMER</text>") && svg.contains(">ORDER</text>"),
            "{svg}"
        );
        // `||` ⇒ exactly one, `o{` ⇒ zero or many; the verb sits between them.
        assert!(svg.contains(">1 places 0..*</text>"), "{svg}");
    }

    #[test]
    fn one_to_many_and_dashed_non_identifying() {
        // `}|..|{` is one-or-many both ends, `..` ⇒ a dashed (non-identifying) line.
        let svg = to_svg("erDiagram\n A }|..|{ B : rel").unwrap();
        assert!(svg.contains(">1..* rel 1..*</text>"), "{svg}");
        assert!(
            svg.contains("stroke-dasharray"),
            "non-identifying ⇒ dashed: {svg}"
        );
    }

    #[test]
    fn attribute_block_columns_stack_under_the_entity_name() {
        let svg = to_svg(
            "erDiagram\n CUSTOMER {\n string name PK\n int age\n }\n CUSTOMER ||--o{ ORDER : places",
        )
        .unwrap();
        // The name heads the box (line 0), with the columns as `<tspan>` rows.
        assert!(svg.contains(">CUSTOMER<tspan"), "name heads the box: {svg}");
        assert!(
            svg.contains(">name: string (PK)</tspan>"),
            "PK column: {svg}"
        );
        assert!(svg.contains(">age: int</tspan>"), "plain column: {svg}");
        // A relationship mentioning CUSTOMER must not clobber its declared columns.
        assert!(svg.contains(">ORDER</text>"), "{svg}");
    }

    #[test]
    fn attribute_comment_is_dropped_and_html_in_columns_is_escaped() {
        let svg =
            to_svg("erDiagram\n T {\n string a<b \"a note here\"\n }\n T ||--|| U : x").unwrap();
        // The quoted comment is not rendered…
        assert!(!svg.contains("a note here"), "comment dropped: {svg}");
        // …and a `<` in a column name is escaped, never emitted raw.
        assert!(
            svg.contains(">a&lt;b: string</tspan>"),
            "escaped column: {svg}"
        );
        assert!(!svg.contains("a<b"), "raw `<` must not leak: {svg}");
    }

    #[test]
    fn empty_diagram_is_unsupported() {
        assert!(to_svg("erDiagram\n %% just a comment").is_err());
    }
}
