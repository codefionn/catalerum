use super::*;

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
pub(super) fn has_cycle(graph: &FlowGraph) -> bool {
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
pub(super) fn unreachable_nodes(graph: &FlowGraph) -> HashSet<String> {
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
pub(super) fn problem_nodes(graph: &FlowGraph) -> HashSet<String> {
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
pub(super) fn zoom_to_cursor(
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
pub(super) fn fit_transform(nodes: &[FlowNode], viewport: (f64, f64)) -> Option<((f64, f64), f64)> {
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
pub(super) const NODE_W: f64 = 168.0;
pub(super) const NODE_H: f64 = 64.0;

/// Canvas snap grid (matches the dotted background). Node positions snap to it so
/// graphs stay tidy without fiddly pixel-aligning.
const GRID: f64 = 24.0;

/// Snap a canvas coordinate to the nearest [`GRID`] line. Pure + testable.
pub(super) fn snap(v: f64) -> f64 {
    (v / GRID).round() * GRID
}

/// The canvas-space center of a node's **input** port (left edge, vertically
/// centered).
pub(super) fn in_port_point(node: &FlowNode) -> (f64, f64) {
    (node.position.x, node.position.y + NODE_H / 2.0)
}

/// Where a commit gate lands on its target node: bottom edge, slightly left of
/// center — visually a gate tapping the write node, kept clear of the input port
/// so the data edge and the gate never overlap into one unreadable double-arrow.
pub(super) fn commit_anchor_point(node: &FlowNode) -> (f64, f64) {
    (node.position.x + NODE_W * 0.35, node.position.y + NODE_H)
}

/// The canvas-space center of a node's **output** port `port` (right edge). A
/// condition stacks its `"true"` / `"false"` ports; every other kind has one
/// centered port.
pub(super) fn out_port_point(node: &FlowNode, port: &str) -> (f64, f64) {
    let x = node.position.x + NODE_W;
    let ports = node.kind.out_ports();
    let idx = ports.iter().position(|p| *p == port).unwrap_or(0);
    let n = ports.len().max(1);
    // Evenly distribute n ports down the right edge.
    let step = NODE_H / (n as f64 + 1.0);
    let y = node.position.y + step * (idx as f64 + 1.0);
    (x, y)
}
