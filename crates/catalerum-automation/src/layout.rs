//! Auto-placement of graph nodes authored **without** canvas positions (SOUL §11).
//!
//! The visual editor always saves explicit node positions, but an automation
//! authored over the API / by a tool-using agent (create_automation /
//! update_automation, REST, MCP) usually omits `position` — serde then defaults
//! every node to the origin and the canvas renders them stacked on top of each
//! other. This module computes readable positions for exactly those defaulted
//! nodes at **write time**, so the stored spec opens cleanly in the editor:
//!
//! - A graph whose nodes are **all** at the origin gets a full layered layout:
//!   left-to-right by longest-path depth from the entry nodes (matching the
//!   canvas' left-in / right-out port orientation), branches fanning downward,
//!   each layer ordered by the average row of its parents to keep edges short.
//! - A **partially** placed graph (an agent edited a hand-arranged automation and
//!   added nodes) keeps every positioned node where it is and slots each
//!   defaulted node next to its already-placed neighbours, bumping downward past
//!   any occupied box.
//! - A lone origin node that overlaps nothing is left alone — the origin is a
//!   legal (if unusual) deliberate spot; only *stacking* is repaired.
//!
//! Everything here is pure and total: no I/O, and a malformed/cyclic graph
//! (rejected by [`Graph::validate`] before persisting anyway) degrades to a
//! simple wrap-grid rather than failing.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use crate::graph::{Graph, Position};

/// Rendered node box size on the canvas, in canvas units (mirrors the web
/// editor's `NODE_W`/`NODE_H` in `catalerum-web` `flow.rs` — kept in sync by
/// hand; a drift only degrades spacing aesthetics, never correctness).
const NODE_W: f64 = 168.0;
const NODE_H: f64 = 64.0;

/// The canvas snap grid (the editor snaps dragged nodes to it).
const GRID: f64 = 24.0;

/// Column / row pitch of the generated layout: one node box plus a comfortable
/// gap, kept on the [`GRID`] so generated and hand-dragged nodes align.
const X_STEP: f64 = 264.0; // 168 + 96 gap
const Y_STEP: f64 = 120.0; // 64 + 56 gap

/// Top-left margin of a full generated layout.
const MARGIN: f64 = 48.0;

/// Snap a coordinate to the nearest [`GRID`] line (same rule as the editor).
fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// Whether two node boxes (top-left anchored, [`NODE_W`]×[`NODE_H`]) intersect.
fn overlaps(a: Position, b: Position) -> bool {
    (a.x - b.x).abs() < NODE_W && (a.y - b.y).abs() < NODE_H
}

/// Compute new positions for the graph's **defaulted** (origin-positioned)
/// nodes. Returns `(node_id, position)` pairs — empty when nothing needs to
/// move. Nodes with an explicit (non-origin) position are never repositioned.
#[must_use]
pub fn auto_layout_positions(graph: &Graph) -> Vec<(String, Position)> {
    if graph.nodes.len() < 2 {
        return Vec::new(); // a single node cannot stack on anything
    }
    let unplaced: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.position == Position::default())
        .map(|n| n.id.as_str())
        .collect();
    if unplaced.is_empty() {
        return Vec::new();
    }
    if unplaced.len() == graph.nodes.len() {
        return full_layout(graph);
    }
    // A lone defaulted node that overlaps no placed node may be a deliberate
    // origin placement — leave it; only stacking is a defect.
    if unplaced.len() == 1 {
        let lone = graph.node(unplaced[0]).expect("id from graph");
        let clear = graph
            .nodes
            .iter()
            .filter(|n| n.id != lone.id)
            .all(|n| !overlaps(lone.position, n.position));
        if clear {
            return Vec::new();
        }
    }
    incremental_layout(graph, &unplaced)
}

/// Longest-path depth (from the in-degree-0 entry nodes) per node id, walked in
/// topological order. `None` when the graph is cyclic (then the caller falls
/// back to a wrap-grid — unreachable after [`Graph::validate`], but total).
fn depths(graph: &Graph) -> Option<HashMap<String, usize>> {
    let order = graph.topo_order().ok()?;
    let mut depth: HashMap<String, usize> = HashMap::new();
    for id in &order {
        let d = graph
            .upstream(id)
            .iter()
            .filter_map(|p| depth.get(p))
            .map(|d| d + 1)
            .max()
            .unwrap_or(0);
        depth.insert(id.clone(), d);
    }
    Some(depth)
}

