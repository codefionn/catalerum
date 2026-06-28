//! The visual node-graph flow editor (SOUL §11 Phase C — the no-code canvas).
//!
//! A drag-and-drop **SVG** editor (not web-sys canvas) for authoring a
//! `catalerum-automation` graph: a palette adds typed nodes (trigger / action /
//! code / condition), nodes are dragged around the canvas, output→input ports are
//! wired into edges, the canvas pans, and a side panel edits the selected node.
//! The whole graph round-trips through the automation's free-form `spec.graph`
//! JSON (the exact backend [`catalerum_automation::graph::Graph`] shape), so a
//! visually-authored automation runs through the same DAG executor as a
//! hand-written one, and an existing graph automation loads back into the canvas.
//!
//! The **pure core** is everything the engine cares about — the serde
//! [`FlowGraph`]/[`FlowNode`]/[`FlowEdge`] types (which serialize to the backend
//! node shape: `{id, kind, <payload>, position}`), [`graph_to_spec_value`] /
//! [`flow_from_spec`] (the `spec.graph` round-trip), [`validate_flow`] (mirroring
//! the backend's cheap checks: unique ids, live edge endpoints, ≥1 trigger,
//! acyclic), and the geometry/CRUD helpers ([`edge_path`], [`screen_to_canvas`],
//! node/edge add+remove, a deterministic [`fresh_id`]). All of it is total,
//! I/O-free, and unit-tested below; the [`FlowEditor`] component is a thin SVG
//! shell over it, holding only ephemeral interaction state (drag / pan /
//! pending-connection / selection) locally so the parent owns the saved graph.

use leptos::ev::{PointerEvent, WheelEvent};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashSet;
use wasm_bindgen::JsCast;

use crate::api::{
    CalendarProviderKind, Connection, CreateConnection, CreateEmailConnection, EmailProviderKind,
    ModelInfo, NodeTypeHit,
};
use crate::components::automations::{
    every_display, every_shape_ok, every_value, is_string_list_field, split_list,
    trigger_field_display, trigger_fields,
};
use crate::components::icons::{Icon, MdIcon};
use crate::components::widgets::{model_autocomplete, model_options};
use crate::{auth, rest};

// ---------------------------------------------------------------------------
// Web graph types — serialize to the EXACT backend `Graph` shape.
// ---------------------------------------------------------------------------

/// A node's canvas coordinates — round-tripped for the editor; the engine ignores
/// it. Mirrors `catalerum_automation::graph::Position`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowPos {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// The typed, `kind`-tagged payload of a [`FlowNode`], mirroring
/// `catalerum_automation::graph::NodeKind`. The `kind` tag is flattened onto the
/// node object (`{id, kind:"trigger", trigger:{..}, position:{..}}`), so this
/// serializes to the exact backend node shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowKind {
    /// A graph entry point: a §11 trigger spec (`{kind, ..}`) kept as JSON so the
    /// canvas doesn't have to model every trigger variant.
    Trigger { trigger: Value },
    /// A §11 action spec (`{kind, ..params}`) kept as JSON.
    Action { action: Value },
    /// An inline pure data-transform: `runtime` (`js`/`shell`/`python`) + `source`.
    Code { runtime: String, source: String },
    /// A branch gate: `runtime` + `source`; its truthy/falsy result routes the
    /// `"true"` / `"false"` out-edge ports.
    Condition { runtime: String, source: String },
    /// A loop-region head (SOUL §11): evaluates `source` (a path into the input
    /// envelope) to an array and runs the nodes between it and its paired
    /// [`FlowKind::LoopEnd`] once per element, bound to `item` (+ optional
    /// 0-based `index`). Mirrors the backend `NodeKind::ForEach` field-for-field
    /// so a loop graph round-trips through the canvas losslessly.
    ForEach {
        source: String,
        item: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<usize>,
    },
    /// A loop-region tail: pairs with its [`FlowKind::ForEach`] head by node id.
    /// Downstream nodes read the per-iteration results array as this node's output.
    LoopEnd { for_each: String },
}

impl FlowKind {
    /// The stable `kind` discriminant (matching the stored JSON tag).
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            FlowKind::Trigger { .. } => "trigger",
            FlowKind::Action { .. } => "action",
            FlowKind::Code { .. } => "code",
            FlowKind::Condition { .. } => "condition",
            FlowKind::ForEach { .. } => "for_each",
            FlowKind::LoopEnd { .. } => "loop_end",
        }
    }

    /// The out-edge port names this kind exposes: a condition branches on
    /// `"true"` / `"false"`; a **collect** source trigger exposes a second `commit`
    /// gate port (wire it to the write node whose success advances the cursor — the
    /// `commit_on` reference, SOUL §11/§28 — represented visually here, never as a
    /// real DAG edge); every other kind has a single default `""` port.
    #[must_use]
    pub fn out_ports(&self) -> &'static [&'static str] {
        match self {
            FlowKind::Condition { .. } => &["true", "false"],
            FlowKind::Trigger { trigger } if trigger_is_collect(trigger) => &["", "commit"],
            _ => &[""],
        }
    }
}

/// The editor `from_port` name carrying a collect trigger's `commit_on` reference.
/// Edges on this port are translated to/from the trigger's `commit_on` field at the
/// spec boundary (see [`graph_to_spec_value`] / [`flow_from_spec`]); they are gates,
/// not execution edges, so the data-flow helpers (cycle / reachability) skip them.
pub const COMMIT_PORT: &str = "commit";

/// Whether a trigger spec Value is a collect source (`collect_email` /
/// `collect_calendar` / `collect_sql`) — the kinds that carry a `commit_on` and
/// get a commit port.
fn trigger_is_collect(trigger: &Value) -> bool {
    matches!(
        trigger.get("kind").and_then(Value::as_str),
        Some("collect_email") | Some("collect_calendar") | Some("collect_sql")
    )
}

/// One node on the canvas: a stable `id`, its typed [`FlowKind`] payload (flattened
/// so `kind` is the JSON tag), and its editor `position`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowNode {
    pub id: String,
    #[serde(flatten)]
    pub kind: FlowKind,
    #[serde(default)]
    pub position: FlowPos,
}

impl FlowNode {
    /// Whether this node is a graph entry point (a [`FlowKind::Trigger`]).
    #[must_use]
    pub fn is_trigger(&self) -> bool {
        matches!(self.kind, FlowKind::Trigger { .. })
    }
}

/// A directed edge from one node's output port to another node's input port. Ports
/// default to `""` (a node's single default port); a condition's `from_port` is
/// `"true"` / `"false"`. Mirrors `catalerum_automation::graph::Edge`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FlowEdge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub from_port: String,
    #[serde(default)]
    pub to_port: String,
}

/// The whole canvas graph: typed [`FlowNode`]s connected by [`FlowEdge`]s. Serializes
/// to the backend `Graph { nodes, edges }` shape, so it persists straight into
/// `spec.graph`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FlowGraph {
    #[serde(default)]
    pub nodes: Vec<FlowNode>,
    #[serde(default)]
    pub edges: Vec<FlowEdge>,
}

// ---------------------------------------------------------------------------
// Pure helpers — the testable core.
// ---------------------------------------------------------------------------

/// Wrap a [`FlowGraph`] into the automation `spec` JSON the backend expects:
/// `{ "graph": { nodes, edges } }`. The exact value sent as `CreateAutomation.spec`
/// / `UpdateAutomation.spec`; the round-trip partner of [`flow_from_spec`].
#[must_use]
pub fn graph_to_spec_value(graph: &FlowGraph) -> Value {
    // Commit edges are an editor-only representation of each collect trigger's
    // `commit_on` node reference — fold them into the trigger field and drop the
    // edges, so the backend `Graph` carries `commit_on` as data (never a write→
    // trigger DAG edge, which would cycle).
    let compiled = compile_commit_edges(graph);
    let mut obj = Map::new();
    obj.insert(
        "graph".to_string(),
        serde_json::to_value(&compiled).unwrap_or(Value::Null),
    );
    Value::Object(obj)
}

/// Fold `commit`-port edges into their source trigger's `commit_on` field and strip
/// them from the edge list (the inverse of [`lift_commit_edges`]).
fn compile_commit_edges(graph: &FlowGraph) -> FlowGraph {
    use std::collections::HashMap;
    let targets: HashMap<&str, &str> = graph
        .edges
        .iter()
        .filter(|e| e.from_port == COMMIT_PORT)
        .map(|e| (e.from.as_str(), e.to.as_str()))
        .collect();
    let nodes = graph
        .nodes
        .iter()
        .map(|n| {
            let mut n = n.clone();
            // Source of truth is the edge: set `commit_on` when wired, clear otherwise.
            let target = targets.get(n.id.as_str()).map(|t| (*t).to_string());
            if let FlowKind::Trigger {
                trigger: Value::Object(map),
            } = &mut n.kind
            {
                match target {
                    Some(t) => {
                        map.insert("commit_on".to_string(), Value::String(t));
                    }
                    None => {
                        map.remove("commit_on");
                    }
                }
            }
            n
        })
        .collect();
    let edges = graph
        .edges
        .iter()
        .filter(|e| e.from_port != COMMIT_PORT)
        .cloned()
        .collect();
    FlowGraph { nodes, edges }
}

/// Lift each collect trigger's `commit_on` field into a visual `commit`-port edge
/// (and strip the field from the in-editor trigger JSON), so the canvas shows the
/// commit relationship as a connection. The inverse of [`compile_commit_edges`].
fn lift_commit_edges(mut graph: FlowGraph) -> FlowGraph {
    let mut commit_edges = Vec::new();
    for n in &mut graph.nodes {
        if let FlowKind::Trigger {
            trigger: Value::Object(map),
        } = &mut n.kind
        {
            if let Some(target) = map.remove("commit_on").and_then(|v| match v {
                Value::String(s) if !s.is_empty() => Some(s),
                _ => None,
            }) {
                commit_edges.push(FlowEdge {
                    from: n.id.clone(),
                    to: target,
                    from_port: COMMIT_PORT.to_string(),
                    to_port: String::new(),
                });
            }
        }
    }
    graph.edges.extend(commit_edges);
    graph
}

/// Parse a [`FlowGraph`] back out of an automation's `spec` JSON: `Some(graph)` when
/// `spec` is an object carrying a `"graph"` key that parses; `None` for a missing /
/// legacy / malformed spec (the editor then starts empty, leaving the raw-JSON
/// editor as the escape hatch). Tolerant by design — never panics, never errors.
#[must_use]
pub fn flow_from_spec(spec: &Value) -> Option<FlowGraph> {
    let graph = spec.as_object()?.get("graph")?;
    let parsed = serde_json::from_value::<FlowGraph>(graph.clone()).ok()?;
    // A collect trigger's stored `commit_on` becomes a visual `commit`-port edge.
    Some(lift_commit_edges(parsed))
}

/// Authoring-time validation, mirroring the backend's cheap checks
/// (`Graph::validate`): node ids are unique, every edge endpoint references an
/// existing node, the graph has ≥1 trigger node, and it is acyclic. The same
/// gate the canvas runs before offering to save.
///
/// # Errors
/// A human-readable message for the first violation (duplicate id, dangling
/// endpoint, no trigger, or a cycle).
pub fn validate_flow(graph: &FlowGraph) -> Result<(), String> {
    let mut ids = HashSet::new();
    for n in &graph.nodes {
        if !ids.insert(n.id.as_str()) {
            return Err(format!("duplicate node id '{}'", n.id));
        }
    }
    for e in &graph.edges {
        if !ids.contains(e.from.as_str()) {
            return Err(format!("edge from unknown node '{}'", e.from));
        }
        if !ids.contains(e.to.as_str()) {
            return Err(format!("edge to unknown node '{}'", e.to));
        }
    }
    if !graph.nodes.iter().any(FlowNode::is_trigger) {
        return Err("graph has no trigger node".to_string());
    }
    if has_cycle(graph) {
        return Err("graph has a cycle".to_string());
    }
    // Loop pairing sanity (SOUL §11) — the cheap client-side half of the backend's
    // region validation: every End-loop must pick an existing For-each head, and
    // every For-each must be closed by exactly one End-loop. (Body shape — non-empty,
    // isolated, non-nested — stays the server's richer check on save.)
    for n in &graph.nodes {
        if let FlowKind::LoopEnd { for_each } = &n.kind {
            let head_exists = graph
                .nodes
                .iter()
                .any(|h| h.id == *for_each && matches!(h.kind, FlowKind::ForEach { .. }));
            if !head_exists {
                return Err(format!(
                    "end-loop '{}' must pick the For-each head it closes",
                    n.id
                ));
            }
        }
    }
    for n in &graph.nodes {
        if matches!(n.kind, FlowKind::ForEach { .. }) {
            let ends = graph
                .nodes
                .iter()
                .filter(|e| matches!(&e.kind, FlowKind::LoopEnd { for_each } if *for_each == n.id))
                .count();
            if ends != 1 {
                return Err(format!(
                    "for-each '{}' must be closed by exactly one End-loop (found {ends})",
                    n.id
                ));
            }
        }
    }
    Ok(())
}

/// Whether the graph contains a directed cycle (Kahn's algorithm: a graph whose
/// remaining count after peeling in-degree-0 nodes is non-zero has a cycle). Only
/// counts edges whose endpoints both exist, so a dangling edge can't underflow.
fn has_cycle(graph: &FlowGraph) -> bool {
    use std::collections::HashMap;
    let mut indeg: HashMap<&str, usize> = graph.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in &graph.edges {
        // Commit-port edges are cursor-commit gates, not execution edges (and point
        // trigger→write), so they don't participate in the execution DAG's acyclicity.
        if e.from_port == COMMIT_PORT {
            continue;
        }
        if indeg.contains_key(e.from.as_str()) && indeg.contains_key(e.to.as_str()) {
            adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
            *indeg.get_mut(e.to.as_str()).unwrap() += 1;
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut visited = 0usize;
    while let Some(id) = queue.pop() {
        visited += 1;
        if let Some(tos) = adj.get(id) {
            for &to in tos {
                let d = indeg.get_mut(to).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push(to);
                }
            }
        }
    }
    visited != graph.nodes.len()
}

/// Node ids **not reachable from any trigger** node — they'll never run (a common
/// "I wired it wrong" mistake the canvas flags with a ⚠ marker). BFS from every
/// trigger node along edge direction; whatever isn't reached is unreachable. Pure +
/// testable. (A graph with no triggers reports every node, but `validate_flow`
/// already rejects that separately.)
fn unreachable_nodes(graph: &FlowGraph) -> HashSet<String> {
    let mut reached: HashSet<String> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, FlowKind::Trigger { .. }))
        .map(|n| n.id.clone())
        .collect();
    let mut frontier: Vec<String> = reached.iter().cloned().collect();
    while let Some(id) = frontier.pop() {
        for e in &graph.edges {
            // A commit gate doesn't make its target "reachable for execution".
            if e.from_port == COMMIT_PORT {
                continue;
            }
            if e.from == id && !reached.contains(&e.to) {
                reached.insert(e.to.clone());
                frontier.push(e.to.clone());
            }
        }
    }
    graph
        .nodes
        .iter()
        .map(|n| n.id.clone())
        .filter(|id| !reached.contains(id))
        .collect()
}

/// Nodes the canvas flags with a ⚠: non-trigger nodes unreachable from any trigger
/// (they never run) **plus** trigger nodes with no outgoing edge (they fire but run
/// nothing — a dead end). Pure + testable.
fn problem_nodes(graph: &FlowGraph) -> HashSet<String> {
    let mut problems = unreachable_nodes(graph);
    for n in &graph.nodes {
        // A trigger with no *execution* out-edge is a dead end — a lone `commit`
        // gate doesn't count (it advances a cursor, it doesn't run anything).
        if matches!(n.kind, FlowKind::Trigger { .. })
            && !graph
                .edges
                .iter()
                .any(|e| e.from == n.id && e.from_port != COMMIT_PORT)
        {
            problems.insert(n.id.clone());
        }
    }
    problems
}

/// An SVG cubic-bezier `d` attribute connecting two port points, bowing the control
/// points **horizontally** (the standard left-to-right node-graph look): the
/// handles sit halfway between the endpoints in x, level with their own endpoint in
/// y. Pure geometry, so the path shape is unit-testable.
#[must_use]
pub fn edge_path(from: (f64, f64), to: (f64, f64)) -> String {
    let (x1, y1) = from;
    let (x2, y2) = to;
    // Control-handle reach: half the horizontal span, clamped to a sane minimum so
    // near-vertical or backward edges still curve instead of kinking.
    let dx = ((x2 - x1).abs() * 0.5).max(40.0);
    let c1x = x1 + dx;
    let c2x = x2 - dx;
    format!("M {x1:.1} {y1:.1} C {c1x:.1} {y1:.1} {c2x:.1} {y2:.1} {x2:.1} {y2:.1}")
}

/// The commit gate's edge path. Two shape rules distinguish it from [`edge_path`]:
/// it leaves the trigger's commit port angling *down*-right (a flat horizontal exit
/// would run straight through the port's own "commit" label), and it arrives
/// **vertically from below** — the gate lands on the target's bottom edge
/// ([`commit_anchor_point`]), so a horizontal arrival would lay the arrowhead flat
/// along the node's bottom border instead of pointing up into it. Pure geometry.
#[must_use]
pub fn commit_edge_path(from: (f64, f64), to: (f64, f64)) -> String {
    let (x1, y1) = from;
    let (x2, y2) = to;
    let dx = ((x2 - x1).abs() * 0.5).max(40.0);
    // Arrival-handle depth below the anchor: enough that the approach reads as
    // straight-up even when the two nodes sit level.
    let dy = ((y2 - y1).abs() * 0.5).max(36.0);
    let c1x = x1 + dx;
    let c1y = y1 + 26.0;
    let c2y = y2 + dy;
    format!("M {x1:.1} {y1:.1} C {c1x:.1} {c1y:.1} {x2:.1} {c2y:.1} {x2:.1} {y2:.1}")
}

/// Convert a pointer's screen (client) coordinates into canvas-space coordinates:
/// subtract the SVG root's on-screen origin (its `getBoundingClientRect` top-left)
/// and the active `pan` translation, then divide by the `scale` (zoom). The inverse
/// of the root `<g>`'s `translate(pan) scale(zoom)` transform. Pure, so the
/// transform is unit-testable.
#[must_use]
pub fn screen_to_canvas(
    client: (f64, f64),
    rect_origin: (f64, f64),
    pan: (f64, f64),
    scale: f64,
) -> (f64, f64) {
    (
        (client.0 - rect_origin.0 - pan.0) / scale,
        (client.1 - rect_origin.1 - pan.1) / scale,
    )
}

/// The new `pan` that keeps the canvas point under the cursor fixed while the zoom
/// changes from `z_old` to `z_new` (zoom-to-cursor): find the canvas point under the
/// cursor at the old zoom, then solve `pan` so the same point sits under the cursor
/// at the new zoom. Pure + testable.
fn zoom_to_cursor(
    cursor: (f64, f64),
    rect_origin: (f64, f64),
    pan: (f64, f64),
    z_old: f64,
    z_new: f64,
) -> (f64, f64) {
    let canvas = screen_to_canvas(cursor, rect_origin, pan, z_old);
    (
        cursor.0 - rect_origin.0 - z_new * canvas.0,
        cursor.1 - rect_origin.1 - z_new * canvas.1,
    )
}

/// The `(pan, zoom)` that frames every node within a `viewport` (the SVG's
/// width/height), with a margin — i.e. fit-to-view. `None` for an empty graph. The
/// zoom is clamped to a sane range; the content's bounding-box center is mapped to
/// the viewport center. Pure + testable.
fn fit_transform(nodes: &[FlowNode], viewport: (f64, f64)) -> Option<((f64, f64), f64)> {
    let first = nodes.first()?;
    let (mut min_x, mut min_y) = (first.position.x, first.position.y);
    let (mut max_x, mut max_y) = (first.position.x + NODE_W, first.position.y + NODE_H);
    for n in nodes {
        min_x = min_x.min(n.position.x);
        min_y = min_y.min(n.position.y);
        max_x = max_x.max(n.position.x + NODE_W);
        max_y = max_y.max(n.position.y + NODE_H);
    }
    let pad = 48.0;
    let (vw, vh) = viewport;
    let content_w = (max_x - min_x).max(1.0);
    let content_h = (max_y - min_y).max(1.0);
    let scale = ((vw - 2.0 * pad) / content_w)
        .min((vh - 2.0 * pad) / content_h)
        .clamp(0.4, 1.5);
    let (cx, cy) = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
    let pan = (vw / 2.0 - scale * cx, vh / 2.0 - scale * cy);
    Some((pan, scale))
}

/// A fresh node id not already used in `graph`, derived from a monotonically
/// incrementing `counter` (the component keeps it in a signal — deterministic,
/// unlike `Math.random` which is unavailable in this pure core). Returns the id and
/// the next counter value; skips any id that (improbably) already exists.
#[must_use]
pub fn fresh_id(graph: &FlowGraph, prefix: &str, counter: u64) -> (String, u64) {
    let existing: HashSet<&str> = graph.nodes.iter().map(|n| n.id.as_str()).collect();
    let mut n = counter;
    loop {
        let id = format!("{prefix}{n}");
        n += 1;
        if !existing.contains(id.as_str()) {
            return (id, n);
        }
    }
}

/// Add a node to the graph (returns a new graph — the signal is updated by clone, in
/// keeping with the crate's immutable-update style). The caller supplies an id from
/// [`fresh_id`]; a duplicate id is rejected by [`validate_flow`] before save.
#[must_use]
pub fn add_node(mut graph: FlowGraph, node: FlowNode) -> FlowGraph {
    graph.nodes.push(node);
    graph
}

/// Remove a node and every edge touching it.
#[must_use]
pub fn remove_node(mut graph: FlowGraph, id: &str) -> FlowGraph {
    graph.nodes.retain(|n| n.id != id);
    graph.edges.retain(|e| e.from != id && e.to != id);
    graph
}

/// Add an edge, rejecting a self-loop and a duplicate (same from/to/ports). Returns
/// the (possibly unchanged) graph; the no-op cases keep the canvas idempotent.
#[must_use]
pub fn add_edge(mut graph: FlowGraph, edge: FlowEdge) -> FlowGraph {
    if edge.from == edge.to {
        return graph;
    }
    let dup = graph.edges.iter().any(|e| {
        e.from == edge.from
            && e.to == edge.to
            && e.from_port == edge.from_port
            && e.to_port == edge.to_port
    });
    if dup {
        return graph;
    }
    graph.edges.push(edge);
    graph
}

