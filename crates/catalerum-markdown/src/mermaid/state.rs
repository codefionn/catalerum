//! State diagrams (`stateDiagram` / `stateDiagram-v2`) → SVG.
//!
//! A state diagram is a directed graph, so this reuses the flowchart's layered
//! layout + SVG renderer ([`super::flow`]): named states become rounded-rect
//! nodes, transitions become labelled edges, and each `[*]` becomes a small
//! `Point` pseudo-state — one shared start dot for `[*] -->`, one shared end dot
//! for `--> [*]`. Composite states, notes, choice/fork and concurrency (`--`) are
//! not yet rendered (their lines are skipped or flattened).

use std::collections::HashMap;

use super::flow::{self, Dir, Shape, Style};
use super::MermaidError;

pub(super) fn to_svg(src: &str) -> Result<String, MermaidError> {
    let mut lines = src
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("%%"));
    lines.next(); // header (`stateDiagram` / `stateDiagram-v2`)

    // Pass 1: collect display labels (`state "desc" as id`) and the transitions.
    // Labels are resolved before nodes are built so a transition that mentions a
    // state can't clobber its declared description.
    let mut labels: HashMap<String, String> = HashMap::new();
    let mut transitions: Vec<(String, String, String)> = Vec::new();
    for line in lines {
        if line == "}" || starts_with_any(line, &["note ", "direction ", "classDef", "class "]) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("state ") {
            if let Some((id, label)) = parse_state_decl(rest) {
                labels.insert(id, label);
            }
            continue;
        }
        if let Some((left, right)) = line.split_once("-->") {
            let from = left.trim().to_string();
            let (to, label) = match right.split_once(':') {
                Some((t, l)) => (t.trim().to_string(), l.trim().to_string()),
                None => (right.trim().to_string(), String::new()),
            };
            if !from.is_empty() && !to.is_empty() {
                transitions.push((from, to, label));
            }
        }
    }

    // Pass 2: build the graph and reuse the flowchart layout + renderer.
    let mut g = flow::Graph::new(Dir::Down);
    let mut star = StarSlots::default();
    for (from, to, label) in &transitions {
        let f = resolve(from, true, &labels, &mut g, &mut star);
        let t = resolve(to, false, &labels, &mut g, &mut star);
        g.add_edge(f, t, Style::Solid, true, label.clone());
    }

    if g.is_empty() {
        return Err(MermaidError("no states"));
    }
    Ok(flow::render_graph(&g))
}

/// The shared `[*]` pseudo-state dots: one start (source side), one end (target).
#[derive(Default)]
struct StarSlots {
    start: Option<usize>,
    end: Option<usize>,
    next_id: usize,
}

/// Resolve a state token to a flow node index, creating it on first use. `[*]`
/// maps to the shared start/end `Point`; a named state to a rounded-rect node.
fn resolve(
    tok: &str,
    is_source: bool,
    labels: &HashMap<String, String>,
    g: &mut flow::Graph,
    star: &mut StarSlots,
) -> usize {
    if tok == "[*]" {
        let slot = if is_source { star.start } else { star.end };
        if let Some(i) = slot {
            return i;
        }
        let id = format!("__star_{}", star.next_id);
        star.next_id += 1;
        let i = g.node_def(&id, String::new(), Shape::Point);
        if is_source {
            star.start = Some(i);
        } else {
            star.end = Some(i);
        }
        i
    } else {
        let label = labels.get(tok).cloned().unwrap_or_else(|| tok.to_string());
        g.node_def(tok, label, Shape::Round)
    }
}

/// Parse a `state "desc" as id` or `state id` / `state id {` declaration into
/// `(id, display_label)`; `None` if there's no usable id.
fn parse_state_decl(rest: &str) -> Option<(String, String)> {
    let rest = rest.trim();
    if let Some((quoted, after)) = rest.strip_prefix('"').and_then(|r| r.split_once('"')) {
        let id = after.trim().strip_prefix("as ").map(str::trim)?;
        return (!id.is_empty()).then(|| (id.to_string(), quoted.to_string()));
    }
    let id = rest.split([' ', '{']).next().unwrap_or("").trim();
    (!id.is_empty()).then(|| (id.to_string(), id.to_string()))
}

fn starts_with_any(line: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| line.starts_with(p))
}

#[cfg(test)]
mod tests {
    use crate::mermaid::to_svg;

    #[test]
    fn renders_states_transitions_and_pseudo_states() {
        let svg = to_svg(
            "stateDiagram-v2\n [*] --> Still\n Still --> Moving : start\n \
             Moving --> Still : stop\n Moving --> [*]",
        )
        .unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("</svg>"));
        // Named states render their labels; the transition labels too.
        assert!(
            svg.contains(">Still</text>") && svg.contains(">Moving</text>"),
            "{svg}"
        );
        assert!(
            svg.contains(">start</text>") && svg.contains(">stop</text>"),
            "{svg}"
        );
        // `[*]` → two pseudo-state dots (one start, one end) as filled circles.
        assert_eq!(svg.matches("<circle").count(), 2, "{svg}");
    }

    #[test]
    fn state_description_alias_is_used_as_the_label() {
        let svg = to_svg("stateDiagram-v2\n state \"In Progress\" as IP\n [*] --> IP").unwrap();
        assert!(svg.contains(">In Progress</text>"), "{svg}");
        // The bare id is not shown when a description is given.
        assert!(!svg.contains(">IP</text>"), "{svg}");
    }

    #[test]
    fn shared_start_dot_is_reused_across_transitions() {
        // Two transitions out of the start `[*]` share one start dot (+ one end dot).
        let svg = to_svg("stateDiagram-v2\n [*] --> A\n [*] --> B\n A --> [*]").unwrap();
        assert_eq!(
            svg.matches("<circle").count(),
            2,
            "one start + one end dot: {svg}"
        );
    }

    #[test]
    fn empty_or_contentless_diagram_is_unsupported() {
        // No transitions ⇒ nothing to render ⇒ raw-source fallback.
        assert!(to_svg("stateDiagram-v2\n note right of X: hi").is_err());
    }
}