/// Layered layout of a fully defaulted graph: column = longest-path depth,
/// row = position within the layer, layers ordered by the mean row of their
/// parents (a one-pass barycenter, so branches stay near their source).
fn full_layout(graph: &Graph) -> Vec<(String, Position)> {
    let Some(depth) = depths(graph) else {
        // Cyclic (never persisted, but stay total): a simple wrap-grid.
        return graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let (col, row) = (i % 4, i / 4);
                let x = MARGIN + col as f64 * X_STEP;
                let y = MARGIN + row as f64 * Y_STEP;
                (n.id.clone(), Position { x, y })
            })
            .collect();
    };
    let max_depth = depth.values().copied().max().unwrap_or(0);
    // Layers in node order (stable for entry nodes and barycenter ties).
    let mut layers: Vec<Vec<&str>> = vec![Vec::new(); max_depth + 1];
    for n in &graph.nodes {
        layers[depth[&n.id]].push(n.id.as_str());
    }
    let mut placed: HashMap<String, Position> = HashMap::new();
    for (d, layer) in layers.iter().enumerate() {
        // Order the layer by the mean y of its (already placed, shallower)
        // parents so an edge runs to a nearby row; entry nodes keep node order.
        let mut keyed: Vec<(f64, &str)> = layer
            .iter()
            .map(|id| {
                let ys: Vec<f64> = graph
                    .upstream(id)
                    .iter()
                    .filter_map(|p| placed.get(p))
                    .map(|p| p.y)
                    .collect();
                let key = if ys.is_empty() {
                    f64::MAX // no placed parent: keep after connected nodes
                } else {
                    ys.iter().sum::<f64>() / ys.len() as f64
                };
                (key, *id)
            })
            .collect();
        keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (row, (_, id)) in keyed.into_iter().enumerate() {
            let x = MARGIN + d as f64 * X_STEP;
            let y = MARGIN + row as f64 * Y_STEP;
            placed.insert(id.to_string(), Position { x, y });
        }
    }
    // Emit in node order (deterministic output for tests and diffs).
    graph
        .nodes
        .iter()
        .filter_map(|n| placed.get(&n.id).map(|p| (n.id.clone(), *p)))
        .collect()
}

/// Slot each defaulted node into an already-arranged canvas: right of its
/// placed parents (or left of its placed children, or below everything when
/// disconnected), bumping downward one row at a time past occupied boxes.
fn incremental_layout(graph: &Graph, unplaced: &[&str]) -> Vec<(String, Position)> {
    let moving: HashSet<&str> = unplaced.iter().copied().collect();
    // Every node that keeps its position occupies its box.
    let mut positioned: HashMap<&str, Position> = graph
        .nodes
        .iter()
        .filter(|n| !moving.contains(n.id.as_str()))
        .map(|n| (n.id.as_str(), n.position))
        .collect();
    // Place in topological order so an unplaced parent lands before its
    // unplaced child (fall back to node order for a cyclic graph).
    let order = graph
        .topo_order()
        .unwrap_or_else(|_| graph.nodes.iter().map(|n| n.id.clone()).collect());
    let mut out = Vec::new();
    for id in order.iter().filter(|id| moving.contains(id.as_str())) {
        let parents: Vec<Position> = graph
            .upstream(id)
            .iter()
            .filter_map(|p| positioned.get(p.as_str()))
            .copied()
            .collect();
        let children: Vec<Position> = graph
            .out_edges(id, None)
            .filter_map(|e| positioned.get(e.to.as_str()))
            .copied()
            .collect();
        let mut pos = if let Some(rightmost) = parents.iter().map(|p| p.x).max_by(f64::total_cmp) {
            let mean_y = parents.iter().map(|p| p.y).sum::<f64>() / parents.len() as f64;
            Position {
                x: rightmost + X_STEP,
                y: mean_y,
            }
        } else if let Some(leftmost) = children.iter().map(|p| p.x).min_by(f64::total_cmp) {
            let mean_y = children.iter().map(|p| p.y).sum::<f64>() / children.len() as f64;
            Position {
                x: leftmost - X_STEP,
                y: mean_y,
            }
        } else {
            // Disconnected from anything placed: below the whole arrangement.
            let bottom = positioned
                .values()
                .map(|p| p.y)
                .max_by(f64::total_cmp)
                .unwrap_or(MARGIN - Y_STEP);
            Position {
                x: MARGIN,
                y: bottom + Y_STEP,
            }
        };
        pos = Position {
            x: snap(pos.x),
            y: snap(pos.y),
        };
        while positioned.values().any(|p| overlaps(pos, *p)) {
            pos.y += Y_STEP;
        }
        positioned.insert(id.as_str(), pos);
        out.push((id.clone(), pos));
    }
    out
}