/// Interpret a finished wiring gesture as the edge to store. Two gestures mean
/// the same commit gate (SOUL §11/§28) and both must work:
///
/// - trigger `commit` port → write node (the canonical spelling), and
/// - write node's output → the **collect trigger** itself (the intuitive
///   reverse: "when this write succeeds, the collector may advance its cursor").
///   Normalized into the canonical commit edge — never stored as a write→trigger
///   execution edge, which could only ever cycle.
///
/// A collect trigger holds ONE commit target, so wiring a new commit gate
/// replaces that trigger's previous one. A gesture with no meaning — any edge
/// into a non-collect trigger, or a commit gate landing on a trigger — returns
/// the graph untouched plus a human-readable message for the canvas banner.
#[must_use]
pub fn wire_edge(mut graph: FlowGraph, edge: FlowEdge) -> (FlowGraph, Option<String>) {
    let node_kind = |id: &str| graph.nodes.iter().find(|n| n.id == id);
    let to_is_trigger = node_kind(&edge.to).is_some_and(FlowNode::is_trigger);
    let from_is_trigger = node_kind(&edge.from).is_some_and(FlowNode::is_trigger);
    let to_is_collect = node_kind(&edge.to).is_some_and(|n| match &n.kind {
        FlowKind::Trigger { trigger } => trigger_is_collect(trigger),
        _ => false,
    });

    if edge.from == edge.to {
        return (graph, None); // self-loop: same silent no-op as add_edge
    }
    if edge.from_port == COMMIT_PORT && to_is_trigger {
        return (
            graph,
            Some(
                "the commit gate must point at the write node whose success advances \
                 the collector's cursor — not at another trigger"
                    .to_string(),
            ),
        );
    }
    let edge = if to_is_trigger {
        if to_is_collect && !from_is_trigger {
            // The reverse commit gesture: flip it into the canonical commit edge.
            FlowEdge {
                from: edge.to,
                to: edge.from,
                from_port: COMMIT_PORT.to_string(),
                to_port: String::new(),
            }
        } else {
            return (
                graph,
                Some(
                    "a trigger has no input — wire from the trigger's output port to \
                     an action instead"
                        .to_string(),
                ),
            );
        }
    } else {
        edge
    };
    if edge.from_port == COMMIT_PORT {
        // One commit target per collect trigger: rewiring replaces the old gate.
        graph
            .edges
            .retain(|e| !(e.from_port == COMMIT_PORT && e.from == edge.from));
    }
    (add_edge(graph, edge), None)
}

/// Remove the edge at `index` (a click on a rendered edge passes its position).
/// Out-of-range is a no-op.
#[must_use]
pub fn remove_edge(mut graph: FlowGraph, index: usize) -> FlowGraph {
    if index < graph.edges.len() {
        graph.edges.remove(index);
    }
    graph
}

// ---------------------------------------------------------------------------
// Geometry constants + per-node port anchor points (shared by render + wiring).
// ---------------------------------------------------------------------------

/// Rendered node box width / height (canvas units == SVG user units).
const NODE_W: f64 = 168.0;
const NODE_H: f64 = 64.0;

/// Canvas snap grid (matches the dotted background). Node positions snap to it so
/// graphs stay tidy without fiddly pixel-aligning.
const GRID: f64 = 24.0;

/// Snap a canvas coordinate to the nearest [`GRID`] line. Pure + testable.
fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// The canvas-space center of a node's **input** port (left edge, vertically
/// centered).
fn in_port_point(node: &FlowNode) -> (f64, f64) {
    (node.position.x, node.position.y + NODE_H / 2.0)
}

/// Where a commit gate lands on its target node: bottom edge, slightly left of
/// center — visually a gate tapping the write node, kept clear of the input port
/// so the data edge and the gate never overlap into one unreadable double-arrow.
fn commit_anchor_point(node: &FlowNode) -> (f64, f64) {
    (node.position.x + NODE_W * 0.35, node.position.y + NODE_H)
}

/// The canvas-space center of a node's **output** port `port` (right edge). A
/// condition stacks its `"true"` / `"false"` ports; every other kind has one
/// centered port.
fn out_port_point(node: &FlowNode, port: &str) -> (f64, f64) {
    let x = node.position.x + NODE_W;
    let ports = node.kind.out_ports();
    let idx = ports.iter().position(|p| *p == port).unwrap_or(0);
    let n = ports.len().max(1);
    // Evenly distribute n ports down the right edge.
    let step = NODE_H / (n as f64 + 1.0);
    let y = node.position.y + step * (idx as f64 + 1.0);
    (x, y)
}

// ---------------------------------------------------------------------------
// Local interaction state.
// ---------------------------------------------------------------------------

/// A node drag in progress: the dragged node id and the grab offset (cursor minus
/// node origin, in canvas space) so the node tracks the pointer without jumping.
#[derive(Clone)]
struct Drag {
    id: String,
    offset: (f64, f64),
}

/// A pending edge connection in progress: the source node + out-port, plus the live
/// cursor point (canvas space) drawn as a temp path until the pointer lands on an
/// in-port (commit) or elsewhere (cancel).
#[derive(Clone)]
struct Pending {
    from: String,
    from_port: String,
    cursor: (f64, f64),
}

/// A canvas pan in progress: the pan value at grab time + the grab client point, so
/// the pan tracks the pointer delta.
#[derive(Clone, Copy)]
struct Panning {
    start_pan: (f64, f64),
    start_client: (f64, f64),
}

// ---------------------------------------------------------------------------
// The component.
// ---------------------------------------------------------------------------