/// Patch auto-layout positions into an automation's raw `spec` JSON, in place.
///
/// Parses `spec.graph` (a non-graph or malformed spec is left untouched — the
/// create/update paths validate separately), computes [`auto_layout_positions`],
/// and rewrites **only** the `position` key of the affected node objects — every
/// other byte of the spec round-trips verbatim (the spec is the client's
/// authoring document; re-serializing whole nodes could drop unknown fields).
pub fn apply_auto_layout(spec: &mut Value) {
    let Some(Ok(graph)) = Graph::from_spec(Some(spec)) else {
        return;
    };
    let placed = auto_layout_positions(&graph);
    if placed.is_empty() {
        return;
    }
    let by_id: HashMap<&str, Position> = placed.iter().map(|(id, p)| (id.as_str(), *p)).collect();
    let Some(nodes) = spec
        .get_mut("graph")
        .and_then(|g| g.get_mut("nodes"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for node in nodes {
        let Some(pos) = node
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| by_id.get(id))
        else {
            continue;
        };
        let pos = json!({ "x": pos.x, "y": pos.y });
        if let Some(obj) = node.as_object_mut() {
            obj.insert("position".to_string(), pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node, NodeKind};
    use serde_json::json;

    fn node(id: &str, x: f64, y: f64) -> Node {
        Node {
            id: id.into(),
            kind: NodeKind::Code {
                runtime: "js".into(),
                source: "1".into(),
            },
            position: Position { x, y },
        }
    }

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from: from.into(),
            to: to.into(),
            from_port: String::new(),
            to_port: String::new(),
        }
    }

    fn positions(graph: &Graph, placed: &[(String, Position)]) -> HashMap<String, Position> {
        let mut all: HashMap<String, Position> = graph
            .nodes
            .iter()
            .map(|n| (n.id.clone(), n.position))
            .collect();
        for (id, p) in placed {
            all.insert(id.clone(), *p);
        }
        all
    }

    fn no_stacking(all: &HashMap<String, Position>) {
        let v: Vec<(&String, &Position)> = all.iter().collect();
        for (i, (ida, a)) in v.iter().enumerate() {
            for (idb, b) in v.iter().skip(i + 1) {
                assert!(
                    !overlaps(**a, **b),
                    "nodes '{ida}' and '{idb}' overlap: {a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn a_fully_defaulted_chain_flows_left_to_right() {
        // t -> a -> b, all at the serde-default origin (the LLM-authored shape).
        let g = Graph {
            nodes: vec![
                node("t", 0.0, 0.0),
                node("a", 0.0, 0.0),
                node("b", 0.0, 0.0),
            ],
            edges: vec![edge("t", "a"), edge("a", "b")],
        };
        let placed = auto_layout_positions(&g);
        assert_eq!(placed.len(), 3, "every defaulted node is placed");
        let all = positions(&g, &placed);
        assert!(
            all["t"].x < all["a"].x && all["a"].x < all["b"].x,
            "depth = column"
        );
        assert_eq!(all["t"].y, all["a"].y, "a single chain stays on one row");
        no_stacking(&all);
    }

    #[test]
    fn branches_fan_vertically_in_the_same_column() {
        // t branches to a and b (both depth 1): same column, different rows.
        let g = Graph {
            nodes: vec![
                node("t", 0.0, 0.0),
                node("a", 0.0, 0.0),
                node("b", 0.0, 0.0),
            ],
            edges: vec![edge("t", "a"), edge("t", "b")],
        };
        let all = positions(&g, &auto_layout_positions(&g));
        assert_eq!(all["a"].x, all["b"].x, "same depth, same column");
        assert!((all["a"].y - all["b"].y).abs() >= Y_STEP, "rows separated");
        no_stacking(&all);
    }

    #[test]
    fn an_already_arranged_graph_is_untouched() {
        let g = Graph {
            nodes: vec![node("t", 48.0, 48.0), node("a", 312.0, 48.0)],
            edges: vec![edge("t", "a")],
        };
        assert!(auto_layout_positions(&g).is_empty());
    }

    #[test]
    fn a_lone_origin_node_that_overlaps_nothing_is_respected() {
        // One node deliberately dragged to (0,0) in an otherwise arranged graph.
        let g = Graph {
            nodes: vec![node("t", 0.0, 0.0), node("a", 312.0, 48.0)],
            edges: vec![edge("t", "a")],
        };
        assert!(auto_layout_positions(&g).is_empty());
    }

    #[test]
    fn a_new_defaulted_node_slots_beside_its_placed_parent() {
        // An agent edited a hand-arranged graph and appended `c` without a
        // position, stacking it on `t` at the origin: `c` moves right of its
        // parent `b`; `t` and `b` stay exactly where the user put them.
        let g = Graph {
            nodes: vec![
                node("t", 48.0, 48.0),
                node("b", 312.0, 48.0),
                node("c", 0.0, 0.0),
            ],
            edges: vec![edge("t", "b"), edge("b", "c")],
        };
        let placed = auto_layout_positions(&g);
        assert_eq!(placed.len(), 1, "only the defaulted node moves");
        assert_eq!(placed[0].0, "c");
        let all = positions(&g, &placed);
        assert_eq!(
            all["t"],
            Position { x: 48.0, y: 48.0 },
            "placed nodes keep their spot"
        );
        assert!(all["c"].x > all["b"].x, "child lands right of its parent");
        no_stacking(&all);
    }

    #[test]
    fn stacked_defaulted_siblings_bump_apart() {
        // Two new nodes, same parent, both defaulted: they must not stack.
        let g = Graph {
            nodes: vec![
                node("t", 48.0, 48.0),
                node("a", 0.0, 0.0),
                node("b", 0.0, 0.0),
            ],
            edges: vec![edge("t", "a"), edge("t", "b")],
        };
        let all = positions(&g, &auto_layout_positions(&g));
        no_stacking(&all);
    }

    #[test]
    fn a_disconnected_defaulted_node_lands_below_the_arrangement() {
        let g = Graph {
            nodes: vec![
                node("t", 48.0, 48.0),
                node("a", 312.0, 48.0),
                node("x", 0.0, 0.0),
                node("y", 0.0, 0.0),
            ],
            edges: vec![edge("t", "a")],
        };
        let placed = auto_layout_positions(&g);
        assert_eq!(placed.len(), 2);
        let all = positions(&g, &placed);
        assert!(all["x"].y > all["a"].y, "disconnected nodes go below");
        no_stacking(&all);
    }

    #[test]
    fn apply_auto_layout_patches_positions_and_keeps_unknown_fields() {
        let mut spec = json!({
            "note": "authoring context the layout must not eat",
            "graph": {
                "extra": true,
                "nodes": [
                    { "id": "t", "kind": "trigger",
                      "trigger": { "kind": "webhook", "path": "/h" },
                      "custom": "kept" },
                    { "id": "a", "kind": "action",
                      "action": { "kind": "create_note", "title": "x" } }
                ],
                "edges": [ { "from": "t", "to": "a" } ]
            }
        });
        apply_auto_layout(&mut spec);
        let nodes = spec["graph"]["nodes"].as_array().unwrap();
        let (t, a) = (&nodes[0], &nodes[1]);
        assert!(t["position"]["x"].as_f64().unwrap() < a["position"]["x"].as_f64().unwrap());
        // Everything but `position` round-trips verbatim.
        assert_eq!(
            spec["note"],
            json!("authoring context the layout must not eat")
        );
        assert_eq!(spec["graph"]["extra"], json!(true));
        assert_eq!(t["custom"], json!("kept"));
        assert_eq!(t["trigger"]["path"], json!("/h"));
    }

    #[test]
    fn apply_auto_layout_ignores_non_graph_and_malformed_specs() {
        let mut legacy = json!({ "note": "freeform" });
        apply_auto_layout(&mut legacy);
        assert_eq!(
            legacy,
            json!({ "note": "freeform" }),
            "non-graph spec untouched"
        );

        let mut malformed = json!({ "graph": { "nodes": [ { "id": "t", "kind": "nope" } ] } });
        let before = malformed.clone();
        apply_auto_layout(&mut malformed);
        assert_eq!(malformed, before, "unparseable graph untouched");
    }

    #[test]
    fn generated_positions_snap_to_the_editor_grid() {
        let g = Graph {
            nodes: vec![
                node("t", 50.0, 70.0),
                node("a", 0.0, 0.0),
                node("b", 0.0, 0.0),
            ],
            edges: vec![edge("t", "a"), edge("t", "b")],
        };
        for (_, p) in auto_layout_positions(&g) {
            assert_eq!(p.x, snap(p.x), "x on grid");
            assert_eq!(p.y, snap(p.y), "y on grid");
        }
    }
}