/// The drag-and-drop SVG flow editor. The parent owns `graph` (an `RwSignal`) so it
/// can persist it into `spec.graph`; all interaction state (drag / pan /
/// pending-connection / selection / id-counter) is local.
#[component]
pub fn FlowEditor(graph: RwSignal<FlowGraph>) -> impl IntoView {
    // A NodeRef to the <svg> root so pointer math can read its on-screen origin.
    let svg_ref: NodeRef<leptos::svg::Svg> = NodeRef::new();

    // Interaction state — all ephemeral, local to the editor.
    let drag = RwSignal::new(Option::<Drag>::None);
    let pending = RwSignal::new(Option::<Pending>::None);
    let panning = RwSignal::new(Option::<Panning>::None);
    let pan = RwSignal::new((0.0_f64, 0.0_f64));
    let zoom = RwSignal::new(1.0_f64);
    let selected = RwSignal::new(Option::<String>::None);
    let id_counter = RwSignal::new(1_u64);
    let error = RwSignal::new(Option::<String>::None);

    // The SVG root's on-screen top-left, for screen→canvas conversion.
    let rect_origin = move || {
        svg_ref
            .get_untracked()
            .map(|el| {
                let el: web_sys::Element = el.unchecked_into();
                let r = el.get_bounding_client_rect();
                (r.left(), r.top())
            })
            .unwrap_or((0.0, 0.0))
    };
    // The SVG root's on-screen size, for fit-to-view (falls back to a sane default
    // before the element is measured).
    let rect_size = move || {
        svg_ref
            .get_untracked()
            .map(|el| {
                let el: web_sys::Element = el.unchecked_into();
                let r = el.get_bounding_client_rect();
                (r.width(), r.height())
            })
            .unwrap_or((800.0, 600.0))
    };

    // Add a node of `kind` at a default canvas spot, offset per new node so they
    // don't stack exactly. Selects the new node.
    let add = move |make: fn() -> FlowKind, prefix: &'static str| {
        let (id, next) = fresh_id(&graph.get_untracked(), prefix, id_counter.get_untracked());
        id_counter.set(next);
        // Stagger placement using the counter so successive adds cascade.
        let offset = (next % 6) as f64 * 26.0;
        let node = FlowNode {
            id: id.clone(),
            kind: make(),
            position: FlowPos {
                x: snap(60.0 + offset),
                y: snap(60.0 + offset),
            },
        };
        graph.update(|g| *g = add_node(g.clone(), node));
        selected.set(Some(id));
        error.set(None);
    };

    // Like `add`, but inserts an already-built `kind` (the palette buttons pass a
    // `fn` constructor; the node-type search passes a node built from a search hit's
    // example payload, which a `fn` pointer can't capture).
    let add_built = move |kind: FlowKind, prefix: &'static str| {
        let (id, next) = fresh_id(&graph.get_untracked(), prefix, id_counter.get_untracked());
        id_counter.set(next);
        let offset = (next % 6) as f64 * 26.0;
        let node = FlowNode {
            id: id.clone(),
            kind,
            position: FlowPos {
                x: snap(60.0 + offset),
                y: snap(60.0 + offset),
            },
        };
        graph.update(|g| *g = add_node(g.clone(), node));
        selected.set(Some(id));
        error.set(None);
    };

    // Insert a paired For-each head + End-loop tail (SOUL §11): a loop-region
    // skeleton with the tail pre-pointing at its head, side by side, so only the
    // body wiring (head → body nodes → tail) remains. Selects the head so its
    // config opens.
    let add_loop = move || {
        let g0 = graph.get_untracked();
        let (head_id, next) = fresh_id(&g0, "each", id_counter.get_untracked());
        let (end_id, next) = {
            // The head isn't inserted yet, so guard the (improbable) collision by
            // deriving the tail id from the bumped counter with a distinct prefix.
            let (id, n) = fresh_id(&g0, "end_each", next);
            (id, n)
        };
        id_counter.set(next);
        let offset = (next % 6) as f64 * 26.0;
        let head = FlowNode {
            id: head_id.clone(),
            kind: FlowKind::ForEach {
                source: String::new(),
                item: "item".to_string(),
                index: None,
                max_iterations: None,
            },
            position: FlowPos {
                x: snap(60.0 + offset),
                y: snap(60.0 + offset),
            },
        };
        let end = FlowNode {
            id: end_id,
            kind: FlowKind::LoopEnd {
                for_each: head_id.clone(),
            },
            position: FlowPos {
                x: snap(60.0 + offset + NODE_W * 2.0),
                y: snap(60.0 + offset),
            },
        };
        graph.update(|g| {
            let with_head = add_node(g.clone(), head);
            *g = add_node(with_head, end);
        });
        selected.set(Some(head_id));
        error.set(None);
    };

    // --- Node-type semantic search (SOUL §11) ------------------------------------
    // Query the backend's node-type catalog by intent and insert the chosen node,
    // pre-filled from its example. Lets an author discover the right trigger/action
    // without knowing the palette's fixed taxonomy.
    let node_query = RwSignal::new(String::new());
    let node_results = RwSignal::new(Vec::<NodeTypeHit>::new());
    let node_searching = RwSignal::new(false);
    let node_search_err = RwSignal::new(Option::<String>::None);

    let run_node_search = move || {
        let q = node_query.get_untracked().trim().to_string();
        if q.is_empty() {
            node_results.set(Vec::new());
            node_search_err.set(None);
            return;
        }
        node_searching.set(true);
        node_search_err.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::search_automation_node_types(token.as_deref(), &q, 10).await {
                Ok(hits) => node_results.set(hits),
                Err(e) => {
                    node_search_err.set(Some(e.to_string()));
                    node_results.set(Vec::new());
                }
            }
            node_searching.set(false);
        });
    };

    // Insert the node a search result describes, then clear the results.
    let insert_hit = move |hit: NodeTypeHit| {
        if let Some((kind, prefix)) = hit_to_flow_kind(&hit) {
            add_built(kind, prefix);
            node_results.set(Vec::new());
            node_query.set(String::new());
        }
    };

    // Insert a starter template (a small pre-wired graph) with fresh ids; selects
    // its first node so its config opens.
    let add_template = move |nodes: Vec<TemplateNode>, edges: Vec<(usize, usize, &'static str)>| {
        let (g, counter, ids) = instantiate_template(
            graph.get_untracked(),
            nodes,
            edges,
            id_counter.get_untracked(),
        );
        id_counter.set(counter);
        graph.set(g);
        selected.set(ids.into_iter().next());
        error.set(None);
    };

    // Begin dragging a node: record its id + the grab offset (canvas space).
    let start_node_drag = move |ev: PointerEvent, id: String| {
        ev.stop_propagation();
        let canvas = screen_to_canvas(
            (ev.client_x() as f64, ev.client_y() as f64),
            rect_origin(),
            pan.get_untracked(),
            zoom.get_untracked(),
        );
        let origin = graph
            .get_untracked()
            .nodes
            .iter()
            .find(|n| n.id == id)
            .map(|n| (n.position.x, n.position.y))
            .unwrap_or((0.0, 0.0));
        drag.set(Some(Drag {
            id: id.clone(),
            offset: (canvas.0 - origin.0, canvas.1 - origin.1),
        }));
        selected.set(Some(id));
    };

    // Begin a pending connection from an out-port.
    let start_connection = move |ev: PointerEvent, from: String, from_port: String| {
        ev.stop_propagation();
        let cursor = screen_to_canvas(
            (ev.client_x() as f64, ev.client_y() as f64),
            rect_origin(),
            pan.get_untracked(),
            zoom.get_untracked(),
        );
        pending.set(Some(Pending {
            from,
            from_port,
            cursor,
        }));
    };

    // Land a pending connection on an in-port → create an edge. `wire_edge`
    // normalizes the reverse commit gesture (write output → collect trigger) into
    // the trigger's commit gate; a meaningless gesture surfaces in the banner.
    let finish_connection = move |ev: PointerEvent, to: String| {
        ev.stop_propagation();
        if let Some(p) = pending.get_untracked() {
            let mut msg = None;
            graph.update(|g| {
                let (wired, e) = wire_edge(
                    g.clone(),
                    FlowEdge {
                        from: p.from,
                        to: to.clone(),
                        from_port: p.from_port,
                        to_port: String::new(),
                    },
                );
                *g = wired;
                msg = e;
            });
            error.set(msg);
        }
        pending.set(None);
    };

    // Begin panning when the pointer goes down on empty canvas (also clears the
    // selection so the config panel collapses).
    let start_pan = move |ev: PointerEvent| {
        // A press on empty background: cancel any pending connection, start a pan.
        pending.set(None);
        selected.set(None);
        panning.set(Some(Panning {
            start_pan: pan.get_untracked(),
            start_client: (ev.client_x() as f64, ev.client_y() as f64),
        }));
    };

    // The canvas-root pointermove: drive whichever interaction is active.
    let on_move = move |ev: PointerEvent| {
        let client = (ev.client_x() as f64, ev.client_y() as f64);
        if let Some(d) = drag.get_untracked() {
            let canvas = screen_to_canvas(
                client,
                rect_origin(),
                pan.get_untracked(),
                zoom.get_untracked(),
            );
            // Snap to the grid so dropped nodes line up tidily.
            let x = snap(canvas.0 - d.offset.0);
            let y = snap(canvas.1 - d.offset.1);
            graph.update(|g| {
                if let Some(n) = g.nodes.iter_mut().find(|n| n.id == d.id) {
                    n.position = FlowPos { x, y };
                }
            });
        } else if let Some(mut p) = pending.get_untracked() {
            p.cursor = screen_to_canvas(
                client,
                rect_origin(),
                pan.get_untracked(),
                zoom.get_untracked(),
            );
            pending.set(Some(p));
        } else if let Some(pn) = panning.get_untracked() {
            pan.set((
                pn.start_pan.0 + (client.0 - pn.start_client.0),
                pn.start_pan.1 + (client.1 - pn.start_client.1),
            ));
        }
    };

    // The canvas-root pointerup: end whatever was active. A pending connection that
    // didn't land on an in-port is cancelled here.
    let on_up = move |_ev: PointerEvent| {
        drag.set(None);
        pending.set(None);
        panning.set(None);
    };

    // Scroll-wheel zoom, centered on the cursor (the canvas point under the pointer
    // stays put). Clamped to a sane range.
    let on_wheel = move |ev: WheelEvent| {
        ev.prevent_default();
        let z_old = zoom.get_untracked();
        let factor = if ev.delta_y() < 0.0 { 1.1 } else { 1.0 / 1.1 };
        let z_new = (z_old * factor).clamp(0.4, 2.5);
        if (z_new - z_old).abs() < f64::EPSILON {
            return;
        }
        let cursor = (ev.client_x() as f64, ev.client_y() as f64);
        let pan_new = zoom_to_cursor(cursor, rect_origin(), pan.get_untracked(), z_old, z_new);
        zoom.set(z_new);
        pan.set(pan_new);
    };

    // Delete the selected node + its edges.
    let delete_selected = move || {
        if let Some(id) = selected.get_untracked() {
            graph.update(|g| *g = remove_node(g.clone(), &id));
            selected.set(None);
        }
    };

    // Duplicate the selected node (a fresh id + offset; edges aren't copied) and
    // select the copy — quick way to reuse a configured node.
    let duplicate_selected = move || {
        if let Some(id) = selected.get_untracked() {
            let (g, next, new_id) =
                duplicate_node(graph.get_untracked(), &id, id_counter.get_untracked());
            id_counter.set(next);
            graph.set(g);
            if let Some(nid) = new_id {
                selected.set(Some(nid));
            }
        }
    };

    // Keyboard: Delete / Backspace removes the selected node (+ its edges), the
    // expected node-editor gesture. The canvas `<svg>` is focusable (tabindex) and
    // the config-panel inputs live *outside* it, so a keystroke while editing a
    // field never reaches here — Backspace-while-typing can't nuke the node.
    let on_canvas_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if matches!(ev.key().as_str(), "Delete" | "Backspace") && selected.get_untracked().is_some()
        {
            ev.prevent_default();
            delete_selected();
        }
    };

    // The validation banner reflects the current graph.
    let validity = move || validate_flow(&graph.get()).err();

    view! {
        <div class="flow">
            <div class="flow-palette">
                <span class="flow-palette-label">"Add node"</span>
                <button
                    class="flow-pal-btn flow-pal-trigger"
                    type="button"
                    on:click=move |_| {
                        add(
                            || FlowKind::Trigger { trigger: default_trigger() },
                            "trigger",
                        )
                    }
                >
                    "+ Trigger"
                </button>
                <button
                    class="flow-pal-btn flow-pal-action"
                    type="button"
                    on:click=move |_| {
                        add(|| FlowKind::Action { action: default_action() }, "action")
                    }
                >
                    "+ Action"
                </button>
                <button
                    class="flow-pal-btn flow-pal-agent"
                    type="button"
                    on:click=move |_| {
                        add(|| FlowKind::Action { action: default_agent_action() }, "agent")
                    }
                >
                    "+ Agent"
                </button>
                <button
                    class="flow-pal-btn flow-pal-classifier"
                    type="button"
                    on:click=move |_| {
                        add(
                            || FlowKind::Action { action: default_classifier_action() },
                            "classify",
                        )
                    }
                >
                    "+ Classifier"
                </button>
                <button
                    class="flow-pal-btn flow-pal-code"
                    type="button"
                    on:click=move |_| {
                        add(
                            || FlowKind::Code {
                                runtime: "js".to_string(),
                                source: String::new(),
                            },
                            "code",
                        )
                    }
                >
                    "+ Code"
                </button>
                <button
                    class="flow-pal-btn flow-pal-condition"
                    type="button"
                    on:click=move |_| {
                        add(
                            || FlowKind::Condition {
                                runtime: "js".to_string(),
                                source: "// Return true to take the 'true' branch, false for 'false'.\n// `input` = { trigger, inputs: { <upstream-node-id>: output } }\nreturn true;".to_string(),
                            },
                            "cond",
                        )
                    }
                >
                    "+ Condition"
                </button>
                <button
                    class="flow-pal-btn flow-pal-loop"
                    type="button"
                    title="A For-each head + End-loop tail: run the nodes wired between them once per element of an array"
                    on:click=move |_| add_loop()
                >
                    "+ Loop"
                </button>
                <span class="flow-palette-sep"></span>
                <span class="flow-palette-label">"Templates"</span>
                <button
                    class="flow-pal-btn flow-pal-agent"
                    type="button"
                    title="A channel-message trigger wired to a tool-calling Agent that replies on the channel"
                    on:click=move |_| {
                        let (n, e) = chatbot_template();
                        add_template(n, e);
                    }
                >
                    "Channel chatbot"
                </button>
                <button
                    class="flow-pal-btn flow-pal-trigger"
                    type="button"
                    title="A daily schedule trigger wired to an Agent that runs a task each morning"
                    on:click=move |_| {
                        let (n, e) = scheduled_template();
                        add_template(n, e);
                    }
                >
                    "Scheduled assistant"
                </button>
                <Show when=move || validity().is_some() fallback=|| ().into_view()>
                    <span class="flow-invalid">{move || validity().unwrap_or_default()}</span>
                </Show>
            </div>

            // Semantic node-type search: describe what you want, get ranked node
            // types from the backend catalog, click one to drop it in pre-filled.
            <div class="flow-node-search">
                <form
                    class="flow-node-search-bar"
                    on:submit=move |ev: leptos::ev::SubmitEvent| {
                        ev.prevent_default();
                        run_node_search();
                    }
                >
                    <span class="flow-palette-label">"Find node type"</span>
                    <input
                        class="flow-node-search-input"
                        type="text"
                        placeholder="Describe what you need — e.g. “every morning”, “when an email arrives”, “post to a channel”"
                        prop:value=move || node_query.get()
                        on:input=move |ev| node_query.set(event_target_value(&ev))
                    />
                    <button class="flow-pal-btn" type="submit">
                        "Search"
                    </button>
                    <Show when=move || {
                        !node_results.get().is_empty() || !node_query.get().is_empty()
                    } fallback=|| ().into_view()>
                        <button
                            class="flow-pal-btn"
                            type="button"
                            on:click=move |_| {
                                node_results.set(Vec::new());
                                node_query.set(String::new());
                                node_search_err.set(None);
                            }
                        >
                            "Clear"
                        </button>
                    </Show>
                    <Show when=move || node_searching.get() fallback=|| ().into_view()>
                        <span class="flow-node-search-status">"Searching…"</span>
                    </Show>
                </form>
                <Show when=move || node_search_err.get().is_some() fallback=|| ().into_view()>
                    <div class="flow-node-search-err">
                        {move || node_search_err.get().unwrap_or_default()}
                    </div>
                </Show>
                <Show
                    when=move || !node_results.get().is_empty()
                    fallback=|| ().into_view()
                >
                    <div class="flow-node-results">
                        {move || {
                            node_results
                                .get()
                                .into_iter()
                                .map(|hit| {
                                    let h = hit.clone();
                                    view! {
                                        <button
                                            class="flow-node-result"
                                            type="button"
                                            title=hit.description.clone()
                                            on:click=move |_| insert_hit(h.clone())
                                        >
                                            <span class=format!(
                                                "flow-node-result-badge flow-rb-{}",
                                                hit.node_kind,
                                            )>{node_kind_badge(&hit)}</span>
                                            <span class="flow-node-result-main">
                                                <span class="flow-node-result-title">
                                                    {hit.title.clone()}
                                                </span>
                                                <span class="flow-node-result-summary">
                                                    {hit.summary.clone()}
                                                </span>
                                            </span>
                                        </button>
                                    }
                                })
                                .collect::<Vec<_>>()
                        }}
                    </div>
                </Show>
            </div>

            <div class="flow-main">
                <div class="flow-canvas-wrap">
                    <svg
                        node_ref=svg_ref
                        class="flow-canvas"
                        tabindex="0"
                        on:pointerdown=start_pan
                        on:pointermove=on_move
                        on:pointerup=on_up
                        on:pointerleave=on_up
                        on:wheel=on_wheel
                        on:keydown=on_canvas_keydown
                    >
                        <defs>
                            <marker
                                id="flow-arrow"
                                markerWidth="9"
                                markerHeight="9"
                                refX="8"
                                refY="3"
                                orient="auto"
                                markerUnits="strokeWidth"
                            >
                                <path class="flow-arrow" d="M0,0 L8,3 L0,6 Z"></path>
                            </marker>
                            // The commit gate's own arrowhead — colour-matched to the
                            // dashed commit edge (a marker can't inherit the path's
                            // stroke, so it needs its own def).
                            <marker
                                id="flow-arrow-commit"
                                markerWidth="9"
                                markerHeight="9"
                                refX="8"
                                refY="3"
                                orient="auto"
                                markerUnits="strokeWidth"
                            >
                                <path class="flow-arrow-commit" d="M0,0 L8,3 L0,6 Z"></path>
                            </marker>
                        </defs>
                        <g transform=move || {
                            let (px, py) = pan.get();
                            let z = zoom.get();
                            format!("translate({px:.1},{py:.1}) scale({z:.3})")
                        }>
                            // Committed edges (click to delete).
                            {move || {
                                let g = graph.get();
                                g.edges
                                    .iter()
                                    .enumerate()
                                    .map(|(i, e)| {
                                        let d = edge_endpoints(&g, e)
                                            .map(|(a, b)| if e.from_port == COMMIT_PORT {
                                                commit_edge_path(a, b)
                                            } else {
                                                edge_path(a, b)
                                            })
                                            .unwrap_or_default();
                                        // A commit gate renders dashed/distinct with its
                                        // own arrowhead, so it reads as "advance the
                                        // cursor when this write succeeds", not a second
                                        // data-flow edge.
                                        let (class, marker) = if e.from_port == COMMIT_PORT {
                                            ("flow-edge flow-edge-commit", "url(#flow-arrow-commit)")
                                        } else {
                                            ("flow-edge", "url(#flow-arrow)")
                                        };
                                        view! {
                                            <path
                                                class=class
                                                d=d
                                                marker-end=marker
                                                on:pointerdown=move |ev: PointerEvent| {
                                                    ev.stop_propagation();
                                                    graph.update(|g| *g = remove_edge(g.clone(), i));
                                                }
                                            ></path>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }}

                            // The live pending-connection path.
                            {move || {
                                pending
                                    .get()
                                    .and_then(|p| {
                                        let g = graph.get();
                                        let from = g
                                            .nodes
                                            .iter()
                                            .find(|n| n.id == p.from)
                                            .map(|n| out_port_point(n, &p.from_port))?;
                                        Some(
                                            view! {
                                                <path
                                                    class="flow-edge flow-edge-pending"
                                                    d=if p.from_port == COMMIT_PORT {
                                                        commit_edge_path(from, p.cursor)
                                                    } else {
                                                        edge_path(from, p.cursor)
                                                    }
                                                ></path>
                                            },
                                        )
                                    })
                            }}

                            // Nodes.
                            {move || {
                                let g = graph.get();
                                // Flag problem nodes (⚠): unreachable from any trigger
                                // (never runs) or a dead-end trigger (fires but runs
                                // nothing) — the commonest "why didn't it work?" bugs.
                                let problems = problem_nodes(&g);
                                g.nodes
                                    .iter()
                                    .cloned()
                                    .map(|n| {
                                        let warn = problems.contains(&n.id);
                                        node_view(
                                            n,
                                            warn,
                                            selected,
                                            start_node_drag,
                                            start_connection,
                                            finish_connection,
                                        )
                                    })
                                    .collect::<Vec<_>>()
                            }}
                        </g>
                    </svg>
                    // Wiring-gesture feedback (a meaningless connection attempt).
                    {move || {
                        error
                            .get()
                            .map(|msg| {
                                view! {
                                    <div class="flow-wire-hint" role="status">
                                        <span>{msg}</span>
                                        <button
                                            type="button"
                                            aria-label="Dismiss"
                                            on:click=move |_| error.set(None)
                                        >
                                            <Icon icon=MdIcon::Close />
                                        </button>
                                    </div>
                                }
                            })
                    }}
                    <Show
                        when=move || graph.with(|g| g.nodes.is_empty())
                        fallback=|| ().into_view()
                    >
                        <div class="flow-empty">
                            <p class="flow-empty-title">"Build an automation visually"</p>
                            <p class="flow-empty-sub">
                                "Add a node from the palette above (Trigger · Action · Agent · Code · Condition · Loop), wire output→input ports, and configure each in the side panel — or drop in a Template like the Channel chatbot."
                            </p>
                        </div>
                    </Show>
                    <div class="flow-zoom">
                        <button
                            class="flow-zoom-btn"
                            type="button"
                            title="Fit all nodes to view"
                            on:click=move |_| {
                                if let Some((p, z))
                                    = fit_transform(&graph.get_untracked().nodes, rect_size())
                                {
                                    pan.set(p);
                                    zoom.set(z);
                                }
                            }
                        >
                            "Fit"
                        </button>
                        <button
                            class="flow-zoom-btn"
                            type="button"
                            title="Zoom out"
                            on:click=move |_| zoom.update(|z| *z = (*z / 1.2).max(0.4))
                        >
                            "−"
                        </button>
                        <button
                            class="flow-zoom-btn flow-zoom-pct"
                            type="button"
                            title="Reset view"
                            on:click=move |_| {
                                zoom.set(1.0);
                                pan.set((0.0, 0.0));
                            }
                        >
                            {move || format!("{:.0}%", zoom.get() * 100.0)}
                        </button>
                        <button
                            class="flow-zoom-btn"
                            type="button"
                            title="Zoom in"
                            on:click=move |_| zoom.update(|z| *z = (*z * 1.2).min(2.5))
                        >
                            "+"
                        </button>
                    </div>
                </div>

                <ConfigPanel
                    graph=graph
                    selected=selected
                    on_delete=delete_selected
                    on_duplicate=duplicate_selected
                />
            </div>
        </div>
    }
}

/// Resolve an edge's two endpoint anchor points against the current graph, or `None`
/// if either node is gone (a stale edge mid-edit). A commit gate lands on the target
/// node's **bottom edge**, not its input port — the data edge already occupies the
/// input, and stacking both on one anchor drew two indistinguishable overlapping
/// arrows (it's a gate tapping the node, not a second input).
fn edge_endpoints(g: &FlowGraph, e: &FlowEdge) -> Option<((f64, f64), (f64, f64))> {
    let from = g.nodes.iter().find(|n| n.id == e.from)?;
    let to = g.nodes.iter().find(|n| n.id == e.to)?;
    let to_point = if e.from_port == COMMIT_PORT {
        commit_anchor_point(to)
    } else {
        in_port_point(to)
    };
    Some((out_port_point(from, &e.from_port), to_point))
}

/// Render one node as an SVG `<g>`: a rounded box, its title (kind + id), an input
/// port, and one output port per [`FlowKind::out_ports`]. Wired to the drag /
/// connection callbacks.
fn node_view(
    n: FlowNode,
    warn: bool,
    selected: RwSignal<Option<String>>,
    start_drag: impl Fn(PointerEvent, String) + Copy + 'static,
    start_conn: impl Fn(PointerEvent, String, String) + Copy + 'static,
    finish_conn: impl Fn(PointerEvent, String) + Copy + 'static,
) -> impl IntoView {
    let id = n.id.clone();
    let pos = n.position;
    let (suffix, icon, kind_label) = kind_meta(&n.kind);
    let id_label = id.clone();
    let sub = node_subtitle(&n);
    let is_sel = {
        let id = id.clone();
        move || selected.get().as_deref() == Some(id.as_str())
    };
    // `warn` (unreachable from any trigger) adds a soft amber outline + a ⚠ marker.
    let warn_cls = if warn { " flow-node-warn" } else { "" };
    let class = move || {
        if is_sel() {
            format!("flow-node flow-node-{suffix} flow-node-selected{warn_cls}")
        } else {
            format!("flow-node flow-node-{suffix}{warn_cls}")
        }
    };
    let (in_x, in_y) = in_port_point(&n);
    // Port anchors are relative to the node origin (the <g> translate).
    let in_rel = (in_x - pos.x, in_y - pos.y);

    let out_ports = n
        .kind
        .out_ports()
        .iter()
        .map(|port| {
            let port = (*port).to_string();
            let (ox, oy) = out_port_point(&n, &port);
            let rel = (ox - pos.x, oy - pos.y);
            let id_for_conn = id.clone();
            let port_for_conn = port.clone();
            let label = if port.is_empty() {
                String::new()
            } else {
                port.clone()
            };
            let (port_class, label_class) = if label.is_empty() {
                (
                    "flow-port flow-port-out".to_string(),
                    "flow-port-label".to_string(),
                )
            } else {
                (
                    format!("flow-port flow-port-out flow-port-{label}"),
                    format!("flow-port-label flow-plabel-{label}"),
                )
            };
            view! {
                <g>
                    <circle
                        class=port_class
                        cx=rel.0
                        cy=rel.1
                        r="6.5"
                        on:pointerdown=move |ev: PointerEvent| {
                            start_conn(ev, id_for_conn.clone(), port_for_conn.clone())
                        }
                    ></circle>
                    <Show when={
                        let has = !label.is_empty();
                        move || has
                    } fallback=|| ().into_view()>
                        // Branch label sits OUTSIDE the node, to the right of its port,
                        // colour-matched (true=green / false=red), so a condition's two
                        // outputs are unambiguous when wiring.
                        <text class=label_class.clone() x=rel.0 + 11.0 y=rel.1 + 3.5>
                            {label.clone()}
                        </text>
                    </Show>
                </g>
            }
        })
        .collect::<Vec<_>>();

    let id_for_drag = id.clone();
    let id_for_in = id.clone();

    view! {
        <g
            class=class
            transform=format!("translate({:.1},{:.1})", pos.x, pos.y)
            on:pointerdown=move |ev: PointerEvent| start_drag(ev, id_for_drag.clone())
        >
            <rect class="flow-node-box" x="0" y="0" width=NODE_W height=NODE_H rx="12"></rect>
            <rect class="flow-node-accent" x="11" y="0" width=NODE_W - 22.0 height="3" rx="1.5"></rect>
            <text class="flow-node-icon" x="13" y="28">{icon}</text>
            <text class="flow-node-title" x="36" y="27">{kind_label}</text>
            <text class="flow-node-id" x=NODE_W - 12.0 y="22">{id_label}</text>
            <text class="flow-node-sub" x="14" y="49">{sub}</text>
            // Unreachable-node warning marker (bottom-right): this node isn't wired
            // to any trigger, so it will never run.
            {warn
                .then(|| {
                    view! {
                        <text class="flow-node-warn-mark" x=NODE_W - 13.0 y=NODE_H - 8.0>
                            "⚠"
                        </text>
                    }
                })}
            // Input port.
            <circle
                class="flow-port flow-port-in"
                cx=in_rel.0
                cy=in_rel.1
                r="6.5"
                on:pointerup=move |ev: PointerEvent| finish_conn(ev, id_for_in.clone())
            ></circle>
            {out_ports}
        </g>
    }
}

/// Build the [`FlowKind`] (and an id prefix) to insert from a node-type search
/// hit's example payload (`{id, kind, …}`). `None` if the hit's `node_kind` is one
/// the canvas doesn't model. Pure + total.
fn hit_to_flow_kind(hit: &NodeTypeHit) -> Option<(FlowKind, &'static str)> {
    let ex = &hit.example;
    let code_field = |key: &str, default: &str| {
        ex.get(key)
            .and_then(Value::as_str)
            .unwrap_or(default)
            .to_string()
    };
    match hit.node_kind.as_str() {
        "trigger" => ex
            .get("trigger")
            .cloned()
            .map(|trigger| (FlowKind::Trigger { trigger }, "trigger")),
        "action" => ex
            .get("action")
            .cloned()
            .map(|action| (FlowKind::Action { action }, "action")),
        "code" => Some((
            FlowKind::Code {
                runtime: code_field("runtime", "js"),
                source: code_field("source", ""),
            },
            "code",
        )),
        "for_each" => Some((
            FlowKind::ForEach {
                source: code_field("source", ""),
                item: code_field("item", "item"),
                index: ex.get("index").and_then(Value::as_str).map(str::to_string),
                max_iterations: ex
                    .get("max_iterations")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
            },
            "each",
        )),
        // The example's `for_each` references the catalog's own sample head, which
        // won't exist in this graph — insert unpaired and let the picker bind it.
        "loop_end" => Some((
            FlowKind::LoopEnd {
                for_each: String::new(),
            },
            "end_each",
        )),
        "condition" => Some((
            FlowKind::Condition {
                runtime: code_field("runtime", "js"),
                source: code_field("source", ""),
            },
            "cond",
        )),
        _ => None,
    }
}

/// The short badge label shown on a node-type search result (mirrors the canvas
/// node titles). Pure + total.
fn node_kind_badge(hit: &NodeTypeHit) -> &'static str {
    match hit.node_kind.as_str() {
        "trigger" => "Trigger",
        "action" => "Action",
        "code" => "Code",
        "condition" => "If",
        "for_each" => "Loop",
        "loop_end" => "End",
        _ => "Node",
    }
}

/// Per-kind display metadata: a CSS class suffix (drives the canvas colour
/// identity + the palette), a glyph icon, and a short label. Pure + total.
fn kind_meta(kind: &FlowKind) -> (&'static str, &'static str, &'static str) {
    match kind {
        FlowKind::Trigger { .. } => ("trigger", "⚡", "Trigger"),
        // A classifier is an `llm_agent` too, so test it before the plain agent.
        FlowKind::Action { action } if is_classifier(action) => ("classify", "📊", "Classifier"),
        FlowKind::Action { action } if is_agent_action(action) => ("agent", "🤖", "Agent"),
        FlowKind::Action { .. } => ("action", "▷", "Action"),
        FlowKind::Code { .. } => ("code", "{ }", "Code"),
        FlowKind::Condition { .. } => ("condition", "◆", "If"),
        FlowKind::ForEach { .. } => ("loop", "🔁", "For each"),
        FlowKind::LoopEnd { .. } => ("loop", "⏹", "End loop"),
    }
}

/// Whether an action payload is an `llm_agent` action — rendered as a first-class
/// "Agent" node (a tool-calling LLM agent, e.g. a channel chatbot).
fn is_agent_action(action: &Value) -> bool {
    action.get("kind").and_then(Value::as_str) == Some("llm_agent")
}

/// Whether an `llm_agent` action is a **Classifier** — an agent steered to emit a
/// JSON map of `outcome → 0.0–1.0 probability`. Marked by its web-only `outcomes`
/// list (the backend just sees a JSON-steered `llm_agent`).
fn is_classifier(action: &Value) -> bool {
    is_agent_action(action) && action.get("outcomes").and_then(Value::as_array).is_some()
}

/// A one-line node subtitle for the box: the trigger/action kind, or the code
/// runtime — a glanceable summary of the node's payload.
fn node_subtitle(n: &FlowNode) -> String {
    match &n.kind {
        FlowKind::Trigger { trigger } => trigger
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("trigger")
            .to_string(),
        FlowKind::Action { action } if is_classifier(action) => {
            let n = action
                .get("outcomes")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            format!("classify · {n} outcome{}", if n == 1 { "" } else { "s" })
        }
        FlowKind::Action { action } if is_agent_action(action) => {
            let n = action
                .get("tools")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if n == 0 {
                "agent · all tools".to_string()
            } else {
                format!("agent · {n} tool{}", if n == 1 { "" } else { "s" })
            }
        }
        FlowKind::Action { action } => action
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("action")
            .to_string(),
        FlowKind::Code { runtime, .. } => format!("code · {runtime}"),
        FlowKind::Condition { runtime, .. } => format!("if · {runtime}"),
        FlowKind::ForEach { source, item, .. } => format!("{item} in {source}"),
        FlowKind::LoopEnd { for_each } => {
            if for_each.is_empty() {
                "pick a For-each head".to_string()
            } else {
                format!("closes {for_each}")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The per-node config panel.
// ---------------------------------------------------------------------------

/// The side panel that edits the selected node: a typed trigger builder, an action
/// kind + params editor, or a code/condition runtime + source editor, plus a delete
/// button. Reads/writes the node in place on the shared `graph` signal.
#[component]
fn ConfigPanel(
    graph: RwSignal<FlowGraph>,
    selected: RwSignal<Option<String>>,
    on_delete: impl Fn() + Copy + Send + Sync + 'static,
    on_duplicate: impl Fn() + Copy + Send + Sync + 'static,
) -> impl IntoView {
    // The currently-selected node, recomputed from the graph.
    let node = move || {
        let id = selected.get()?;
        graph.with(|g| g.nodes.iter().find(|n| n.id == id).cloned())
    };

    view! {
        // On mobile this is a bottom sheet: `flow-config-open` (set only when a node
        // is selected) slides it up over the canvas; otherwise it stays off-screen so
        // the canvas keeps full height. On desktop it's a static right rail.
        <aside class=move || {
            if node().is_some() { "flow-config flow-config-open" } else { "flow-config" }
        }>
            <Show
                when=move || node().is_some()
                fallback=|| {
                    view! {
                        <div class="flow-config-empty">
                            "Select a node to edit it, or add one from the palette."
                        </div>
                    }
                }
            >
                {move || {
                    // Rebuild the editor only when the SELECTED node changes — never
                    // when its data mutates. The typed editors below own their input
                    // state locally and write straight back to `graph` on every
                    // keystroke; if this closure tracked `graph` (via `node()`), each
                    // keystroke would tear the whole editor down and rebuild it —
                    // dropping the focused input's caret and re-firing the collect
                    // configs' `spawn_local` connection fetch per character. The Poll
                    // cadence / mailbox fields (nested a dynamic layer deeper inside
                    // `trigger_config`) felt this worst. So track `selected` only and
                    // read the node's data untracked. A node's `FlowKind` variant is
                    // fixed for its id, and sub-kind switches are handled by the
                    // editors' own local signals, so nothing here needs `graph`.
                    let Some(id) = selected.get() else { return ().into_any() };
                    let Some(n) = graph
                        .with_untracked(|g| g.nodes.iter().find(|n| n.id == id).cloned())
                    else {
                        return ().into_any();
                    };
                    let head = view! {
                        <div class="flow-config-head">
                            <span class="flow-config-title">
                                {format!("{} · {}", n.kind.tag(), n.id)}
                            </span>
                            <div class="flow-cfg-head-btns">
                                <button
                                    class="flow-cfg-btn"
                                    type="button"
                                    title="Duplicate this node"
                                    on:click=move |_| on_duplicate()
                                >
                                    "Duplicate"
                                </button>
                                // Mobile-only: dismiss the bottom sheet (deselect). On
                                // desktop the rail is always shown, so it's hidden.
                                <button
                                    class="flow-cfg-btn flow-cfg-close"
                                    type="button"
                                    title="Close"
                                    on:click=move |_| selected.set(None)
                                >
                                    <Icon icon=MdIcon::Close />
                                </button>
                                <button
                                    class="flow-cfg-btn flow-cfg-del"
                                    type="button"
                                    on:click=move |_| on_delete()
                                >
                                    "Delete"
                                </button>
                            </div>
                        </div>
                    };
                    let body = match &n.kind {
                        FlowKind::Trigger { trigger } => {
                            trigger_config(graph, id.clone(), trigger.clone()).into_any()
                        }
                        FlowKind::Action { action } => {
                            action_config(graph, id.clone(), action.clone()).into_any()
                        }
                        FlowKind::Code { runtime, source }
                        | FlowKind::Condition { runtime, source } => {
                            let is_cond = matches!(n.kind, FlowKind::Condition { .. });
                            code_config(
                                    graph,
                                    id.clone(),
                                    runtime.clone(),
                                    source.clone(),
                                    is_cond,
                                )
                                .into_any()
                        }
                        FlowKind::ForEach { .. } => {
                            for_each_config(graph, id.clone()).into_any()
                        }
                        FlowKind::LoopEnd { for_each } => {
                            loop_end_config(graph, id.clone(), for_each.clone()).into_any()
                        }
                    };
                    view! {
                        <div class="flow-config-body">
                            {head}
                            {body}
                        </div>
                    }
                        .into_any()
                }}
            </Show>
        </aside>
    }
}

/// Trigger-node config: a kind `<select>` + **typed fields** per kind (channel,
/// cron, path, mailbox + filters, …), shared with the Raw-mode trigger builder via
/// [`trigger_fields`]. Each field **merges** into the trigger object, preserving any
/// opaque predicate (e.g. a channel `filter`) the typed form doesn't model; changing
/// the kind resets the trigger to that kind. Opaque-only kinds (`calendar_event`)
/// show a hint instead.
fn trigger_config(graph: RwSignal<FlowGraph>, id: String, trigger: Value) -> impl IntoView {
    let current_kind = trigger
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("webhook")
        .to_string();
    let kind_sig = RwSignal::new(current_kind);
    let id_for_kind = id.clone();
    let id_for_body = id.clone();

    view! {
        <label class="flow-cfg-label">"Trigger kind"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                let k = event_target_value(&ev);
                // Switching kind resets the trigger to that bare kind (its fields differ).
                set_node_trigger(graph, &id_for_kind, serde_json::json!({ "kind": k }));
                kind_sig.set(k);
            }
            prop:value=move || kind_sig.get()
        >
            {TRIGGER_KINDS
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        {move || {
            let id = id_for_body.clone();
            let kind = kind_sig.get();
            // A collect source trigger configures its email/calendar source inline
            // (SOUL §28: no global settings) + wires its `commit` port to a write
            // node — so it gets a typed config, not the generic field list.
            if kind == "collect_email" {
                return collect_email_config(graph, id).into_any();
            }
            if kind == "collect_calendar" {
                return collect_calendar_config(graph, id).into_any();
            }
            if kind == "collect_sql" {
                return collect_sql_config(graph, id).into_any();
            }
            let fields = trigger_fields(&kind);
            if fields.is_empty() {
                return view! {
                    <div class="flow-cfg-hint">
                        "This trigger fires on its kind alone — no extra fields. (Advanced predicates can be set in Raw mode.)"
                    </div>
                }
                .into_any();
            }
            let trigger = node_trigger(graph, &id);
            fields
                .iter()
                .map(|(key, label, _required, multiline)| {
                    let key = *key;
                    let sig = RwSignal::new(trigger_field_display(&trigger, key));
                    let id2 = id.clone();
                    let input = if *multiline {
                        view! {
                            <textarea
                                class="flow-cfg-area"
                                prop:value=move || sig.get()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    sig.set(v.clone());
                                    set_trigger_field(graph, &id2, key, &v);
                                }
                            ></textarea>
                        }
                        .into_any()
                    } else {
                        view! {
                            <input
                                class="flow-cfg-input"
                                prop:value=move || sig.get()
                                on:input=move |ev| {
                                    let v = event_target_value(&ev);
                                    sig.set(v.clone());
                                    set_trigger_field(graph, &id2, key, &v);
                                }
                            />
                        }
                        .into_any()
                    };
                    view! {
                        <div class="flow-cfg-field">
                            <label class="flow-cfg-label">{*label}</label>
                            {input}
                        </div>
                    }
                })
                .collect::<Vec<_>>()
                .into_any()
        }}
    }
}

/// "Use an existing source" picker shared by both collect configs (SOUL §28):
/// most collect nodes should point at a source that already exists — re-entering
/// credentials per node was the only path before. `existing` is the
/// already-kind-filtered list; picking hands the chosen connection to `on_pick`
/// (which writes the id onto the trigger and flips the node to configured).
/// Renders nothing while the list is empty.
fn existing_source_picker(
    existing: RwSignal<Vec<Connection>>,
    on_pick: impl Fn(Connection) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <Show when=move || !existing.get().is_empty() fallback=|| ().into_view()>
            <div class="flow-cfg-field">
                <label class="flow-cfg-label">"Use an existing source"</label>
                <select
                    class="flow-cfg-input"
                    on:change=move |ev| {
                        let id = event_target_value(&ev);
                        if let Some(c) = existing
                            .get_untracked()
                            .into_iter()
                            .find(|c| c.id == id)
                        {
                            on_pick(c);
                        }
                    }
                >
                    <option value="" selected>"— pick a source —"</option>
                    {move || {
                        existing
                            .get()
                            .into_iter()
                            .map(|c| {
                                let text = if c.collecting {
                                    c.name.clone()
                                } else {
                                    format!("{} · idle", c.name)
                                };
                                view! { <option value=c.id.clone()>{text}</option> }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
                <div class="flow-cfg-hint">"…or create a new one below."</div>
            </div>
        </Show>
    }
}

/// A labelled text/password input bound to a string signal (one config row).
fn cfg_field(
    label: &'static str,
    placeholder: &'static str,
    sig: RwSignal<String>,
    password: bool,
) -> impl IntoView {
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">{label}</label>
            <input
                class="flow-cfg-input"
                r#type=if password { "password" } else { "text" }
                placeholder=placeholder
                prop:value=move || sig.get()
                on:input=move |ev| sig.set(event_target_value(&ev))
            />
        </div>
    }
}

/// The **poll cadence** (`every`) row shared by both collect source configs (SOUL
/// §29). A free text input accepting the documented shapes — a bare number of minutes,
/// a compact duration string (`5m`, `1h30m`), or `{"seconds":N}` — persisted verbatim
/// onto the trigger's `every` field via [`set_trigger_every`]. A soft client-side
/// pattern check ([`every_shape_ok`]) warns on an unrecognized shape but NEVER blocks
/// saving; the server re-parses and clamps `[60s, 1 year]` at scan time.
fn collect_every_field(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let sig = RwSignal::new(every_display(&node_trigger(graph, &id)));
    let id_sv = StoredValue::new(id);
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Poll cadence (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder=r#"e.g. 5m, 1h30m, or {"seconds":90}"#
                prop:value=move || sig.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    sig.set(v.clone());
                    set_trigger_every(graph, &id_sv.get_value(), &v);
                }
            />
            <Show
                when=move || {
                    let v = sig.get();
                    !v.trim().is_empty() && !every_shape_ok(&v)
                }
                fallback=|| ().into_view()
            >
                <div class="flow-cfg-warn">
                    "Unrecognized cadence shape — saved as-is. Use minutes (5m, 1h30m), a bare number of minutes, or {\"seconds\":N}."
                </div>
            </Show>
            <div class="flow-cfg-hint">
                "How often to poll. Clamped to 60s–1 year at scan time; blank = every scheduler tick (60s)."
            </div>
        </div>
    }
}

/// Inline **email source** config for a `CollectEmail` node (SOUL §28: the source
/// is set up in the node, not a global settings tab). A provider form whose "Create
/// source" button creates the connection and writes its id into the trigger's
/// `connection`; plus an optional mailbox filter. The cursor-commit write is wired
/// via the node's `commit` port, not typed here.
fn collect_email_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    // The node id lives in a Copy `StoredValue` so the event-handler closures (which
    // need it) stay `Fn`/`Copy` rather than moving a `String` out of the view body.
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let configured = RwSignal::new(!initial.is_empty());
    let conn_label = RwSignal::new(initial.clone());

    // The workspace's existing email sources: fed to the reuse picker, and used to
    // resolve an already-configured trigger's connection id to its NAME (the ✓
    // badge showed a raw uuid before).
    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_email_connections(tok.as_deref()).await {
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
    });

    let provider = RwSignal::new(EmailProviderKind::Maildir);
    let name = RwSignal::new(String::new());
    let root = RwSignal::new(String::new());
    let folder = RwSignal::new(String::new());
    let host = RwSignal::new(String::new());
    let port = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let session_url = RwSignal::new(String::new());
    let token = RwSignal::new(String::new());
    let account_id = RwSignal::new(String::new());
    let client_id = RwSignal::new(String::new());
    let client_secret = RwSignal::new(String::new());
    let refresh_token = RwSignal::new(String::new());
    let label = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let err = RwSignal::new(Option::<String>::None);
    let mailbox = RwSignal::new(
        trigger
            .get("mailbox")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    // Edit-an-existing-source mode (SOUL §28): `editing` holds the connection id
    // being edited (the form is prefilled from its stored, secret-free settings
    // and the submit PUTs instead of POSTing); `edit_provider` remembers the
    // stored provider so secrets may be left blank (= keep) only while the
    // provider is unchanged — a provider switch needs fresh credentials.
    let editing = RwSignal::new(Option::<String>::None);
    let edit_provider = RwSignal::new(Option::<EmailProviderKind>::None);

    let opt = move |s: RwSignal<String>| {
        let v = s.get_untracked().trim().to_string();
        (!v.is_empty()).then_some(v)
    };
    // Prefill the provider form from the stored source and enter edit mode. The
    // connection id is read fresh off the trigger (the picker may have changed it).
    let start_edit = move || {
        let trig = node_trigger(graph, &id_sv.get_value());
        let Some(cid) = trig
            .get("connection")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return;
        };
        err.set(None);
        busy.set(true);
        spawn_local(async move {
            let tok = auth::resolve_token();
            match rest::get_email_connection(tok.as_deref(), &cid).await {
                Ok(d) => {
                    let prov = EmailProviderKind::parse_token(&d.provider).unwrap_or_default();
                    provider.set(prov);
                    name.set(d.name.clone());
                    root.set(d.setting("root"));
                    // Maildir stores its folder under `name`; IMAP under `mailbox`.
                    folder.set(match prov {
                        EmailProviderKind::Maildir => d.setting("name"),
                        _ => d.setting("mailbox"),
                    });
                    host.set(d.setting("host"));
                    port.set(d.setting("port"));
                    username.set(d.setting("username"));
                    session_url.set(d.setting("session_url"));
                    account_id.set(d.setting("account_id"));
                    client_id.set(d.setting("client_id"));
                    label.set(d.setting("label"));
                    // Secrets never arrive — blank means "keep the stored one".
                    password.set(String::new());
                    token.set(String::new());
                    client_secret.set(String::new());
                    refresh_token.set(String::new());
                    edit_provider.set(Some(prov));
                    editing.set(Some(d.id));
                    configured.set(false);
                }
                Err(e) => err.set(Some(format!("Could not load the source: {e}"))),
            }
            busy.set(false);
        });
    };
    // Create the source (POST) — or, in edit mode, save it (PUT). When editing
    // with the provider unchanged, blank secret fields keep the stored secrets
    // (the server back-fills them); otherwise secrets are required as on create.
    let submit = move || {
        let prov = provider.get_untracked();
        let nm = name.get_untracked().trim().to_string();
        err.set(None);
        if nm.is_empty() {
            err.set(Some("Give the source a name.".into()));
            return;
        }
        let editing_id = editing.get_untracked();
        let secrets_kept = editing_id.is_some() && edit_provider.get_untracked() == Some(prov);
        let mut body = CreateEmailConnection {
            provider: prov,
            name: nm,
            ..Default::default()
        };
        let missing: Option<&str> = match prov {
            EmailProviderKind::Maildir => match opt(root) {
                Some(r) => {
                    body.root = r;
                    body.mailbox = opt(folder);
                    None
                }
                None => Some("Enter the Maildir directory (the folder with new/ cur/ tmp/)."),
            },
            EmailProviderKind::Imap => match (opt(host), opt(username), opt(password)) {
                (Some(h), Some(u), p) if p.is_some() || secrets_kept => {
                    body.host = Some(h);
                    body.username = Some(u);
                    body.password = p;
                    body.port = opt(port).and_then(|p| p.parse::<u16>().ok());
                    body.mailbox = opt(folder);
                    None
                }
                _ => Some("IMAP needs a host, username, and password."),
            },
            EmailProviderKind::Jmap => match (opt(session_url), opt(token)) {
                (Some(u), t) if t.is_some() || secrets_kept => {
                    body.session_url = Some(u);
                    body.token = t;
                    body.account_id = opt(account_id);
                    None
                }
                _ => Some("JMAP needs a session URL and a bearer token."),
            },
            EmailProviderKind::Gmail => {
                match (opt(client_id), opt(client_secret), opt(refresh_token)) {
                    (Some(c), s, r) if (s.is_some() && r.is_some()) || secrets_kept => {
                        body.client_id = Some(c);
                        body.client_secret = s;
                        body.refresh_token = r;
                        body.label = opt(label);
                        None
                    }
                    _ => Some("Gmail needs a client id, client secret, and refresh token."),
                }
            }
        };
        if let Some(m) = missing {
            err.set(Some(m.into()));
            return;
        }
        busy.set(true);
        let id2 = id_sv.get_value();
        spawn_local(async move {
            let tok = auth::resolve_token();
            let result = match &editing_id {
                Some(cid) => rest::update_email_connection(tok.as_deref(), cid, &body).await,
                None => rest::create_email_connection(tok.as_deref(), &body).await,
            };
            match result {
                Ok(c) => {
                    set_trigger_field(graph, &id2, "connection", &c.id);
                    conn_label.set(c.name.clone());
                    // Keep the reuse picker's entry fresh (a rename shows at once).
                    existing.update(|list| {
                        if let Some(e) = list.iter_mut().find(|e| e.id == c.id) {
                            e.name = c.name.clone();
                        }
                    });
                    editing.set(None);
                    edit_provider.set(None);
                    configured.set(true);
                    busy.set(false);
                }
                Err(e) => {
                    err.set(Some(e.to_string()));
                    busy.set(false);
                }
            }
        });
    };
    let is = move |k: EmailProviderKind| move || provider.get() == k;

    view! {
        <Show when=move || configured.get() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">
                {move || format!("✓ Email source configured ({})", conn_label.get())}
            </div>
            <button
                class="flow-cfg-btn"
                disabled=move || busy.get()
                title="View and change this source's settings"
                on:click=move |_| start_edit()
            >
                {move || if busy.get() { "Loading…" } else { "View / edit source" }}
            </button>
            <button
                class="flow-cfg-btn"
                on:click=move |_| {
                    editing.set(None);
                    edit_provider.set(None);
                    configured.set(false);
                }
            >
                "Switch source"
            </button>
        </Show>
        <Show when=move || !configured.get() fallback=|| ().into_view()>
            <Show when=move || editing.with(Option::is_none) fallback=|| ().into_view()>
                {existing_source_picker(existing, move |c: Connection| {
                    set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
                    conn_label.set(c.name);
                    configured.set(true);
                })}
            </Show>
            <Show when=move || editing.with(Option::is_some) fallback=|| ().into_view()>
                <div class="flow-cfg-hint">
                    "Editing this source — leave the secret fields blank to keep the stored values."
                </div>
                <button
                    class="flow-cfg-btn"
                    on:click=move |_| {
                        editing.set(None);
                        edit_provider.set(None);
                        err.set(None);
                        configured.set(true);
                    }
                >
                    "Cancel edit"
                </button>
            </Show>
            <label class="flow-cfg-label">"Email provider"</label>
            <select
                class="flow-cfg-input"
                on:change=move |ev| {
                    if let Some(k) = EmailProviderKind::parse_token(&event_target_value(&ev)) {
                        provider.set(k);
                    }
                }
            >
                {EmailProviderKind::all()
                    .into_iter()
                    .map(|k| {
                        let sel = move || provider.get() == k;
                        view! { <option value=k.as_str() selected=sel>{k.label()}</option> }
                    })
                    .collect::<Vec<_>>()}
            </select>
            {cfg_field("Name", "Personal inbox", name, false)}
            <Show when=is(EmailProviderKind::Maildir) fallback=|| ().into_view()>
                {cfg_field("Maildir directory", "/home/me/Mail/INBOX", root, false)}
                {cfg_field("Mailbox name (optional)", "INBOX", folder, false)}
            </Show>
            <Show when=is(EmailProviderKind::Imap) fallback=|| ().into_view()>
                {cfg_field("Host", "imap.example.com", host, false)}
                {cfg_field("Port (optional, default 993)", "993", port, false)}
                {cfg_field("Username", "me@example.com", username, false)}
                {cfg_field("Password", "", password, true)}
                {cfg_field("Folder (optional)", "INBOX", folder, false)}
            </Show>
            <Show when=is(EmailProviderKind::Jmap) fallback=|| ().into_view()>
                {cfg_field("Session URL", "https://api.fastmail.com/jmap/session", session_url, false)}
                {cfg_field("Bearer token", "", token, true)}
                {cfg_field("Account id (optional)", "", account_id, false)}
            </Show>
            <Show when=is(EmailProviderKind::Gmail) fallback=|| ().into_view()>
                {cfg_field("Client id", "", client_id, false)}
                {cfg_field("Client secret", "", client_secret, true)}
                {cfg_field("Refresh token", "", refresh_token, true)}
                {cfg_field("Label (optional)", "INBOX", label, false)}
            </Show>
            <button class="flow-cfg-btn" disabled=move || busy.get() on:click=move |_| submit()>
                {move || match (busy.get(), editing.with(Option::is_some)) {
                    (true, true) => "Saving…",
                    (true, false) => "Creating…",
                    (false, true) => "Save changes",
                    (false, false) => "Create source",
                }}
            </button>
        </Show>
        <Show when=move || err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || err.get().unwrap_or_default()}</div>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Mailbox filter (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="INBOX"
                prop:value=move || mailbox.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    mailbox.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "mailbox", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the write node whose success advances the cursor (optional)."
        </div>
    }
}

/// Inline **calendar source** config for a `CollectCalendar` node — the calendar
/// twin of [`collect_email_config`]. Creates a calendar connection (local `.ics`
/// directory, CalDAV, or webcal) and writes its id into the trigger's `connection`.
fn collect_calendar_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let configured = RwSignal::new(!initial.is_empty());
    let conn_label = RwSignal::new(initial.clone());

    // Existing calendar sources for the reuse picker (`GET /connections` returns
    // every kind — keep only calendars) + uuid → name resolution for the ✓ badge.
    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_connections(tok.as_deref()).await {
            let list: Vec<Connection> = list.into_iter().filter(|c| c.kind == "calendar").collect();
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
    });

    let provider = RwSignal::new(CalendarProviderKind::Local);
    let name = RwSignal::new(String::new());
    let target = RwSignal::new(String::new());
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let err = RwSignal::new(Option::<String>::None);
    let calendar = RwSignal::new(
        trigger
            .get("calendar")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );

    let opt = move |s: RwSignal<String>| {
        let v = s.get_untracked().trim().to_string();
        (!v.is_empty()).then_some(v)
    };
    let create = move || {
        let prov = provider.get_untracked();
        let nm = name.get_untracked().trim().to_string();
        err.set(None);
        if nm.is_empty() {
            err.set(Some("Give the source a name.".into()));
            return;
        }
        let Some(tgt) = opt(target) else {
            err.set(Some(
                "Enter the directory (local) or URL (CalDAV/webcal).".into(),
            ));
            return;
        };
        let mut config = Map::new();
        config.insert(prov.config_key().to_string(), Value::String(tgt));
        if matches!(prov, CalendarProviderKind::Caldav) {
            if let Some(u) = opt(username) {
                config.insert("username".to_string(), Value::String(u));
            }
            if let Some(p) = opt(password) {
                config.insert("password".to_string(), Value::String(p));
            }
        }
        let body = CreateConnection {
            kind: prov,
            name: nm,
            config: Value::Object(config),
            credentials: None,
        };
        busy.set(true);
        let id2 = id_sv.get_value();
        spawn_local(async move {
            let tok = auth::resolve_token();
            match rest::create_connection(tok.as_deref(), &body).await {
                Ok(c) => {
                    set_trigger_field(graph, &id2, "connection", &c.id);
                    conn_label.set(c.name);
                    configured.set(true);
                    busy.set(false);
                }
                Err(e) => {
                    err.set(Some(e.to_string()));
                    busy.set(false);
                }
            }
        });
    };
    let is = move |k: CalendarProviderKind| move || provider.get() == k;
    let is_local = move || provider.get() == CalendarProviderKind::Local;

    view! {
        <Show when=move || configured.get() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">
                {move || format!("✓ Calendar source configured ({})", conn_label.get())}
            </div>
            <button class="flow-cfg-btn" on:click=move |_| configured.set(false)>
                "Reconfigure source"
            </button>
        </Show>
        <Show when=move || !configured.get() fallback=|| ().into_view()>
            {existing_source_picker(existing, move |c: Connection| {
                set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
                conn_label.set(c.name);
                configured.set(true);
            })}
            <label class="flow-cfg-label">"Calendar provider"</label>
            <select
                class="flow-cfg-input"
                on:change=move |ev| {
                    if let Some(k) = CalendarProviderKind::parse_token(&event_target_value(&ev)) {
                        provider.set(k);
                    }
                }
            >
                {[
                    CalendarProviderKind::Local,
                    CalendarProviderKind::Caldav,
                    CalendarProviderKind::Webcal,
                ]
                    .into_iter()
                    .map(|k| {
                        let sel = move || provider.get() == k;
                        view! { <option value=k.as_str() selected=sel>{k.label()}</option> }
                    })
                    .collect::<Vec<_>>()}
            </select>
            {cfg_field("Name", "Home calendar", name, false)}
            <Show when=is_local fallback=|| ().into_view()>
                {cfg_field("Directory (.ics files)", "/srv/calendars", target, false)}
            </Show>
            <Show when=move || !is_local() fallback=|| ().into_view()>
                {cfg_field("URL", "https://dav.example.com/cal/", target, false)}
            </Show>
            <Show when=is(CalendarProviderKind::Caldav) fallback=|| ().into_view()>
                {cfg_field("Username (optional)", "", username, false)}
                {cfg_field("Password (optional)", "", password, true)}
            </Show>
            <Show when=move || err.with(Option::is_some) fallback=|| ().into_view()>
                <div class="flow-cfg-err">{move || err.get().unwrap_or_default()}</div>
            </Show>
            <button class="flow-cfg-btn" disabled=move || busy.get() on:click=move |_| create()>
                {move || if busy.get() { "Creating…" } else { "Create source" }}
            </button>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Calendar filter (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="(every calendar on the connection)"
                prop:value=move || calendar.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    calendar.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "calendar", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the write node whose success advances the cursor (optional)."
        </div>
    }
}

/// Inline **external database source** config for a `CollectSql` node (SOUL
/// §11/§19): pick an existing external Postgres connection (they are registered
/// by an admin over `POST /db/connections` — there is no inline create form),
/// plus the tables wildcard pattern, an optional cursor column, and the poll
/// cadence. The cursor-commit node is wired via the node's `commit` port.
fn collect_sql_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let trigger = node_trigger(graph, &id);
    let id_sv = StoredValue::new(id);
    let initial = trigger
        .get("connection")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // The picked connection's display name (the raw uuid until the list resolves).
    let conn_label = RwSignal::new(initial.clone());
    let loaded = RwSignal::new(false);

    let existing = RwSignal::new(Vec::<Connection>::new());
    spawn_local(async move {
        let tok = auth::resolve_token();
        if let Ok(list) = rest::list_db_connections(tok.as_deref()).await {
            if let Some(c) = list.iter().find(|c| c.id == initial) {
                conn_label.set(c.name.clone());
            }
            existing.set(list);
        }
        loaded.set(true);
    });

    let on_pick = move |c: Connection| {
        set_trigger_field(graph, &id_sv.get_value(), "connection", &c.id);
        conn_label.set(c.name);
    };

    let tables = RwSignal::new(
        trigger
            .get("tables")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );
    let cursor_column = RwSignal::new(
        trigger
            .get("cursor_column")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    );

    view! {
        {existing_source_picker(existing, on_pick)}
        <Show when=move || !conn_label.get().is_empty() fallback=|| ().into_view()>
            <div class="flow-cfg-hint">{move || format!("Source: {}", conn_label.get())}</div>
        </Show>
        <Show
            when=move || loaded.get() && existing.get().is_empty()
            fallback=|| ().into_view()
        >
            <div class="flow-cfg-hint">
                "No external Postgres connections yet — an admin registers one via POST /db/connections (or ask the assistant)."
            </div>
        </Show>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Tables pattern"</label>
            <input
                class="flow-cfg-input"
                placeholder="e.g. orders_* or analytics.fact_*"
                prop:value=move || tables.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    tables.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "tables", &v);
                }
            />
            <div class="flow-cfg-hint">
                "* is a wildcard; later-created matching tables join automatically. Fires one run per newly-inserted row — downstream nodes read trigger.row.<column>."
            </div>
        </div>
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Cursor column (optional)"</label>
            <input
                class="flow-cfg-input"
                placeholder="auto-detect: a serial id, else created_at"
                prop:value=move || cursor_column.get()
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    cursor_column.set(v.clone());
                    set_trigger_field(graph, &id_sv.get_value(), "cursor_column", &v);
                }
            />
        </div>
        {collect_every_field(graph, id_sv.get_value())}
        <div class="flow-cfg-hint">
            "Wire the ⚡ commit port to the node whose success advances the cursor (optional)."
        </div>
    }
}

/// Action-node config: an action-kind `<select>`, then either a typed **Agent**
/// form (when the kind is `llm_agent` — a tool-calling chatbot) or a params-JSON
/// textarea for any other action kind.
fn action_config(graph: RwSignal<FlowGraph>, id: String, action: Value) -> impl IntoView {
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("summarize")
        .to_string();
    let kind_sig = RwSignal::new(kind);
    let id_for_kind = id.clone();
    let id_for_body = id.clone();

    view! {
        <label class="flow-cfg-label">"Action kind"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                let k = event_target_value(&ev);
                // Switching kind resets the payload to a sensible default — an agent
                // gets its chatbot defaults, any other kind a bare `{kind}`.
                let next = if k == "llm_agent" {
                    default_agent_action()
                } else {
                    serde_json::json!({ "kind": k })
                };
                set_node_action(graph, &id_for_kind, next);
                kind_sig.set(k);
            }
            prop:value=move || kind_sig.get()
        >
            {ACTION_KINDS
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        {move || {
            let id = id_for_body.clone();
            match kind_sig.get().as_str() {
                "llm_agent" => {
                    // Both an Agent and a Classifier are `llm_agent`; pick the editor
                    // by the node's current shape (a classifier carries `outcomes`).
                    if is_classifier(&node_action(graph, &id)) {
                        classifier_config(graph, id).into_any()
                    } else {
                        agent_config(graph, id).into_any()
                    }
                }
                "notify" => notify_config(graph, id).into_any(),
                "create_chat_thread" => chat_thread_config(graph, id).into_any(),
                "create_note" => note_config(graph, id).into_any(),
                "create_task" => task_config(graph, id).into_any(),
                "create_event" => event_config(graph, id).into_any(),
                "summarize" => summarize_config(graph, id).into_any(),
                "write_object" => write_object_config(graph, id).into_any(),
                "move_object" => move_object_config(graph, id).into_any(),
                "webhook" => webhook_config(graph, id).into_any(),
                _ => params_config(graph, id).into_any(),
            }
        }}
    }
}

/// Typed config for a **Notify** action: a channel + message. The message can
/// reference upstream node output (it's available to the agent/runner), but the
/// common case is a fixed announcement; for a chatbot reply, an Agent node
/// auto-replies instead.
fn notify_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let msg_sig = RwSignal::new(str_field(&action, "message"));
    let chan_sig = RwSignal::new(str_field(&action, "channel"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = notify_params(&chan_sig.get_untracked(), &msg_sig.get_untracked());
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Channel (optional, default \"default\")"</label>
        <input
            class="flow-cfg-input"
            placeholder="ops"
            prop:value=move || chan_sig.get()
            on:input=move |ev| {
                chan_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Message"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="message to deliver…"
            prop:value=move || msg_sig.get()
            on:input=move |ev| {
                msg_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">"Delivers to a configured channel ([channels])."</div>
    }
}

/// Typed config for a **Create chat thread** output action. Its message commonly
/// templates the final text produced by an upstream Agent or Summarize node.
fn chat_thread_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let message_sig = RwSignal::new(str_field(&action, "message"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let value = chat_thread_params(&title_sig.get_untracked(), &message_sig.get_untracked());
        set_node_action(graph, &id_sv.get_value(), value);
    };
    view! {
        <label class="flow-cfg-label">"Thread title (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Automation output"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Message"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="{{ inputs.agent.text }}"
            prop:value=move || message_sig.get()
            on:input=move |ev| {
                message_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">
            "Creates a new thread in Chat with this assistant message. You can reference upstream output with {{ inputs.node.field }}."
        </div>
    }
}

/// Typed config for a **Create note** action: title, markdown body, and tags.
fn note_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let md_sig = RwSignal::new(str_field(&action, "markdown"));
    let tags_sig = RwSignal::new(
        action
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
    );
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = note_params(
            &title_sig.get_untracked(),
            &md_sig.get_untracked(),
            &tags_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Title"</label>
        <input
            class="flow-cfg-input"
            placeholder="note title"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Markdown (optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || md_sig.get()
            on:input=move |ev| {
                md_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Tags (comma-separated, optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="home, urgent"
            prop:value=move || tags_sig.get()
            on:input=move |ev| {
                tags_sig.set(event_target_value(&ev));
                apply();
            }
        />
    }
}

/// Typed config for a **Create task** action: a board id (required by the
/// backend), an optional column id (defaults to the board's first column), a
/// title, and an optional markdown body. Board/column are ids — the Boards panel
/// surfaces them — kept as free text here so a graph can target a known board
/// without dropping to raw JSON, mirroring how Notify takes a channel name.
fn task_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let board_sig = RwSignal::new(str_field(&action, "board_id"));
    let col_sig = RwSignal::new(str_field(&action, "column_id"));
    let title_sig = RwSignal::new(str_field(&action, "title"));
    let body_sig = RwSignal::new(str_field(&action, "body"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = task_params(
            &board_sig.get_untracked(),
            &col_sig.get_untracked(),
            &title_sig.get_untracked(),
            &body_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Board id"</label>
        <input
            class="flow-cfg-input"
            placeholder="board uuid"
            prop:value=move || board_sig.get()
            on:input=move |ev| {
                board_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Column id (optional, defaults to first)"</label>
        <input
            class="flow-cfg-input"
            placeholder="column uuid"
            prop:value=move || col_sig.get()
            on:input=move |ev| {
                col_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Title"</label>
        <input
            class="flow-cfg-input"
            placeholder="task title"
            prop:value=move || title_sig.get()
            on:input=move |ev| {
                title_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Body (markdown, optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || body_sig.get()
            on:input=move |ev| {
                body_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">
            "Board/column are ids (see the Boards panel); board_id + title are required."
        </div>
    }
}

/// Typed config for a **Create event** action: a summary, RFC-3339 start/end
/// times, an all-day toggle, and optional location / body / calendar id. Times
/// are free text (the backend validates RFC 3339 + `end >= start`), kept verbatim
/// so a fixed timestamp round-trips losslessly — no ambiguous local→UTC coercion.
fn event_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let summary_sig = RwSignal::new(str_field(&action, "summary"));
    let start_sig = RwSignal::new(str_field(&action, "start"));
    let end_sig = RwSignal::new(str_field(&action, "end"));
    let loc_sig = RwSignal::new(str_field(&action, "location"));
    let body_sig = RwSignal::new(str_field(&action, "body"));
    let cal_sig = RwSignal::new(str_field(&action, "calendar_id"));
    let allday_sig = RwSignal::new(
        action
            .get("all_day")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = event_params(
            &summary_sig.get_untracked(),
            &start_sig.get_untracked(),
            &end_sig.get_untracked(),
            &loc_sig.get_untracked(),
            &body_sig.get_untracked(),
            &cal_sig.get_untracked(),
            allday_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Summary"</label>
        <input
            class="flow-cfg-input"
            placeholder="event title"
            prop:value=move || summary_sig.get()
            on:input=move |ev| {
                summary_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Start (RFC 3339)"</label>
        <input
            class="flow-cfg-input"
            placeholder="2026-06-18T09:00:00Z"
            prop:value=move || start_sig.get()
            on:input=move |ev| {
                start_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"End (RFC 3339)"</label>
        <input
            class="flow-cfg-input"
            placeholder="2026-06-18T10:00:00Z"
            prop:value=move || end_sig.get()
            on:input=move |ev| {
                end_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-tool">
            <input
                type="checkbox"
                prop:checked=move || allday_sig.get()
                on:change=move |_| {
                    allday_sig.update(|b| *b = !*b);
                    apply();
                }
            />
            "All-day event"
        </label>
        <label class="flow-cfg-label">"Location (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="Room 1 / a URL"
            prop:value=move || loc_sig.get()
            on:input=move |ev| {
                loc_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Description (optional)"</label>
        <textarea
            class="flow-cfg-area"
            prop:value=move || body_sig.get()
            on:input=move |ev| {
                body_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Calendar id (optional, defaults to local)"</label>
        <input
            class="flow-cfg-input"
            placeholder="calendar uuid"
            prop:value=move || cal_sig.get()
            on:input=move |ev| {
                cal_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Times are RFC 3339 (e.g. 2026-06-18T09:00:00Z); end must not precede start."
        </div>
    }
}

/// Read a string action field, or `""`.
fn str_field(action: &Value, key: &str) -> String {
    action
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Insert `{key: trimmed-value}` into `obj` only when `value` is non-blank.
fn insert_if_set(obj: &mut Map<String, Value>, key: &str, value: &str) {
    let v = value.trim();
    if !v.is_empty() {
        obj.insert(key.to_string(), Value::String(v.to_string()));
    }
}

/// Build a `notify` action from the typed form (blank fields omitted). Pure +
/// testable.
fn notify_params(channel: &str, message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("notify".to_string()));
    insert_if_set(&mut obj, "message", message);
    insert_if_set(&mut obj, "channel", channel);
    Value::Object(obj)
}

/// Build a `create_chat_thread` action from the typed form. A blank title is
/// omitted so the backend supplies "Automation output"; `message` is required at
/// execution time.
fn chat_thread_params(title: &str, message: &str) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("create_chat_thread".to_string()),
    );
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "message", message);
    Value::Object(obj)
}

/// Build a `create_note` action from the typed form (blank fields omitted; tags
/// split on commas). Pure + testable.
fn note_params(title: &str, markdown: &str, tags_csv: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("create_note".to_string()));
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "markdown", markdown);
    let tags: Vec<Value> = tags_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect();
    if !tags.is_empty() {
        obj.insert("tags".to_string(), Value::Array(tags));
    }
    Value::Object(obj)
}

/// Build a `create_task` action from the typed form (blank fields omitted). The
/// backend `create_task` tool requires `board_id` + `title`; `column_id` defaults
/// to the board's first column and `body` is optional markdown. Pure + testable.
fn task_params(board_id: &str, column_id: &str, title: &str, body: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("create_task".to_string()));
    insert_if_set(&mut obj, "board_id", board_id);
    insert_if_set(&mut obj, "column_id", column_id);
    insert_if_set(&mut obj, "title", title);
    insert_if_set(&mut obj, "body", body);
    Value::Object(obj)
}

/// Build a `create_event` action from the typed form (blank fields omitted;
/// `all_day` emitted only when set, since the backend defaults it to false). The
/// backend `create_event` tool requires `summary` + RFC-3339 `start`/`end`;
/// `location`, `body`, and `calendar_id` are optional. Start/end stay free text in
/// the form (the tool itself documents RFC 3339, e.g. `2026-06-18T09:00:00Z`, and
/// validates the format + `end >= start` at run time), so the builder is a pure,
/// lossless pass-through — no ambiguous datetime-local→UTC conversion. Testable.
fn event_params(
    summary: &str,
    start: &str,
    end: &str,
    location: &str,
    body: &str,
    calendar_id: &str,
    all_day: bool,
) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("create_event".to_string()),
    );
    insert_if_set(&mut obj, "summary", summary);
    insert_if_set(&mut obj, "start", start);
    insert_if_set(&mut obj, "end", end);
    insert_if_set(&mut obj, "location", location);
    insert_if_set(&mut obj, "body", body);
    insert_if_set(&mut obj, "calendar_id", calendar_id);
    if all_day {
        obj.insert("all_day".to_string(), Value::Bool(true));
    }
    Value::Object(obj)
}

/// Build a `summarize` action from the typed form (blank fields omitted;
/// `max_words` only when it parses as a positive integer). Pure + testable.
fn summarize_params(input: &str, instructions: &str, max_words: &str, model: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("summarize".to_string()));
    insert_if_set(&mut obj, "input", input);
    insert_if_set(&mut obj, "instructions", instructions);
    if let Ok(words) = max_words.trim().parse::<u64>() {
        if words > 0 {
            obj.insert("max_words".to_string(), Value::from(words));
        }
    }
    insert_if_set(&mut obj, "model", model);
    Value::Object(obj)
}

/// Typed config for a **Summarize** action: the input to condense (usually a
/// `{{ path }}` template over an upstream node's output; unset = the firing
/// trigger event), optional steering instructions, a word bound, and a model
/// override. One LLM call, no tools — for tool-calling steps use an Agent node.
fn summarize_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let input_sig = RwSignal::new(str_field(&action, "input"));
    let instr_sig = RwSignal::new(str_field(&action, "instructions"));
    let words_sig = RwSignal::new(
        action
            .get("max_words")
            .and_then(Value::as_u64)
            .map(|w| w.to_string())
            .unwrap_or_default(),
    );
    let model_sig = RwSignal::new(str_field(&action, "model"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = summarize_params(
            &input_sig.get_untracked(),
            &instr_sig.get_untracked(),
            &words_sig.get_untracked(),
            &model_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Input (optional — empty summarizes the trigger event)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="{{ inputs.fetch.content }}"
            prop:value=move || input_sig.get()
            on:input=move |ev| {
                input_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Instructions (optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="Focus on action items; bullet list."
            prop:value=move || instr_sig.get()
            on:input=move |ev| {
                instr_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Max words (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="120"
            inputmode="numeric"
            prop:value=move || words_sig.get()
            on:input=move |ev| {
                words_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Model (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default model"
            prop:value=move || model_sig.get()
            on:input=move |ev| {
                model_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "One LLM call, no tools; downstream nodes read {{ inputs.<node-id>.summary }}."
        </div>
    }
}

/// Build a `write_object` action from the typed form (blank fields omitted).
/// Pure + testable. The form covers the text case; binary `content_base64` is
/// authoring-tool territory.
fn write_object_params(key: &str, content: &str, store: &str, content_type: &str) -> Value {
    let mut obj = Map::new();
    obj.insert(
        "kind".to_string(),
        Value::String("write_object".to_string()),
    );
    insert_if_set(&mut obj, "key", key);
    // Content is NOT trimmed like the id-ish fields: written bytes are verbatim
    // (leading/trailing whitespace can be intentional), only fully-empty is omitted.
    if !content.is_empty() {
        obj.insert("content".to_string(), Value::String(content.to_string()));
    }
    insert_if_set(&mut obj, "store", store);
    insert_if_set(&mut obj, "content_type", content_type);
    Value::Object(obj)
}

/// Typed config for a **Write object** action: create/overwrite a stored file
/// from content in hand — commonly a template over an upstream node's output.
fn write_object_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let key_sig = RwSignal::new(str_field(&action, "key"));
    let content_sig = RwSignal::new(str_field(&action, "content"));
    let store_sig = RwSignal::new(str_field(&action, "store"));
    let ctype_sig = RwSignal::new(str_field(&action, "content_type"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = write_object_params(
            &key_sig.get_untracked(),
            &content_sig.get_untracked(),
            &store_sig.get_untracked(),
            &ctype_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"Key"</label>
        <input
            class="flow-cfg-input"
            placeholder="reports/2026/summary.md"
            prop:value=move || key_sig.get()
            on:input=move |ev| {
                key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Content"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            placeholder="{{ inputs.agent.content }}"
            prop:value=move || content_sig.get()
            on:input=move |ev| {
                content_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <label class="flow-cfg-label">"Store (optional, default files store)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default"
            prop:value=move || store_sig.get()
            on:input=move |ev| {
                store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Content type (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="text/markdown"
            prop:value=move || ctype_sig.get()
            on:input=move |ev| {
                ctype_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Creates or overwrites the file; it is catalogued, searchable, and can fire storage triggers."
        </div>
    }
}

/// Build a `move_object` action from the typed form (blank fields omitted).
/// Pure + testable.
fn move_object_params(from_key: &str, to_key: &str, from_store: &str, to_store: &str) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("move_object".to_string()));
    insert_if_set(&mut obj, "from_key", from_key);
    insert_if_set(&mut obj, "to_key", to_key);
    insert_if_set(&mut obj, "from_store", from_store);
    insert_if_set(&mut obj, "to_store", to_store);
    Value::Object(obj)
}

/// Typed config for a **Move object** action: relocate a stored file (copy to
/// the destination, delete the source), within a store or across stores.
fn move_object_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let from_key_sig = RwSignal::new(str_field(&action, "from_key"));
    let to_key_sig = RwSignal::new(str_field(&action, "to_key"));
    let from_store_sig = RwSignal::new(str_field(&action, "from_store"));
    let to_store_sig = RwSignal::new(str_field(&action, "to_store"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let v = move_object_params(
            &from_key_sig.get_untracked(),
            &to_key_sig.get_untracked(),
            &from_store_sig.get_untracked(),
            &to_store_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"From key"</label>
        <input
            class="flow-cfg-input"
            placeholder="{{ trigger.key }}"
            prop:value=move || from_key_sig.get()
            on:input=move |ev| {
                from_key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"To key (optional, defaults to from key)"</label>
        <input
            class="flow-cfg-input"
            placeholder="archive/{{ trigger.key }}"
            prop:value=move || to_key_sig.get()
            on:input=move |ev| {
                to_key_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"From store (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="default"
            prop:value=move || from_store_sig.get()
            on:input=move |ev| {
                from_store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"To store (optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="archive"
            prop:value=move || to_store_sig.get()
            on:input=move |ev| {
                to_store_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <div class="flow-cfg-hint">
            "Copies to the destination, then deletes the source; to keep the source use copy_object from a Code node."
        </div>
    }
}

/// Build a `webhook` action from the typed form. `payload`/`headers` arrive as
/// **already-parsed** JSON objects (the form validates before applying); blank
/// url/method are omitted (method defaults to post server-side). Pure + testable.
fn webhook_params(
    url: &str,
    method: &str,
    payload: Option<Map<String, Value>>,
    headers: Option<Map<String, Value>>,
) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("webhook".to_string()));
    insert_if_set(&mut obj, "url", url);
    // "post" is the server default — omit it so the stored node stays minimal.
    if method != "post" {
        insert_if_set(&mut obj, "method", method);
    }
    if let Some(p) = payload.filter(|p| !p.is_empty()) {
        obj.insert("payload".to_string(), Value::Object(p));
    }
    if let Some(h) = headers.filter(|h| !h.is_empty()) {
        obj.insert("headers".to_string(), Value::Object(h));
    }
    Value::Object(obj)
}

/// Typed config for a **Webhook** action: deliver a JSON payload to an external
/// URL. Payload/headers are JSON-object textareas validated on input (an invalid
/// edit shows the error and leaves the node's last good state in place, like the
/// raw params editor).
fn webhook_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let url_sig = RwSignal::new(str_field(&action, "url"));
    let method_sig = RwSignal::new({
        let m = str_field(&action, "method");
        if m.is_empty() {
            "post".to_string()
        } else {
            m
        }
    });
    let json_obj_text = |key: &str| {
        action
            .get(key)
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty())
            .map(|m| serde_json::to_string_pretty(&Value::Object(m.clone())).unwrap_or_default())
            .unwrap_or_default()
    };
    let payload_sig = RwSignal::new(json_obj_text("payload"));
    let headers_sig = RwSignal::new(json_obj_text("headers"));
    let payload_err = RwSignal::new(Option::<String>::None);
    let headers_err = RwSignal::new(Option::<String>::None);
    let id_sv = StoredValue::new(id);
    // Apply only when BOTH JSON fields parse; an invalid edit surfaces its error
    // and leaves the node's last good params untouched.
    let apply = move || {
        let payload = match parse_params(&payload_sig.get_untracked()) {
            Ok(p) => {
                payload_err.set(None);
                p
            }
            Err(e) => {
                payload_err.set(Some(e));
                return;
            }
        };
        let headers = match parse_params(&headers_sig.get_untracked()) {
            Ok(h) => {
                headers_err.set(None);
                h
            }
            Err(e) => {
                headers_err.set(Some(e));
                return;
            }
        };
        let v = webhook_params(
            &url_sig.get_untracked(),
            &method_sig.get_untracked(),
            Some(payload),
            Some(headers),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };
    view! {
        <label class="flow-cfg-label">"URL"</label>
        <input
            class="flow-cfg-input"
            placeholder="https://hooks.example.com/notify"
            prop:value=move || url_sig.get()
            on:input=move |ev| {
                url_sig.set(event_target_value(&ev));
                apply();
            }
        />
        <label class="flow-cfg-label">"Method"</label>
        <select
            class="flow-cfg-input"
            prop:value=move || method_sig.get()
            on:change=move |ev| {
                method_sig.set(event_target_value(&ev));
                apply();
            }
        >
            <option value="post">"POST"</option>
            <option value="put">"PUT"</option>
            <option value="patch">"PATCH"</option>
        </select>
        <label class="flow-cfg-label">"Payload (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"summary":"{{ inputs.summarize.summary }}"}"#
            prop:value=move || payload_sig.get()
            on:input=move |ev| {
                payload_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <Show when=move || payload_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || payload_err.get().unwrap_or_default()}</div>
        </Show>
        <label class="flow-cfg-label">"Headers (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"Authorization":"Bearer …"}"#
            prop:value=move || headers_sig.get()
            on:input=move |ev| {
                headers_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <Show when=move || headers_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || headers_err.get().unwrap_or_default()}</div>
        </Show>
        <div class="flow-cfg-hint">
            "Sends the payload as application/json; a non-2xx response fails the step. Values may be {{ path }} templates."
        </div>
    }
}

/// The raw-JSON params editor for a non-agent action: a `{..params}` textarea
/// merged with the node's current `kind`.
fn params_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let action = node_action(graph, &id);
    let kind = action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("summarize")
        .to_string();
    let params = {
        let mut m = action.as_object().cloned().unwrap_or_default();
        m.remove("kind");
        if m.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default()
        }
    };
    let params_sig = RwSignal::new(params);
    let parse_err = RwSignal::new(Option::<String>::None);
    let id_for_params = id.clone();

    view! {
        <label class="flow-cfg-label">"Params (JSON object, optional)"</label>
        <textarea
            class="flow-cfg-area"
            placeholder=r#"{"channel":"ops","message":"hi"}"#
            prop:value=move || params_sig.get()
            on:input=move |ev| {
                params_sig.set(event_target_value(&ev));
                match parse_params(&params_sig.get_untracked()) {
                    Ok(mut obj) => {
                        obj.insert("kind".to_string(), Value::String(kind.clone()));
                        parse_err.set(None);
                        set_node_action(graph, &id_for_params, Value::Object(obj));
                    }
                    Err(e) => parse_err.set(Some(e)),
                }
            }
        ></textarea>
        <Show when=move || parse_err.with(Option::is_some) fallback=|| ().into_view()>
            <div class="flow-cfg-err">{move || parse_err.get().unwrap_or_default()}</div>
        </Show>
    }
}

/// Typed config for an **Agent** (`llm_agent`) node — a tool-calling LLM agent
/// (e.g. a channel chatbot). A system prompt, an optional model, a grouped tool
/// picker, and skills, all written into the action's `llm_agent` params. Uses the
/// **normal agent infrastructure**: empty `tools` = the agent may use any tool;
/// include `notify` so it can reply on a channel; `recall`/`remember` give it
/// memory.
fn agent_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let (system, model, tools, skills, output, reasoning) =
        agent_form_from_params(&node_action(graph, &id));
    let system_sig = RwSignal::new(system);
    let model_sig = RwSignal::new(model);
    let tools_sig = RwSignal::new(tools);
    let skills_sig = RwSignal::new(skills);
    let output_sig = RwSignal::new(output);
    let reasoning_sig = RwSignal::new(reasoning);
    let id_sv = StoredValue::new(id);

    // The gateway's chat models feed the model autocomplete (best-effort — an empty
    // catalog just leaves the field as free text accepting a typed id).
    let flow_models = RwSignal::new(Vec::<ModelInfo>::new());
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(list) = rest::list_llm_models(token.as_deref(), "llm").await {
            flow_models.set(list);
        }
    });

    // Push the current form state into the node's `llm_agent` action.
    let apply = move || {
        let v = agent_params_from_form(
            &system_sig.get_untracked(),
            &model_sig.get_untracked(),
            &tools_sig.get_untracked(),
            &skills_sig.get_untracked(),
            &output_sig.get_untracked(),
            &reasoning_sig.get_untracked(),
        );
        set_node_action(graph, &id_sv.get_value(), v);
    };

    view! {
        <label class="flow-cfg-label">"System prompt"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            placeholder="You are a helpful assistant in a chat channel…"
            prop:value=move || system_sig.get()
            on:input=move |ev| {
                system_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>

        <label class="flow-cfg-label">"Output format"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                output_sig.set(event_target_value(&ev));
                apply();
            }
            prop:value=move || output_sig.get()
        >
            <option value="text">"Text"</option>
            <option value="json">"JSON (structured — usable by downstream nodes)"</option>
        </select>

        <label class="flow-cfg-label">"Model (optional)"</label>
        {model_autocomplete(
            Signal::derive(move || model_sig.get()),
            move |v| {
                model_sig.set(v);
                apply();
            },
            model_options(flow_models, false),
            Signal::derive(|| "gateway default".to_string()),
            Signal::derive(|| false),
            "flow-cfg-input",
        )}

        <label class="flow-cfg-label">"Thinking (reasoning effort)"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                reasoning_sig.set(event_target_value(&ev));
                apply();
            }
            prop:value=move || reasoning_sig.get()
        >
            <option value="">"Off (model default)"</option>
            <option value="low">"Low"</option>
            <option value="medium">"Medium"</option>
            <option value="high">"High"</option>
            <option value="xhigh">"Extra high"</option>
            <option value="max">"Max"</option>
        </select>

        <label class="flow-cfg-label">"Tools the agent may call"</label>
        <div class="flow-tools">
            {KNOWN_TOOLS
                .iter()
                .map(|(group, names)| {
                    let checks = names
                        .iter()
                        .map(|name| {
                            let name = *name;
                            view! {
                                <label class="flow-tool">
                                    <input
                                        type="checkbox"
                                        prop:checked=move || {
                                            tools_sig.with(|sel| sel.iter().any(|x| x.as_str() == name))
                                        }
                                        on:change=move |_| {
                                            tools_sig
                                                .update(|sel| {
                                                    if let Some(i)
                                                        = sel.iter().position(|x| x.as_str() == name)
                                                    {
                                                        sel.remove(i);
                                                    } else {
                                                        sel.push(name.to_string());
                                                    }
                                                });
                                            apply();
                                        }
                                    />
                                    {name}
                                </label>
                            }
                        })
                        .collect::<Vec<_>>();
                    view! {
                        <div class="flow-tool-group">
                            <span class="flow-tool-grp">{*group}</span>
                            {checks}
                        </div>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
        <div class="flow-cfg-hint">
            "No tools ticked = the agent may use any available tool. On a channel-message trigger its reply is sent back to that channel automatically; `recall`/`remember` give it memory. Add `notify` only to also post to other channels."
        </div>

        <label class="flow-cfg-label">"Skills (comma-separated, optional)"</label>
        <input
            class="flow-cfg-input"
            placeholder="triage-inbox, …"
            prop:value=move || skills_sig.get()
            on:input=move |ev| {
                skills_sig.set(event_target_value(&ev));
                apply();
            }
        />
    }
}

/// The selected node's current action payload (`Value::Null` if it isn't an action
/// node). Read untracked — the config editors seed their form state from it once.
fn node_action(graph: RwSignal<FlowGraph>, id: &str) -> Value {
    graph
        .with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| match &n.kind {
                    FlowKind::Action { action } => Some(action.clone()),
                    _ => None,
                })
        })
        .unwrap_or(Value::Null)
}

/// Read an `llm_agent` action into the agent form's fields: `(system, model,
/// tools, skills_csv, output, reasoning)`. Missing fields default to empty; `output`
/// defaults to `"text"` and `reasoning` to `""` (off). Pure + testable.
fn agent_form_from_params(action: &Value) -> (String, String, Vec<String>, String, String, String) {
    let str_field = |k: &str| {
        action
            .get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let arr_field = |k: &str| {
        action
            .get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let output = match action.get("output").and_then(Value::as_str) {
        Some("json") => "json",
        _ => "text",
    }
    .to_string();
    (
        str_field("system"),
        str_field("model"),
        arr_field("tools"),
        arr_field("skills").join(", "),
        output,
        str_field("reasoning_effort"),
    )
}

/// Build an `llm_agent` action from the agent form fields, omitting blank/empty
/// ones (an empty `tools` array means "any tool"; `output` is set only for `"json"`,
/// since absent means plain text). Pure + testable.
fn agent_params_from_form(
    system: &str,
    model: &str,
    tools: &[String],
    skills_csv: &str,
    output: &str,
    reasoning: &str,
) -> Value {
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("llm_agent".to_string()));
    if !system.trim().is_empty() {
        obj.insert(
            "system".to_string(),
            Value::String(system.trim().to_string()),
        );
    }
    if !model.trim().is_empty() {
        obj.insert("model".to_string(), Value::String(model.trim().to_string()));
    }
    if !reasoning.trim().is_empty() {
        obj.insert(
            "reasoning_effort".to_string(),
            Value::String(reasoning.trim().to_string()),
        );
    }
    if !tools.is_empty() {
        obj.insert(
            "tools".to_string(),
            Value::Array(tools.iter().map(|t| Value::String(t.clone())).collect()),
        );
    }
    let skills: Vec<Value> = skills_csv
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect();
    if !skills.is_empty() {
        obj.insert("skills".to_string(), Value::Array(skills));
    }
    if output == "json" {
        obj.insert("output".to_string(), Value::String("json".to_string()));
    }
    Value::Object(obj)
}

/// Split a newline-separated outcomes textarea into trimmed, non-empty outcome
/// names (the order is preserved). Pure + testable.
fn parse_outcomes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The system prompt a [`classifier_action`] carries: it instructs the model to
/// score each named outcome from 0.0–1.0 and return them as a JSON object. The
/// `instructions` (optional) describe *what* to classify.
fn classifier_system_prompt(outcomes: &[String], instructions: &str) -> String {
    let mut s = String::from(
        "You are a classifier. Read the input and judge how strongly it matches each of the outcomes below.",
    );
    if !instructions.trim().is_empty() {
        s.push_str("\n\nClassification task: ");
        s.push_str(instructions.trim());
    }
    s.push_str("\n\nOutcomes:\n");
    if outcomes.is_empty() {
        s.push_str("- (none defined yet)\n");
    } else {
        for o in outcomes {
            s.push_str("- ");
            s.push_str(o);
            s.push('\n');
        }
    }
    s.push_str(
        "\nReturn a single JSON object whose keys are EXACTLY the outcome names listed above and whose values are probabilities from 0.0 to 1.0. The scores are independent — they need not sum to 1. Output ONLY the JSON object, with no prose, explanation, or markdown fences.",
    );
    s
}

/// Build a **Classifier** action: an `llm_agent` steered to JSON output whose
/// system prompt scores each `outcome` from 0.0–1.0. The `outcomes` list (and the
/// raw `instructions`) are kept as web-only metadata so the node round-trips for
/// editing; the backend reads only the generated `system` + `output: "json"`.
/// Pure + testable.
fn classifier_action(outcomes: &[String], instructions: &str) -> Value {
    let clean: Vec<String> = outcomes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut obj = Map::new();
    obj.insert("kind".to_string(), Value::String("llm_agent".to_string()));
    obj.insert(
        "system".to_string(),
        Value::String(classifier_system_prompt(&clean, instructions)),
    );
    obj.insert("output".to_string(), Value::String("json".to_string()));
    obj.insert(
        "outcomes".to_string(),
        Value::Array(clean.into_iter().map(Value::String).collect()),
    );
    if !instructions.trim().is_empty() {
        obj.insert(
            "instructions".to_string(),
            Value::String(instructions.trim().to_string()),
        );
    }
    Value::Object(obj)
}

/// Read a classifier action back into its editable fields: `(outcomes, instructions)`.
/// Pure + testable.
fn classifier_form_from_params(action: &Value) -> (Vec<String>, String) {
    let outcomes = action
        .get("outcomes")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let instructions = action
        .get("instructions")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (outcomes, instructions)
}

/// Typed config for a **Classifier** node: a free-text "what to classify" plus a
/// newline-separated list of outcomes. Every edit regenerates the underlying
/// JSON-steered `llm_agent` action (with a freshly-built scoring system prompt).
fn classifier_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let (outcomes, instructions) = classifier_form_from_params(&node_action(graph, &id));
    let instr_sig = RwSignal::new(instructions);
    let outcomes_sig = RwSignal::new(outcomes.join("\n"));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        let outs = parse_outcomes(&outcomes_sig.get_untracked());
        let v = classifier_action(&outs, &instr_sig.get_untracked());
        set_node_action(graph, &id_sv.get_value(), v);
    };

    view! {
        <label class="flow-cfg-label">"What to classify"</label>
        <textarea
            class="flow-cfg-area"
            placeholder="e.g. Judge the sentiment / intent of the incoming message…"
            prop:value=move || instr_sig.get()
            on:input=move |ev| {
                instr_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>

        <label class="flow-cfg-label">"Outcomes (one per line)"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            placeholder="positive&#10;negative&#10;neutral"
            prop:value=move || outcomes_sig.get()
            on:input=move |ev| {
                outcomes_sig.set(event_target_value(&ev));
                apply();
            }
        ></textarea>
        <div class="flow-cfg-hint">
            "The agent returns a JSON object scoring each outcome 0.0–1.0. Downstream nodes read it from this node's `data` output — e.g. a Condition checking `input.inputs[<id>].data.positive > 0.5`."
        </div>
    }
}

/// Code / condition node config: a runtime `<select>` (js/shell/python) + a `source`
/// textarea. A condition's source is evaluated for truthiness to route its branch
/// ports.
fn code_config(
    graph: RwSignal<FlowGraph>,
    id: String,
    runtime: String,
    source: String,
    is_condition: bool,
) -> impl IntoView {
    let runtime_sig = RwSignal::new(runtime);
    let source_sig = RwSignal::new(source);

    let id_for_rt = id.clone();
    let id_for_src = id.clone();

    let hint = if is_condition {
        "Condition source — its result routes the true / false ports."
    } else {
        "Code source — a pure data transform over `input`."
    };

    view! {
        <label class="flow-cfg-label">"Runtime"</label>
        <select
            class="flow-cfg-input"
            on:change=move |ev| {
                runtime_sig.set(event_target_value(&ev));
                set_node_code(graph, &id_for_rt, runtime_sig.get_untracked(), source_sig.get_untracked(), is_condition);
            }
            prop:value=move || runtime_sig.get()
        >
            {RUNTIMES
                .iter()
                .map(|(k, label)| {
                    view! { <option value=*k>{*label}</option> }
                })
                .collect::<Vec<_>>()}
        </select>
        <label class="flow-cfg-label">"Source"</label>
        <textarea
            class="flow-cfg-area flow-cfg-area-tall"
            prop:value=move || source_sig.get()
            on:input=move |ev| {
                source_sig.set(event_target_value(&ev));
                set_node_code(graph, &id_for_src, runtime_sig.get_untracked(), source_sig.get_untracked(), is_condition);
            }
        ></textarea>
        <div class="flow-cfg-hint">{hint}</div>
    }
}

// --- node-mutation helpers used by the config editors ---

/// Replace the selected trigger node's `trigger` payload.
fn set_node_trigger(graph: RwSignal<FlowGraph>, id: &str, trigger: Value) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                *t = trigger;
            }
        }
    });
}

/// The selected trigger node's current payload (`Value::Null` if it isn't a trigger
/// node). Untracked — the config fields seed from it once.
fn node_trigger(graph: RwSignal<FlowGraph>, id: &str) -> Value {
    graph
        .with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .and_then(|n| match &n.kind {
                    FlowKind::Trigger { trigger } => Some(trigger.clone()),
                    _ => None,
                })
        })
        .unwrap_or(Value::Null)
}

/// Merge a single typed field into the selected trigger node's payload, preserving
/// every other key (including opaque predicates the typed form doesn't model).
fn set_trigger_field(graph: RwSignal<FlowGraph>, id: &str, key: &str, value: &str) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                *t = merge_field(std::mem::take(t), key, value);
            }
        }
    });
}

/// Merge the collect trigger's `every` cadence field: a typed value (a bare-integer
/// minutes number, a `{"seconds":N}`-style object, or a duration string like `5m`) via
/// [`every_value`] when non-blank, removed when blank — so `every`'s non-string shapes
/// survive (unlike [`set_trigger_field`], which only writes strings). Persists exactly
/// what the user typed; the server clamps `[60s, 1 year]` at scan time.
fn set_trigger_every(graph: RwSignal<FlowGraph>, id: &str, raw: &str) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Trigger { trigger: t } = &mut n.kind {
                if let Some(obj) = t.as_object_mut() {
                    match every_value(raw) {
                        Some(v) => {
                            obj.insert("every".to_string(), v);
                        }
                        None => {
                            obj.remove("every");
                        }
                    }
                }
            }
        }
    });
}

/// Set (or, when blank, remove) one `key` on a trigger object, keeping all other
/// keys. A [string-list field](is_string_list_field) (e.g. `extensions`) is written
/// as a JSON array split from the comma-separated `value`; every other field is a
/// plain string. Non-objects pass through unchanged. Pure + testable.
fn merge_field(mut trigger: Value, key: &str, value: &str) -> Value {
    if let Some(obj) = trigger.as_object_mut() {
        let v = value.trim();
        if is_string_list_field(key) {
            let items: Vec<Value> = split_list(v).into_iter().map(Value::String).collect();
            if items.is_empty() {
                obj.remove(key);
            } else {
                obj.insert(key.to_string(), Value::Array(items));
            }
        } else if v.is_empty() {
            obj.remove(key);
        } else {
            obj.insert(key.to_string(), Value::String(v.to_string()));
        }
    }
    trigger
}

/// Replace the selected action node's `action` payload.
fn set_node_action(graph: RwSignal<FlowGraph>, id: &str, action: Value) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::Action { action: a } = &mut n.kind {
                *a = action;
            }
        }
    });
}

/// Config panel for a **For each** loop head (SOUL §11): the array `source`
/// path, the `item` loop variable, and the optional `index` / `max_iterations`.
/// Every input writes straight onto the node (the [`set_for_each_field`]
/// pattern), like the other typed configs.
fn for_each_config(graph: RwSignal<FlowGraph>, id: String) -> impl IntoView {
    let read = |g: RwSignal<FlowGraph>, id: &str, pick: fn(&FlowKind) -> String| {
        g.with_untracked(|g| {
            g.nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| pick(&n.kind))
                .unwrap_or_default()
        })
    };
    let source_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { source, .. } => source.clone(),
        _ => String::new(),
    }));
    let item_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { item, .. } => item.clone(),
        _ => String::new(),
    }));
    let index_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { index, .. } => index.clone().unwrap_or_default(),
        _ => String::new(),
    }));
    let max_sig = RwSignal::new(read(graph, &id, |k| match k {
        FlowKind::ForEach { max_iterations, .. } => {
            max_iterations.map(|n| n.to_string()).unwrap_or_default()
        }
        _ => String::new(),
    }));
    let id_sv = StoredValue::new(id);
    let apply = move || {
        set_for_each_fields(
            graph,
            &id_sv.get_value(),
            source_sig.get_untracked(),
            item_sig.get_untracked(),
            index_sig.get_untracked(),
            max_sig.get_untracked(),
        );
    };
    let field = move |label: &'static str,
                      placeholder: &'static str,
                      sig: RwSignal<String>,
                      hint: &'static str| {
        view! {
            <div class="flow-cfg-field">
                <label class="flow-cfg-label">{label}</label>
                <input
                    class="flow-cfg-input"
                    placeholder=placeholder
                    prop:value=move || sig.get()
                    on:input=move |ev| {
                        sig.set(event_target_value(&ev));
                        apply();
                    }
                />
                <div class="flow-cfg-hint">{hint}</div>
            </div>
        }
    };
    view! {
        {field(
            "Array source",
            "inputs.web_search.searches.rust.results",
            source_sig,
            "A path into the input envelope that yields an array — one iteration per element.",
        )}
        {field(
            "Item variable",
            "item",
            item_sig,
            "The loop variable the body references, e.g. {{ item.title }}.",
        )}
        {field(
            "Index variable (optional)",
            "i",
            index_sig,
            "Bound to the 0-based position when set.",
        )}
        {field(
            "Max iterations (optional)",
            "1000",
            max_sig,
            "Caps the run (hard ceiling 1000). Blank = the hard ceiling.",
        )}
        <div class="flow-cfg-hint">
            "Wire: For each → body nodes → End loop. The body runs once per element; \
             nodes after the End loop read the collected results array."
        </div>
    }
}

/// Config panel for an **End loop** node: pick which For-each head it closes.
/// The picker lists the graph's For-each node ids so the pairing can't typo.
fn loop_end_config(graph: RwSignal<FlowGraph>, id: String, for_each: String) -> impl IntoView {
    let sig = RwSignal::new(for_each);
    let id_sv = StoredValue::new(id);
    let heads = move || {
        graph.with(|g| {
            g.nodes
                .iter()
                .filter(|n| matches!(n.kind, FlowKind::ForEach { .. }))
                .map(|n| n.id.clone())
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="flow-cfg-field">
            <label class="flow-cfg-label">"Closes the For-each"</label>
            <select
                class="flow-cfg-input"
                prop:value=move || sig.get()
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    sig.set(v.clone());
                    graph
                        .update(|g| {
                            if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id_sv.get_value()) {
                                if let FlowKind::LoopEnd { for_each } = &mut n.kind {
                                    *for_each = v.clone();
                                }
                            }
                        });
                }
            >
                <option value="">"— pick the loop head —"</option>
                {move || {
                    heads()
                        .into_iter()
                        .map(|h| {
                            let sel = {
                                let h = h.clone();
                                move || sig.get() == h
                            };
                            view! { <option value=h.clone() selected=sel>{h.clone()}</option> }
                        })
                        .collect::<Vec<_>>()
                }}
            </select>
            <Show when=move || heads().is_empty() fallback=|| ().into_view()>
                <div class="flow-cfg-warn">
                    "No For-each node in this graph yet — add one for this to close."
                </div>
            </Show>
            <div class="flow-cfg-hint">
                "Downstream nodes read this node's output: the array of per-iteration results."
            </div>
        </div>
    }
}

/// Overwrite a For-each node's fields in place (no-op for other kinds). Blank
/// `index` clears it; `max_iterations` keeps only a parseable positive number.
fn set_for_each_fields(
    graph: RwSignal<FlowGraph>,
    id: &str,
    source: String,
    item: String,
    index: String,
    max_iterations: String,
) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            if let FlowKind::ForEach {
                source: s,
                item: it,
                index: ix,
                max_iterations: mx,
            } = &mut n.kind
            {
                *s = source;
                *it = item;
                let index = index.trim();
                *ix = (!index.is_empty()).then(|| index.to_string());
                *mx = max_iterations
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0);
            }
        }
    });
}

/// Replace the selected code/condition node's `runtime` + `source`.
fn set_node_code(
    graph: RwSignal<FlowGraph>,
    id: &str,
    runtime: String,
    source: String,
    is_condition: bool,
) {
    graph.update(|g| {
        if let Some(n) = g.nodes.iter_mut().find(|n| n.id == id) {
            match &mut n.kind {
                FlowKind::Code {
                    runtime: r,
                    source: s,
                }
                | FlowKind::Condition {
                    runtime: r,
                    source: s,
                } => {
                    let _ = is_condition;
                    *r = runtime;
                    *s = source;
                }
                _ => {}
            }
        }
    });
}

/// Parse the action-params textarea into a JSON object. Empty → an empty object;
/// a non-object or malformed JSON is an error.
fn parse_params(input: &str) -> Result<Map<String, Value>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    match v {
        Value::Object(m) => Ok(m),
        _ => Err("expected a JSON object".to_string()),
    }
}

// --- defaults + menu data ---

/// The default trigger payload a fresh trigger node carries (a manual webhook).
fn default_trigger() -> Value {
    serde_json::json!({ "kind": "webhook", "path": "/hook" })
}

/// The default action payload a fresh action node carries.
fn default_action() -> Value {
    serde_json::json!({ "kind": "summarize" })
}

/// The default payload a fresh **Agent** node carries: a tool-calling `llm_agent`
/// seeded with chatbot-friendly guidance + the tools a channel bot wants (reply +
/// memory). Authored over the existing agent infrastructure.
fn default_agent_action() -> Value {
    serde_json::json!({
        "kind": "llm_agent",
        "system": "You are a helpful assistant in a chat channel. Read the incoming message, use your tools when useful, and write a concise reply. Your response is delivered back to the user on their channel automatically.",
        "tools": ["recall", "remember"]
    })
}

/// The default payload a fresh **Classifier** node carries: a JSON-steered
/// `llm_agent` that scores three common sentiment buckets — edit the outcomes to
/// fit the task.
fn default_classifier_action() -> Value {
    classifier_action(
        &[
            "positive".to_string(),
            "negative".to_string(),
            "neutral".to_string(),
        ],
        "Classify the incoming message.",
    )
}

/// A node in a starter [`chatbot_template`]/[`scheduled_template`]: `(id-prefix,
/// kind, dx, dy)` — `dx`/`dy` lay it out relative to the insertion point.
type TemplateNode = (&'static str, FlowKind, f64, f64);

/// The **Channel chatbot** starter: a `channel_message` trigger wired into an Agent
/// node — drop it in, set the channel, and you have a tool-calling bot that replies
/// on the channel.
fn chatbot_template() -> (Vec<TemplateNode>, Vec<(usize, usize, &'static str)>) {
    (
        vec![
            (
                "chan",
                FlowKind::Trigger {
                    trigger: serde_json::json!({ "kind": "channel_message", "channel": "general" }),
                },
                0.0,
                0.0,
            ),
            (
                "agent",
                FlowKind::Action {
                    action: default_agent_action(),
                },
                260.0,
                0.0,
            ),
        ],
        vec![(0, 1, "")],
    )
}

/// The **Scheduled assistant** starter: a daily `schedule` trigger wired into an
/// Agent node that runs a configured task each morning.
fn scheduled_template() -> (Vec<TemplateNode>, Vec<(usize, usize, &'static str)>) {
    (
        vec![
            (
                "sched",
                FlowKind::Trigger {
                    trigger: serde_json::json!({ "kind": "schedule", "cron": "0 9 * * *" }),
                },
                0.0,
                0.0,
            ),
            (
                "agent",
                FlowKind::Action {
                    action: serde_json::json!({
                        "kind": "llm_agent",
                        "system": "Each morning, review the workspace and carry out your configured task using your tools.",
                        "tools": ["recall", "remember", "query_structured"]
                    }),
                },
                260.0,
                0.0,
            ),
        ],
        vec![(0, 1, "")],
    )
}

/// Instantiate a template into `g`: assign each node a fresh id (via [`fresh_id`],
/// from `counter`), lay them out from a staggered base, and wire the edges by node
/// index. Returns the new graph, the advanced counter, and the new node ids (for
/// selection). Pure + testable.
fn instantiate_template(
    mut g: FlowGraph,
    nodes: Vec<TemplateNode>,
    edges: Vec<(usize, usize, &str)>,
    mut counter: u64,
) -> (FlowGraph, u64, Vec<String>) {
    // Stagger successive inserts so a template dropped onto a non-empty canvas
    // doesn't land exactly on existing nodes.
    let base_y = 90.0 + (counter % 8) as f64 * 18.0;
    let mut ids = Vec::with_capacity(nodes.len());
    for (prefix, kind, dx, dy) in nodes {
        let (id, next) = fresh_id(&g, prefix, counter);
        counter = next;
        g = add_node(
            g,
            FlowNode {
                id: id.clone(),
                kind,
                position: FlowPos {
                    x: snap(90.0 + dx),
                    y: snap(base_y + dy),
                },
            },
        );
        ids.push(id);
    }
    for (from, to, port) in edges {
        if let (Some(f), Some(t)) = (ids.get(from), ids.get(to)) {
            g = add_edge(
                g,
                FlowEdge {
                    from: f.clone(),
                    to: t.clone(),
                    from_port: port.to_string(),
                    to_port: String::new(),
                },
            );
        }
    }
    (g, counter, ids)
}

/// Clone the node `id` into `g` with a fresh id (same `kind`, offset + snapped
/// position; edges are NOT copied). Returns the new graph, the advanced counter, and
/// the new id (`None` if `id` wasn't found). Pure + testable.
fn duplicate_node(g: FlowGraph, id: &str, counter: u64) -> (FlowGraph, u64, Option<String>) {
    let Some(orig) = g.nodes.iter().find(|n| n.id == id).cloned() else {
        return (g, counter, None);
    };
    let (prefix, _, _) = kind_meta(&orig.kind);
    let (new_id, next) = fresh_id(&g, prefix, counter);
    let node = FlowNode {
        id: new_id.clone(),
        kind: orig.kind.clone(),
        position: FlowPos {
            x: snap(orig.position.x + 24.0),
            y: snap(orig.position.y + 24.0),
        },
    };
    (add_node(g, node), next, Some(new_id))
}

/// The tool names an [`agent_config`] node may grant its agent, grouped for the
/// picker. Mirrors the registered tools in `catalerum-api`'s registry (some are
/// only live when their backend is configured, e.g. `notify` needs `[channels]`,
/// `run_command` needs `[exec]`, `search_*`/`query_graph` need their stores).
const KNOWN_TOOLS: &[(&str, &[&str])] = &[
    ("Reply / comms", &["notify"]),
    ("Memory", &["recall", "remember", "update_memory", "forget"]),
    (
        "Notes",
        &[
            "create_note",
            "edit_note",
            "delete_note",
            "read_note",
            "list_notes",
        ],
    ),
    (
        "Tasks",
        &[
            "kanban_create_task",
            "kanban_edit_task",
            "kanban_move_task",
            "kanban_complete_task",
            "kanban_set_task_status",
            "kanban_delete_task",
            "kanban_next_task",
            "kanban_read_task",
        ],
    ),
    (
        "Calendar",
        &[
            "create_calendar",
            "create_event",
            "update_event",
            "delete_event",
            "read_event",
        ],
    ),
    (
        "Query / search",
        &[
            "query_structured",
            "search_semantic",
            "search_files",
            "search_messages",
            "search_events",
            "kanban_search_tasks",
            "read_conversation",
            "read_object",
            "query_graph",
            "get_emails",
            "search_emails",
            "read_email",
        ],
    ),
    (
        "Web / exec",
        &[
            "fetch_url",
            "web_search",
            "html_to_markdown",
            "extract_html",
            "run_command",
        ],
    ),
    ("Skills", &["use_skill", "list_skills"]),
    ("Profile", &["update_profile"]),
    ("Time", &["current_time"]),
];

/// Trigger kinds offered in the config `<select>`, as `(wire kind, label)`. Mirrors
/// `catalerum_automation::Trigger`.
const TRIGGER_KINDS: &[(&str, &str)] = &[
    ("task_moved", "Task moved"),
    ("webhook", "Webhook"),
    ("channel_message", "Channel message"),
    ("collect_email", "Collect email"),
    ("collect_calendar", "Collect calendar"),
    ("collect_sql", "Collect DB rows"),
    ("storage_object", "Storage object"),
    ("schedule", "Schedule (cron)"),
    ("graph_query", "Graph query"),
    ("calendar_event", "Calendar event"),
];

/// Action kinds offered in the config `<select>`, as `(wire kind, label)`. Mirrors
/// `catalerum_automation::ActionKind`.
const ACTION_KINDS: &[(&str, &str)] = &[
    ("summarize", "Summarize"),
    ("create_chat_thread", "Create chat thread"),
    ("notify", "Notify"),
    ("llm_agent", "LLM agent"),
    ("create_note", "Create note"),
    ("edit_note", "Edit note"),
    ("create_task", "Create task"),
    ("move_task", "Move task"),
    ("create_event", "Create event"),
    ("update_event", "Update event"),
    ("write_email", "Write email (collected)"),
    ("write_event", "Write event (collected)"),
    ("label_email", "Label email"),
    ("mark_email_read", "Mark email read"),
    ("move_object", "Move object"),
    ("write_object", "Write object"),
    ("run_command", "Run command"),
    ("webhook", "Webhook"),
];

/// Code/condition runtimes offered in the config `<select>`.
const RUNTIMES: &[(&str, &str)] = &[
    ("js", "JavaScript"),
    ("shell", "Shell"),
    ("python", "Python"),
];

// ---------------------------------------------------------------------------
// Tests — the pure core.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_graph() -> FlowGraph {
        FlowGraph {
            nodes: vec![
                FlowNode {
                    id: "t1".into(),
                    kind: FlowKind::Trigger {
                        trigger: json!({ "kind": "webhook", "path": "/h" }),
                    },
                    position: FlowPos { x: 10.0, y: 20.0 },
                },
                FlowNode {
                    id: "a1".into(),
                    kind: FlowKind::Action {
                        action: json!({ "kind": "summarize" }),
                    },
                    position: FlowPos { x: 200.0, y: 20.0 },
                },
            ],
            edges: vec![FlowEdge {
                from: "t1".into(),
                to: "a1".into(),
                from_port: String::new(),
                to_port: String::new(),
            }],
        }
    }

    #[test]
    fn serializes_to_the_backend_node_shape() {
        // The flattened `kind` tag + payload + position, exactly as graph.rs parses.
        let g = sample_graph();
        let v = serde_json::to_value(&g).unwrap();
        let n0 = &v["nodes"][0];
        assert_eq!(n0["id"], json!("t1"));
        assert_eq!(n0["kind"], json!("trigger"));
        assert_eq!(n0["trigger"]["kind"], json!("webhook"));
        assert_eq!(n0["position"]["x"], json!(10.0));
        let n1 = &v["nodes"][1];
        assert_eq!(n1["kind"], json!("action"));
        assert_eq!(n1["action"]["kind"], json!("summarize"));
        let e = &v["edges"][0];
        assert_eq!(e["from"], json!("t1"));
        assert_eq!(e["to"], json!("a1"));
        assert_eq!(e["from_port"], json!(""));
    }

    #[test]
    fn deserializes_a_backend_graph_with_defaults() {
        // No position / no ports → defaults, like the backend Graph.
        let g: FlowGraph = serde_json::from_value(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" }
            ],
            "edges": [ { "from": "t", "to": "c" } ]
        }))
        .unwrap();
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.nodes[0].position, FlowPos::default());
        assert_eq!(g.edges[0].from_port, "");
        match &g.nodes[1].kind {
            FlowKind::Condition { runtime, source } => {
                assert_eq!(runtime, "js");
                assert_eq!(source, "true");
            }
            other => panic!("expected condition, got {other:?}"),
        }
    }

    #[test]
    fn spec_round_trip() {
        let g = sample_graph();
        let spec = graph_to_spec_value(&g);
        assert!(spec["graph"]["nodes"].is_array());
        let back = flow_from_spec(&spec).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn flow_from_spec_tolerates_missing_or_legacy() {
        // No graph key / non-object / a malformed graph → None (start empty).
        assert!(flow_from_spec(&json!({ "other": 1 })).is_none());
        assert!(flow_from_spec(&json!("scalar")).is_none());
        assert!(flow_from_spec(&json!(null)).is_none());
        assert!(flow_from_spec(
            &json!({ "graph": { "nodes": [ { "id": "x", "kind": "nope" } ] } })
        )
        .is_none());
    }

    #[test]
    fn validate_accepts_a_linear_dag() {
        assert_eq!(validate_flow(&sample_graph()), Ok(()));
    }

    #[test]
    fn validate_rejects_duplicate_ids() {
        let mut g = sample_graph();
        g.nodes[1].id = "t1".into();
        g.edges.clear();
        assert_eq!(validate_flow(&g), Err("duplicate node id 't1'".to_string()));
    }

    #[test]
    fn validate_rejects_a_dangling_edge() {
        let mut g = sample_graph();
        g.edges[0].to = "ghost".into();
        assert_eq!(
            validate_flow(&g),
            Err("edge to unknown node 'ghost'".to_string())
        );
        let mut g2 = sample_graph();
        g2.edges[0].from = "ghost".into();
        assert_eq!(
            validate_flow(&g2),
            Err("edge from unknown node 'ghost'".to_string())
        );
    }

    #[test]
    fn validate_rejects_no_trigger() {
        let g = FlowGraph {
            nodes: vec![FlowNode {
                id: "a".into(),
                kind: FlowKind::Action {
                    action: json!({ "kind": "summarize" }),
                },
                position: FlowPos::default(),
            }],
            edges: vec![],
        };
        assert_eq!(
            validate_flow(&g),
            Err("graph has no trigger node".to_string())
        );
    }

    #[test]
    fn validate_rejects_a_cycle() {
        // t -> a -> b -> a : a cycle.
        let g = FlowGraph {
            nodes: vec![
                FlowNode {
                    id: "t".into(),
                    kind: FlowKind::Trigger {
                        trigger: json!({ "kind": "webhook", "path": "/h" }),
                    },
                    position: FlowPos::default(),
                },
                FlowNode {
                    id: "a".into(),
                    kind: FlowKind::Action {
                        action: json!({ "kind": "summarize" }),
                    },
                    position: FlowPos::default(),
                },
                FlowNode {
                    id: "b".into(),
                    kind: FlowKind::Action {
                        action: json!({ "kind": "notify" }),
                    },
                    position: FlowPos::default(),
                },
            ],
            edges: vec![
                FlowEdge {
                    from: "t".into(),
                    to: "a".into(),
                    from_port: String::new(),
                    to_port: String::new(),
                },
                FlowEdge {
                    from: "a".into(),
                    to: "b".into(),
                    from_port: String::new(),
                    to_port: String::new(),
                },
                FlowEdge {
                    from: "b".into(),
                    to: "a".into(),
                    from_port: String::new(),
                    to_port: String::new(),
                },
            ],
        };
        assert_eq!(validate_flow(&g), Err("graph has a cycle".to_string()));
    }

    #[test]
    fn validate_accepts_a_diamond() {
        // A DAG with a join (no cycle) passes.
        let mut g = sample_graph();
        g.nodes.push(FlowNode {
            id: "a2".into(),
            kind: FlowKind::Action {
                action: json!({ "kind": "notify" }),
            },
            position: FlowPos::default(),
        });
        g.nodes.push(FlowNode {
            id: "j".into(),
            kind: FlowKind::Action {
                action: json!({ "kind": "create_note" }),
            },
            position: FlowPos::default(),
        });
        g.edges.push(FlowEdge {
            from: "t1".into(),
            to: "a2".into(),
            from_port: String::new(),
            to_port: String::new(),
        });
        g.edges.push(FlowEdge {
            from: "a1".into(),
            to: "j".into(),
            from_port: String::new(),
            to_port: String::new(),
        });
        g.edges.push(FlowEdge {
            from: "a2".into(),
            to: "j".into(),
            from_port: String::new(),
            to_port: String::new(),
        });
        assert_eq!(validate_flow(&g), Ok(()));
    }

    #[test]
    fn edge_path_is_a_cubic_bezier_through_the_endpoints() {
        let d = edge_path((0.0, 0.0), (200.0, 100.0));
        // Starts with a move to `from`, has a cubic `C`, ends at `to`.
        assert!(d.starts_with("M 0.0 0.0 "), "starts at from: {d}");
        assert!(d.contains(" C "), "has a cubic segment: {d}");
        assert!(d.ends_with("200.0 100.0"), "ends at to: {d}");
        // The horizontal control reach is half the span (100), so the first handle
        // x is 100.
        assert!(
            d.contains("C 100.0 0.0 100.0 100.0"),
            "horizontal handles: {d}"
        );
    }

    #[test]
    fn edge_path_clamps_the_handle_reach_for_close_points() {
        // Two points 10 apart in x: the reach clamps to the 40 minimum.
        let d = edge_path((0.0, 0.0), (10.0, 0.0));
        assert!(d.contains("C 40.0 0.0 "), "clamped reach: {d}");
    }

    #[test]
    fn commit_edge_path_arrives_vertically_from_below() {
        let d = commit_edge_path((0.0, 0.0), (300.0, 40.0));
        assert!(d.starts_with("M 0.0 0.0 "), "starts at from: {d}");
        assert!(d.ends_with("300.0 40.0"), "ends at to: {d}");
        // The arrival handle shares the endpoint's x and sits below it (40 + the
        // 36 minimum depth), so the path enters the bottom anchor traveling
        // straight up and the arrowhead points into the node, not along it.
        assert!(
            d.contains(" 300.0 76.0 300.0 40.0"),
            "vertical arrival from below: {d}"
        );
        // The exit handle drops below the source port, so the gate dives under
        // the port's own label instead of striking it through.
        assert!(d.contains("C 150.0 26.0 "), "down-angled exit: {d}");
    }

    #[test]
    fn screen_to_canvas_inverts_origin_pan_and_zoom() {
        // client (150,120), rect origin (50,20), pan (30,10), zoom 1 → (70,90).
        assert_eq!(
            screen_to_canvas((150.0, 120.0), (50.0, 20.0), (30.0, 10.0), 1.0),
            (70.0, 90.0)
        );
        // No pan / origin, zoom 1 → identity.
        assert_eq!(
            screen_to_canvas((5.0, 6.0), (0.0, 0.0), (0.0, 0.0), 1.0),
            (5.0, 6.0)
        );
        // Zoom 2 halves the canvas-space delta (the inverse of `scale(2)`).
        assert_eq!(
            screen_to_canvas((150.0, 120.0), (50.0, 20.0), (30.0, 10.0), 2.0),
            (35.0, 45.0)
        );
    }

    #[test]
    fn zoom_to_cursor_keeps_the_point_under_the_pointer_fixed() {
        let cursor = (130.0, 90.0);
        let origin = (10.0, 5.0);
        let pan = (20.0, 8.0);
        let (z_old, z_new) = (1.0, 1.8);
        let before = screen_to_canvas(cursor, origin, pan, z_old);
        let pan_new = zoom_to_cursor(cursor, origin, pan, z_old, z_new);
        let after = screen_to_canvas(cursor, origin, pan_new, z_new);
        // The canvas point under the cursor is unchanged across the zoom.
        assert!(
            (before.0 - after.0).abs() < 1e-9,
            "x fixed: {before:?} {after:?}"
        );
        assert!(
            (before.1 - after.1).abs() < 1e-9,
            "y fixed: {before:?} {after:?}"
        );
    }

    #[test]
    fn fit_transform_centers_content_and_clamps_zoom() {
        assert!(fit_transform(&[], (800.0, 600.0)).is_none(), "empty → None");
        let n = FlowNode {
            id: "n".into(),
            kind: FlowKind::Action {
                action: json!({ "kind": "summarize" }),
            },
            position: FlowPos { x: 0.0, y: 0.0 },
        };
        let (pan, zoom) = fit_transform(std::slice::from_ref(&n), (800.0, 600.0)).unwrap();
        // The node's center maps to the viewport center (`pan + zoom*center`).
        let (cx, cy) = (NODE_W / 2.0, NODE_H / 2.0);
        assert!(((pan.0 + zoom * cx) - 400.0).abs() < 1e-6, "centered x");
        assert!(((pan.1 + zoom * cy) - 300.0).abs() < 1e-6, "centered y");
        assert!((0.4..=1.5).contains(&zoom), "zoom clamped: {zoom}");
    }

    #[test]
    fn fresh_id_is_deterministic_and_unique() {
        let g = sample_graph();
        let (id1, c1) = fresh_id(&g, "node", 1);
        assert_eq!(id1, "node1");
        assert_eq!(c1, 2);
        let (id2, c2) = fresh_id(&g, "node", c1);
        assert_eq!(id2, "node2");
        assert_eq!(c2, 3);
        // It skips an id that already exists in the graph.
        let mut g2 = sample_graph();
        g2.nodes[0].id = "node1".into();
        let (id, next) = fresh_id(&g2, "node", 1);
        assert_eq!(id, "node2");
        assert_eq!(next, 3);
    }

    #[test]
    fn add_and_remove_node() {
        let g = sample_graph();
        let g = add_node(
            g,
            FlowNode {
                id: "c1".into(),
                kind: FlowKind::Code {
                    runtime: "js".into(),
                    source: "input".into(),
                },
                position: FlowPos::default(),
            },
        );
        assert_eq!(g.nodes.len(), 3);
        // Removing a1 drops the t1->a1 edge too.
        let g = remove_node(g, "a1");
        assert_eq!(g.nodes.len(), 2);
        assert!(g.edges.is_empty());
        assert!(g.nodes.iter().any(|n| n.id == "c1"));
    }

    #[test]
    fn add_edge_rejects_self_loops_and_dups() {
        let g = FlowGraph {
            nodes: sample_graph().nodes,
            edges: vec![],
        };
        // A self-loop is a no-op.
        let g = add_edge(
            g,
            FlowEdge {
                from: "t1".into(),
                to: "t1".into(),
                from_port: String::new(),
                to_port: String::new(),
            },
        );
        assert!(g.edges.is_empty());
        // A real edge is added.
        let edge = FlowEdge {
            from: "t1".into(),
            to: "a1".into(),
            from_port: String::new(),
            to_port: String::new(),
        };
        let g = add_edge(g, edge.clone());
        assert_eq!(g.edges.len(), 1);
        // The same edge again is a no-op (dup).
        let g = add_edge(g, edge);
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn remove_edge_by_index() {
        let g = sample_graph();
        assert_eq!(g.edges.len(), 1);
        let g = remove_edge(g, 5); // out of range → no-op
        assert_eq!(g.edges.len(), 1);
        let g = remove_edge(g, 0);
        assert!(g.edges.is_empty());
    }

    #[test]
    fn out_ports_distinguish_condition_branches() {
        let cond = FlowKind::Condition {
            runtime: "js".into(),
            source: "x".into(),
        };
        assert_eq!(cond.out_ports(), &["true", "false"]);
        let act = FlowKind::Action {
            action: json!({ "kind": "notify" }),
        };
        assert_eq!(act.out_ports(), &[""]);
        // A collect source trigger exposes the data port + a `commit` gate port.
        let collect = FlowKind::Trigger {
            trigger: json!({ "kind": "collect_email", "connection": "c1" }),
        };
        assert_eq!(collect.out_ports(), &["", "commit"]);
        // A non-collect trigger has only the default port.
        let webhook = FlowKind::Trigger {
            trigger: json!({ "kind": "webhook", "path": "/h" }),
        };
        assert_eq!(webhook.out_ports(), &[""]);
    }

    fn collect_node(id: &str) -> FlowNode {
        FlowNode {
            id: id.into(),
            kind: FlowKind::Trigger {
                trigger: json!({ "kind": "collect_email", "connection": "conn1" }),
            },
            position: FlowPos::default(),
        }
    }
    fn write_node(id: &str) -> FlowNode {
        FlowNode {
            id: id.into(),
            kind: FlowKind::Action {
                action: json!({ "kind": "write_email" }),
            },
            position: FlowPos::default(),
        }
    }
    fn edge(from: &str, to: &str, from_port: &str) -> FlowEdge {
        FlowEdge {
            from: from.into(),
            to: to.into(),
            from_port: from_port.into(),
            to_port: String::new(),
        }
    }

    #[test]
    fn for_each_graph_round_trips_and_validates() {
        // The regression that motivated first-class loop nodes: a stored graph
        // with for_each/loop_end previously FAILED FlowGraph parsing entirely, so
        // the visual editor opened an empty canvas over a real graph.
        let spec = json!({
            "graph": {
                "nodes": [
                    { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" },
                      "position": { "x": 0.0, "y": 0.0 } },
                    { "id": "each", "kind": "for_each", "source": "inputs.t.items",
                      "item": "row", "index": "i", "position": { "x": 200.0, "y": 0.0 } },
                    { "id": "a", "kind": "action", "action": { "kind": "summarize" },
                      "position": { "x": 400.0, "y": 0.0 } },
                    { "id": "end", "kind": "loop_end", "for_each": "each",
                      "position": { "x": 600.0, "y": 0.0 } }
                ],
                "edges": [
                    { "from": "t", "to": "each" },
                    { "from": "each", "to": "a" },
                    { "from": "a", "to": "end" }
                ]
            }
        });
        let g = flow_from_spec(&spec).expect("a loop graph parses into the canvas");
        assert!(matches!(
            &g.nodes[1].kind,
            FlowKind::ForEach { source, item, index, max_iterations }
                if source == "inputs.t.items" && item == "row"
                    && index.as_deref() == Some("i") && max_iterations.is_none()
        ));
        assert!(matches!(&g.nodes[3].kind, FlowKind::LoopEnd { for_each } if for_each == "each"));
        assert!(validate_flow(&g).is_ok());
        // Round-trip: saving re-serializes the exact backend node shape.
        let back = graph_to_spec_value(&g);
        assert_eq!(back["graph"]["nodes"][1]["kind"], json!("for_each"));
        assert_eq!(back["graph"]["nodes"][1]["item"], json!("row"));
        assert_eq!(back["graph"]["nodes"][3]["kind"], json!("loop_end"));
        assert_eq!(back["graph"]["nodes"][3]["for_each"], json!("each"));
    }

    #[test]
    fn validate_flow_checks_loop_pairing() {
        let trigger = FlowNode {
            id: "t".into(),
            kind: FlowKind::Trigger {
                trigger: json!({ "kind": "webhook" }),
            },
            position: FlowPos::default(),
        };
        let each = FlowNode {
            id: "each".into(),
            kind: FlowKind::ForEach {
                source: "inputs.t.items".into(),
                item: "item".into(),
                index: None,
                max_iterations: None,
            },
            position: FlowPos::default(),
        };
        let end = |target: &str| FlowNode {
            id: "end".into(),
            kind: FlowKind::LoopEnd {
                for_each: target.into(),
            },
            position: FlowPos::default(),
        };
        // A dangling end-loop (no head picked / unknown head) fails.
        let g = FlowGraph {
            nodes: vec![trigger.clone(), end("")],
            edges: vec![],
        };
        assert!(validate_flow(&g).unwrap_err().contains("end-loop"));
        // A head with no closing end fails.
        let g = FlowGraph {
            nodes: vec![trigger.clone(), each.clone()],
            edges: vec![],
        };
        assert!(validate_flow(&g).unwrap_err().contains("for-each"));
        // A proper pair passes.
        let g = FlowGraph {
            nodes: vec![trigger, each, end("each")],
            edges: vec![],
        };
        assert!(validate_flow(&g).is_ok());
    }

    #[test]
    fn loop_node_search_hits_insert_typed_nodes() {
        // Clicking a for_each / loop_end search result used to silently insert
        // NOTHING (hit_to_flow_kind returned None for both).
        let hit = |node_kind: &str, example: Value| NodeTypeHit {
            id: format!("{node_kind}.x"),
            node_kind: node_kind.into(),
            kind: String::new(),
            title: "t".into(),
            summary: String::new(),
            description: String::new(),
            params: Vec::new(),
            example,
            score: 0.0,
        };
        let (kind, prefix) = hit_to_flow_kind(&hit(
            "for_each",
            json!({ "id": "e", "kind": "for_each", "source": "inputs.s.results", "item": "article" }),
        ))
        .expect("for_each inserts");
        assert_eq!(prefix, "each");
        assert!(matches!(
            kind,
            FlowKind::ForEach { source, item, .. }
                if source == "inputs.s.results" && item == "article"
        ));
        // loop_end inserts UNPAIRED (the example's head id won't exist here).
        let (kind, prefix) = hit_to_flow_kind(&hit(
            "loop_end",
            json!({ "id": "end", "kind": "loop_end", "for_each": "each_article" }),
        ))
        .expect("loop_end inserts");
        assert_eq!(prefix, "end_each");
        assert!(matches!(kind, FlowKind::LoopEnd { for_each } if for_each.is_empty()));
    }

    #[test]
    fn commit_edges_anchor_on_the_target_bottom_not_the_input_port() {
        // The data edge and the commit gate share endpoints (trigger → write); if
        // both landed on the input port they'd overlay into one unreadable
        // double-arrow. The gate lands on the target's bottom edge instead.
        let g = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w")],
            edges: vec![edge("c", "w", ""), edge("c", "w", "commit")],
        };
        let data = edge_endpoints(&g, &g.edges[0]).unwrap();
        let gate = edge_endpoints(&g, &g.edges[1]).unwrap();
        assert_ne!(data.1, gate.1, "distinct landing anchors");
        let w = &g.nodes[1];
        assert_eq!(
            gate.1 .1,
            w.position.y + NODE_H,
            "gate taps the bottom edge"
        );
        // And the two edges leave from distinct out-ports on the trigger.
        assert_ne!(data.0, gate.0);
    }

    #[test]
    fn wire_edge_normalizes_reverse_commit_gesture() {
        // Dragging write output → collect trigger reads as "commit on this write":
        // it lands as the canonical trigger-commit-port edge, never write→trigger.
        let g = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w")],
            edges: vec![edge("c", "w", "")],
        };
        let (g, msg) = wire_edge(g, edge("w", "c", ""));
        assert!(msg.is_none());
        assert_eq!(g.edges.len(), 2);
        assert!(
            g.edges
                .iter()
                .any(|e| e.from == "c" && e.to == "w" && e.from_port == COMMIT_PORT),
            "reverse gesture became the commit gate, got {:?}",
            g.edges
        );
        assert!(
            !g.edges.iter().any(|e| e.from == "w" && e.to == "c"),
            "no write→trigger execution edge is ever stored"
        );
    }

    #[test]
    fn wire_edge_replaces_the_previous_commit_gate() {
        // A collect trigger holds ONE commit target: wiring a second write replaces
        // the first gate (whichever gesture spelled it).
        let g = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w1"), write_node("w2")],
            edges: vec![edge("c", "w1", ""), edge("c", "w1", "commit")],
        };
        let (g, msg) = wire_edge(g, edge("w2", "c", ""));
        assert!(msg.is_none());
        let commits: Vec<_> = g
            .edges
            .iter()
            .filter(|e| e.from_port == COMMIT_PORT)
            .collect();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].to, "w2");
    }

    #[test]
    fn wire_edge_rejects_meaningless_trigger_targets() {
        // An edge into a NON-collect trigger has no meaning — untouched + a message.
        let g = sample_graph(); // t1 is a webhook trigger
        let before = g.edges.len();
        let (g, msg) = wire_edge(g, edge("a1", "t1", ""));
        assert!(msg.is_some());
        assert_eq!(g.edges.len(), before);

        // A commit-port edge landing on a trigger is equally meaningless.
        let g2 = FlowGraph {
            nodes: vec![collect_node("c"), collect_node("c2"), write_node("w")],
            edges: vec![],
        };
        let (g2, msg) = wire_edge(g2, edge("c", "c2", "commit"));
        assert!(msg.is_some());
        assert!(g2.edges.is_empty());

        // A trigger dragged onto a collect trigger is not a commit gate either.
        let g3 = FlowGraph {
            nodes: vec![collect_node("c"), collect_node("c2")],
            edges: vec![],
        };
        let (g3, msg) = wire_edge(g3, edge("c2", "c", ""));
        assert!(msg.is_some());
        assert!(g3.edges.is_empty());
    }

    #[test]
    fn wire_edge_passes_plain_edges_through() {
        // The ordinary trigger→action wire behaves exactly like add_edge.
        let g = FlowGraph {
            nodes: sample_graph().nodes,
            edges: vec![],
        };
        let (g, msg) = wire_edge(g, edge("t1", "a1", ""));
        assert!(msg.is_none());
        assert_eq!(g.edges.len(), 1);
        // Self-loop stays a silent no-op.
        let (g, msg) = wire_edge(g, edge("a1", "a1", ""));
        assert!(msg.is_none());
        assert_eq!(g.edges.len(), 1);
    }

    #[test]
    fn commit_edge_round_trips_through_commit_on() {
        // A collect node, a write node, a data edge, and a `commit` gate edge.
        let g = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w")],
            edges: vec![edge("c", "w", ""), edge("c", "w", "commit")],
        };
        // Saving folds the commit edge into the trigger's `commit_on` field and drops
        // it from the serialized edges (the backend never sees a commit edge).
        let spec = graph_to_spec_value(&g);
        let gv = &spec["graph"];
        assert_eq!(gv["nodes"][0]["trigger"]["commit_on"], json!("w"));
        let edges = gv["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "only the data edge is serialized");
        assert_eq!(edges[0]["from_port"], json!(""));

        // Loading lifts `commit_on` back into a visual commit edge and strips the field.
        let back = flow_from_spec(&spec).unwrap();
        let trig = match &back.nodes[0].kind {
            FlowKind::Trigger { trigger } => trigger.clone(),
            _ => panic!("expected a trigger node"),
        };
        assert!(
            trig.get("commit_on").is_none(),
            "commit_on lifted out of the in-editor JSON"
        );
        assert!(back
            .edges
            .iter()
            .any(|e| e.from == "c" && e.to == "w" && e.from_port == "commit"));
        assert!(back
            .edges
            .iter()
            .any(|e| e.from == "c" && e.to == "w" && e.from_port.is_empty()));
        // spec → flow → spec is identity.
        assert_eq!(graph_to_spec_value(&back), spec);
    }

    #[test]
    fn commit_edges_are_gates_not_execution_edges() {
        // A commit edge is the ONLY thing wiring the write node — so for *execution*
        // the write is unreachable and the trigger is a dead end (a commit gate runs
        // nothing), yet the graph is still valid (no cycle, has a trigger, endpoints
        // exist).
        let g = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w")],
            edges: vec![edge("c", "w", "commit")],
        };
        assert!(
            validate_flow(&g).is_ok(),
            "a commit-only graph is structurally valid"
        );
        let probs = problem_nodes(&g);
        assert!(
            probs.contains("w"),
            "w reachable only via a commit gate ⇒ never runs"
        );
        assert!(
            probs.contains("c"),
            "c has no execution out-edge ⇒ dead-end trigger"
        );

        // A commit-labelled back-edge does NOT form a cycle (commit edges are skipped).
        let cyc = FlowGraph {
            nodes: vec![collect_node("c"), write_node("w")],
            edges: vec![edge("c", "w", ""), edge("w", "c", "commit")],
        };
        assert!(
            !has_cycle(&cyc),
            "a commit back-edge is ignored by the cycle check"
        );
        assert!(validate_flow(&cyc).is_ok());
    }

    #[test]
    fn condition_out_port_points_are_stacked() {
        let n = FlowNode {
            id: "c".into(),
            kind: FlowKind::Condition {
                runtime: "js".into(),
                source: "x".into(),
            },
            position: FlowPos { x: 100.0, y: 100.0 },
        };
        let t = out_port_point(&n, "true");
        let f = out_port_point(&n, "false");
        // Both on the right edge, true above false.
        assert_eq!(t.0, 100.0 + NODE_W);
        assert_eq!(f.0, 100.0 + NODE_W);
        assert!(t.1 < f.1, "true port above false port");
        // The single in-port is on the left edge, vertically centered.
        let i = in_port_point(&n);
        assert_eq!(i, (100.0, 100.0 + NODE_H / 2.0));
    }

    #[test]
    fn parse_params_handles_empty_object_and_errors() {
        assert!(parse_params("   ").unwrap().is_empty());
        let m = parse_params(r#"{"to":"ops"}"#).unwrap();
        assert_eq!(m.get("to").unwrap(), &json!("ops"));
        assert!(parse_params("[1,2]").is_err());
        assert!(parse_params("{bad").is_err());
    }

    #[test]
    fn node_subtitle_summarizes_each_kind() {
        let t = FlowNode {
            id: "t".into(),
            kind: FlowKind::Trigger {
                trigger: json!({ "kind": "schedule" }),
            },
            position: FlowPos::default(),
        };
        assert_eq!(node_subtitle(&t), "schedule");
        let c = FlowNode {
            id: "c".into(),
            kind: FlowKind::Code {
                runtime: "shell".into(),
                source: String::new(),
            },
            position: FlowPos::default(),
        };
        assert_eq!(node_subtitle(&c), "code · shell");
    }

    #[test]
    fn tag_matches_the_backend_discriminants() {
        assert_eq!(FlowKind::Trigger { trigger: json!({}) }.tag(), "trigger");
        assert_eq!(FlowKind::Action { action: json!({}) }.tag(), "action");
        assert_eq!(
            FlowKind::Code {
                runtime: "js".into(),
                source: String::new()
            }
            .tag(),
            "code"
        );
        assert_eq!(
            FlowKind::Condition {
                runtime: "js".into(),
                source: String::new()
            }
            .tag(),
            "condition"
        );
    }

    #[test]
    fn agent_params_round_trip_and_omit_empties() {
        // Build from the form: blanks/empties omitted; tools/skills arrays included;
        // output "json" is set.
        let v = agent_params_from_form(
            "  be terse  ",
            "",
            &["notify".to_string(), "recall".to_string()],
            "triage, , inbox",
            "json",
            "  high  ",
        );
        assert_eq!(v["kind"], json!("llm_agent"));
        assert_eq!(v["system"], json!("be terse"));
        assert!(v.get("model").is_none(), "blank model omitted");
        assert_eq!(v["tools"], json!(["notify", "recall"]));
        assert_eq!(v["skills"], json!(["triage", "inbox"]));
        assert_eq!(v["output"], json!("json"));
        assert_eq!(v["reasoning_effort"], json!("high"));

        // Empty tools array → omitted; "text" output → omitted; blanks too.
        let bare = agent_params_from_form("", "gpt-x", &[], "", "text", "");
        assert_eq!(bare["model"], json!("gpt-x"));
        assert!(bare.get("tools").is_none());
        assert!(bare.get("system").is_none());
        assert!(bare.get("skills").is_none());
        assert!(bare.get("output").is_none(), "text output omitted");
        assert!(
            bare.get("reasoning_effort").is_none(),
            "blank reasoning omitted"
        );

        // Parse back into the form fields (round-trip).
        let (system, model, tools, skills, output, reasoning) = agent_form_from_params(&v);
        assert_eq!(system, "be terse");
        assert_eq!(model, "");
        assert_eq!(tools, vec!["notify".to_string(), "recall".to_string()]);
        assert_eq!(skills, "triage, inbox");
        assert_eq!(output, "json");
        assert_eq!(reasoning, "high");
        // An absent output reads back as "text".
        assert_eq!(
            agent_form_from_params(&json!({ "kind": "llm_agent" })).4,
            "text"
        );
        // An absent reasoning reads back as "" (off).
        assert_eq!(
            agent_form_from_params(&json!({ "kind": "llm_agent" })).5,
            ""
        );

        // An llm_agent action is rendered as an "agent" node, not a plain action.
        assert!(is_agent_action(&json!({ "kind": "llm_agent" })));
        assert!(!is_agent_action(&json!({ "kind": "notify" })));
    }

    #[test]
    fn classifier_action_builds_json_steered_agent_and_round_trips() {
        let v = classifier_action(
            &[
                "spam".to_string(),
                "  ".to_string(), // blank → dropped
                "ham".to_string(),
            ],
            "  Label the email.  ",
        );
        // It's an `llm_agent` steered to JSON, with the outcomes kept as metadata.
        assert_eq!(v["kind"], json!("llm_agent"));
        assert_eq!(v["output"], json!("json"));
        assert_eq!(v["outcomes"], json!(["spam", "ham"]));
        assert_eq!(v["instructions"], json!("Label the email."));
        // The generated system prompt names each outcome and the instruction.
        let sys = v["system"].as_str().unwrap();
        assert!(sys.contains("- spam") && sys.contains("- ham"));
        assert!(sys.contains("Label the email."));
        assert!(sys.contains("0.0 to 1.0"));

        // A classifier is its own visual kind — detected ahead of a plain agent.
        assert!(is_classifier(&v));
        assert!(is_agent_action(&v));
        assert!(!is_classifier(&json!({ "kind": "llm_agent" })));

        // Round-trip back into the editor fields.
        let (outcomes, instructions) = classifier_form_from_params(&v);
        assert_eq!(outcomes, vec!["spam".to_string(), "ham".to_string()]);
        assert_eq!(instructions, "Label the email.");

        // The textarea parser trims and drops blank lines, preserving order.
        assert_eq!(
            parse_outcomes("  a \n\n  b\nc  \n"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );

        // No instructions → the field is omitted but the node is still a classifier.
        let bare = classifier_action(&["x".to_string()], "   ");
        assert!(bare.get("instructions").is_none());
        assert!(is_classifier(&bare));
    }

    #[test]
    fn typed_action_forms_build_clean_params() {
        // Notify: message trimmed, channel included; blank channel omitted.
        assert_eq!(
            notify_params("ops", "  ship it  "),
            json!({ "kind": "notify", "message": "ship it", "channel": "ops" })
        );
        assert_eq!(
            notify_params("  ", "hi"),
            json!({ "kind": "notify", "message": "hi" })
        );

        // Chat thread output: title/message trim; blank title delegates to the
        // backend's default.
        assert_eq!(
            chat_thread_params(" Daily report ", " {{ inputs.agent.text }} "),
            json!({
                "kind": "create_chat_thread",
                "title": "Daily report",
                "message": "{{ inputs.agent.text }}"
            })
        );
        assert_eq!(
            chat_thread_params(" ", "done"),
            json!({ "kind": "create_chat_thread", "message": "done" })
        );

        // Create note: title + markdown trimmed, tags split on commas (blanks dropped).
        assert_eq!(
            note_params("Standup", "notes", "work, , urgent"),
            json!({ "kind": "create_note", "title": "Standup", "markdown": "notes", "tags": ["work", "urgent"] })
        );
        // Blank markdown/tags omitted.
        assert_eq!(
            note_params("T", "  ", " "),
            json!({ "kind": "create_note", "title": "T" })
        );

        // Create task: all fields trimmed and included.
        assert_eq!(
            task_params("  bd1 ", "col2", "  Ship it ", "do the thing"),
            json!({ "kind": "create_task", "board_id": "bd1", "column_id": "col2", "title": "Ship it", "body": "do the thing" })
        );
        // Optional column/body blank → omitted (the backend defaults the column).
        assert_eq!(
            task_params("bd1", "  ", "Title only", " "),
            json!({ "kind": "create_task", "board_id": "bd1", "title": "Title only" })
        );

        // Create event: every field set, all_day on.
        assert_eq!(
            event_params(
                "  Standup ",
                "2026-06-18T09:00:00Z",
                "2026-06-18T09:15:00Z",
                " Room 1 ",
                "daily sync",
                "cal9",
                true,
            ),
            json!({
                "kind": "create_event",
                "summary": "Standup",
                "start": "2026-06-18T09:00:00Z",
                "end": "2026-06-18T09:15:00Z",
                "location": "Room 1",
                "body": "daily sync",
                "calendar_id": "cal9",
                "all_day": true
            })
        );
        // Minimal: only summary/start/end; blanks + all_day=false omitted.
        assert_eq!(
            event_params(
                "Sync",
                "2026-06-18T09:00:00Z",
                "2026-06-18T10:00:00Z",
                " ",
                "",
                "  ",
                false
            ),
            json!({
                "kind": "create_event",
                "summary": "Sync",
                "start": "2026-06-18T09:00:00Z",
                "end": "2026-06-18T10:00:00Z"
            })
        );

        // Summarize: all fields set; max_words parses as a positive int.
        assert_eq!(
            summarize_params("{{ inputs.fetch.content }}", " Focus. ", " 120 ", "m/x"),
            json!({
                "kind": "summarize",
                "input": "{{ inputs.fetch.content }}",
                "instructions": "Focus.",
                "max_words": 120,
                "model": "m/x"
            })
        );
        // Blank input (summarize the trigger) + unparsable/zero max_words omitted.
        assert_eq!(
            summarize_params("  ", "", "abc", " "),
            json!({ "kind": "summarize" })
        );
        assert_eq!(
            summarize_params("t", "", "0", ""),
            json!({ "kind": "summarize", "input": "t" })
        );

        // Write object: content is verbatim (never trimmed — written bytes are
        // the user's), optional store/content_type omitted when blank.
        assert_eq!(
            write_object_params(" reports/a.md ", " body \n", "  ", "text/markdown"),
            json!({
                "kind": "write_object",
                "key": "reports/a.md",
                "content": " body \n",
                "content_type": "text/markdown"
            })
        );
        assert_eq!(
            write_object_params("k", "", "", ""),
            json!({ "kind": "write_object", "key": "k" })
        );

        // Move object: to_key/store fields optional.
        assert_eq!(
            move_object_params(
                "{{ trigger.key }}",
                "archive/{{ trigger.key }}",
                "",
                " cold "
            ),
            json!({
                "kind": "move_object",
                "from_key": "{{ trigger.key }}",
                "to_key": "archive/{{ trigger.key }}",
                "to_store": "cold"
            })
        );

        // Webhook: the default post method + empty payload/headers stay omitted
        // (minimal stored node); a non-default method + non-empty objects land.
        assert_eq!(
            webhook_params("https://h.test/x", "post", Some(Map::new()), None),
            json!({ "kind": "webhook", "url": "https://h.test/x" })
        );
        let payload = parse_params(r#"{"a": 1}"#).unwrap();
        let headers = parse_params(r#"{"Authorization": "Bearer t"}"#).unwrap();
        assert_eq!(
            webhook_params("https://h.test/x", "put", Some(payload), Some(headers)),
            json!({
                "kind": "webhook",
                "url": "https://h.test/x",
                "method": "put",
                "payload": { "a": 1 },
                "headers": { "Authorization": "Bearer t" }
            })
        );
    }

    #[test]
    fn merge_field_sets_clears_and_preserves_other_keys() {
        let t = json!({ "kind": "channel_message", "channel": "ops", "filter": { "text": "x" } });
        // Set a typed field → updated, the opaque `filter` preserved.
        let t2 = merge_field(t.clone(), "channel", " sales ");
        assert_eq!(
            t2,
            json!({ "kind": "channel_message", "channel": "sales", "filter": { "text": "x" } })
        );
        // Blank value → key removed, everything else kept.
        let t3 = merge_field(t, "channel", "  ");
        assert_eq!(
            t3,
            json!({ "kind": "channel_message", "filter": { "text": "x" } })
        );
        // A non-object passes through unchanged.
        assert_eq!(merge_field(json!("nope"), "k", "v"), json!("nope"));

        // A string-list field (`extensions`) is written as a JSON array split from
        // the comma-separated value; a blank value removes the key (not [] ).
        let s = json!({ "kind": "storage_object", "event": "created", "bucket": "docs" });
        let s2 = merge_field(s.clone(), "extensions", " docx , xlsx ,, pptx ");
        assert_eq!(
            s2,
            json!({ "kind": "storage_object", "event": "created", "bucket": "docs",
                    "extensions": ["docx", "xlsx", "pptx"] })
        );
        assert_eq!(merge_field(s2, "extensions", "  "), s);
    }

    #[test]
    fn chatbot_template_instantiates_a_valid_trigger_agent_graph() {
        let (nodes, edges) = chatbot_template();
        assert_eq!(nodes.len(), 2);
        let (g, counter, ids) = instantiate_template(FlowGraph::default(), nodes, edges, 1);
        assert_eq!(g.nodes.len(), 2);
        assert_eq!(g.edges.len(), 1);
        assert!(counter > 1, "the id counter advanced");
        // A channel_message trigger wired into an llm_agent action.
        assert!(matches!(
            &g.nodes[0].kind,
            FlowKind::Trigger { trigger } if trigger["kind"] == "channel_message"
        ));
        assert!(matches!(
            &g.nodes[1].kind,
            FlowKind::Action { action } if is_agent_action(action)
        ));
        assert_eq!(g.edges[0].from, ids[0]);
        assert_eq!(g.edges[0].to, ids[1]);
        // It's a valid graph out of the box, and re-instantiating gives fresh,
        // non-colliding ids (so a second insert doesn't clash).
        assert!(validate_flow(&g).is_ok());
        let (g2, _, ids2) = instantiate_template(
            g.clone(),
            scheduled_template().0,
            scheduled_template().1,
            counter,
        );
        assert_eq!(g2.nodes.len(), 4);
        assert!(ids2.iter().all(|new| !ids.contains(new)), "fresh ids");
        assert!(validate_flow(&g2).is_ok());
    }

    #[test]
    fn unreachable_nodes_flags_orphans_not_wired_to_a_trigger() {
        let node = |id: &str, kind: FlowKind| FlowNode {
            id: id.to_string(),
            kind,
            position: FlowPos::default(),
        };
        let edge = |from: &str, to: &str| FlowEdge {
            from: from.to_string(),
            to: to.to_string(),
            from_port: String::new(),
            to_port: String::new(),
        };
        let act = || FlowKind::Action {
            action: json!({ "kind": "summarize" }),
        };
        // trigger t → a (reachable); b → c is an orphan chain (no trigger feeds it).
        let g = FlowGraph {
            nodes: vec![
                node(
                    "t",
                    FlowKind::Trigger {
                        trigger: json!({ "kind": "webhook", "path": "/x" }),
                    },
                ),
                node("a", act()),
                node("b", act()),
                node("c", act()),
            ],
            edges: vec![edge("t", "a"), edge("b", "c")],
        };
        let u = unreachable_nodes(&g);
        assert!(
            !u.contains("t") && !u.contains("a"),
            "trigger + wired node ok"
        );
        assert!(u.contains("b") && u.contains("c"), "orphan chain flagged");
        assert_eq!(u.len(), 2);
    }

    #[test]
    fn problem_nodes_also_flags_dead_end_triggers() {
        let node = |id: &str, kind: FlowKind| FlowNode {
            id: id.to_string(),
            kind,
            position: FlowPos::default(),
        };
        let trig = || FlowKind::Trigger {
            trigger: json!({ "kind": "webhook", "path": "/x" }),
        };
        let act = || FlowKind::Action {
            action: json!({ "kind": "summarize" }),
        };
        // t1 → a is fine; t2 is a dead-end trigger (no outgoing edge).
        let g = FlowGraph {
            nodes: vec![node("t1", trig()), node("a", act()), node("t2", trig())],
            edges: vec![FlowEdge {
                from: "t1".into(),
                to: "a".into(),
                from_port: String::new(),
                to_port: String::new(),
            }],
        };
        let p = problem_nodes(&g);
        assert!(p.contains("t2"), "dead-end trigger flagged");
        assert!(
            !p.contains("t1") && !p.contains("a"),
            "a wired trigger + its action are fine"
        );
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn duplicate_node_clones_with_a_fresh_id_and_no_edges() {
        let g = FlowGraph {
            nodes: vec![FlowNode {
                id: "agent1".into(),
                kind: FlowKind::Action {
                    action: json!({ "kind": "llm_agent", "tools": ["notify"] }),
                },
                position: FlowPos { x: 100.0, y: 60.0 },
            }],
            edges: vec![],
        };
        let (g2, _next, new_id) = duplicate_node(g, "agent1", 2);
        let new_id = new_id.expect("duplicated");
        assert_ne!(new_id, "agent1", "fresh id");
        assert_eq!(g2.nodes.len(), 2);
        let copy = g2.nodes.iter().find(|n| n.id == new_id).unwrap();
        // Same kind/config, offset+snapped position, no edges copied.
        assert!(matches!(&copy.kind, FlowKind::Action { action } if is_agent_action(action)));
        assert_eq!(
            copy.position,
            FlowPos {
                x: snap(124.0),
                y: snap(84.0)
            }
        );
        assert!(g2.edges.is_empty());
        // A missing id is a no-op.
        let (g3, _, none) = duplicate_node(g2.clone(), "nope", 9);
        assert!(none.is_none());
        assert_eq!(g3.nodes.len(), g2.nodes.len());
    }

    #[test]
    fn snap_rounds_to_the_grid() {
        assert_eq!(snap(0.0), 0.0);
        assert_eq!(snap(24.0), 24.0);
        assert_eq!(snap(11.0), 0.0);
        assert_eq!(snap(13.0), 24.0);
        assert_eq!(snap(50.0), 48.0);
        assert_eq!(snap(-50.0), -48.0);
    }
}
