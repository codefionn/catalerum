//! The node-graph automation model (SOUL §11, Phase A): a workspace's automation
//! can be authored as a **directed acyclic graph** of typed nodes instead of the
//! legacy linear `trigger → condition → action` list. The graph lives in the
//! existing free-form `spec` JSON column (under a `"graph"` key), so legacy
//! automations keep running unchanged — the executor only runs the graph when one
//! is present.
//!
//! This module is the **pure** model + helpers: the serde types ([`Graph`],
//! [`Node`], [`Edge`], [`NodeKind`]), authoring-time [`Graph::validate`], and the
//! topological ordering ([`Graph::topo_order`]) the DAG executor walks. It has no
//! I/O and no engine state — every helper is a total function over the graph, unit
//! tested below. The DAG *executor* (which records durable run/step state) lives in
//! [`crate::executor`]; the inline-code runtimes (Boa JS) are Phase B.
//!
//! Node kinds:
//! - **Trigger** — wraps a §11 [`Trigger`]; a graph entry point. Its output is the
//!   firing event (or null for a manual run).
//! - **Action** — wraps a §11 [`Action`]; dispatched via the existing `ActionRunner`.
//! - **Code** `{ runtime, source }` — a pure data-transform node run by a
//!   `CodeRunner` (Phase B; the Phase-A default fails it).
//! - **Condition** `{ runtime, source }` — like Code, but its truthy/falsy result
//!   routes execution down the `"true"` / `"false"` out-edge ports.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::{Action, Trigger};

/// A node-graph automation: typed [`Node`]s connected by directed [`Edge`]s. The
/// engine runs it as a DAG (data flows along edges; each node's output feeds its
/// downstream nodes). Validated by [`Graph::validate`] before it is persisted.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub edges: Vec<Edge>,
}

/// One node in a [`Graph`]: a stable `id` (unique within the graph), its typed
/// [`NodeKind`] (which flattens its `kind`-tagged payload), and an editor
/// `position` (ignored by the engine, round-tripped for the canvas).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    #[serde(flatten)]
    pub kind: NodeKind,
    #[serde(default)]
    pub position: Position,
}

/// A node's canvas coordinates (SOUL §11 Phase C). Round-tripped for the visual
/// editor; the engine never reads it. Defaults to the origin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// A directed edge from one node's output port to another node's input port. The
/// ports default to the empty string (`""`, a node's single default port); a
/// [`NodeKind::Condition`] uses `from_port` `"true"` / `"false"` to route its two
/// branches.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub from_port: String,
    #[serde(default)]
    pub to_port: String,
}

/// The typed payload of a [`Node`], discriminated by its `kind` JSON tag.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeKind {
    /// A graph entry point wrapping a §11 [`Trigger`]; its output is the firing
    /// event (or null for a manual run).
    Trigger { trigger: Trigger },
    /// A §11 [`Action`], dispatched through the existing `ActionRunner`.
    Action { action: Action },
    /// A pure data-transform run by a `CodeRunner` (Phase B). `runtime` selects the
    /// language (e.g. `"js"`); `source` is the inline code.
    Code { runtime: String, source: String },
    /// Like [`NodeKind::Code`], but a branch gate: its truthy/falsy result routes
    /// execution down the `"true"` / `"false"` out-edge ports.
    Condition { runtime: String, source: String },
    /// The head of a **loop region** (SOUL §11): iterate `source` (a dotted path
    /// into this node's input envelope resolving to a JSON array, e.g.
    /// `"inputs.web_search.searches.rust.results"`) and run the region body once per element,
    /// binding the element to `item` (and its 0-based position to `index`) as
    /// top-level template variables the body can reference (`{{ item.title }}`).
    /// The region is the nodes between this node and its paired [`LoopEnd`].
    /// `max_iterations` caps the run (≤ [`MAX_LOOP_ITERATIONS`]).
    ForEach {
        source: String,
        item: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<usize>,
    },
    /// The tail of a loop region: pairs with its [`ForEach`](NodeKind::ForEach) by
    /// id (`for_each`). Its output is the array of per-iteration body results, so
    /// nodes downstream of the loop read `inputs.<loop_end_id>`.
    LoopEnd { for_each: String },
}

/// Hard cap on loop iterations per `ForEach` run (SOUL §11) — a runaway guard,
/// independent of a node's own (smaller) `max_iterations`.
pub const MAX_LOOP_ITERATIONS: usize = 1000;

/// Hard cap on the number of nodes inside one loop-region body — keeps the
/// durable per-iteration step ordinals within a bounded, collision-free range.
pub const MAX_LOOP_BODY_NODES: usize = 256;

/// A resolved loop region (SOUL §11): a [`ForEach`](NodeKind::ForEach) head, its
/// paired [`LoopEnd`](NodeKind::LoopEnd), and the body nodes between them in
/// topological order. Produced by [`Graph::for_each_regions`] and consumed by the
/// executor to run the body once per element.
#[derive(Clone, Debug, PartialEq)]
pub struct ForEachRegion {
    /// The `ForEach` node id.
    pub for_each: String,
    /// The paired `LoopEnd` node id.
    pub loop_end: String,
    /// Body node ids (excludes `for_each`/`loop_end`), in region-topo order.
    pub body: Vec<String>,
    /// The array source path, evaluated against the `ForEach`'s input envelope.
    pub source: String,
    /// The loop variable name bound to each element.
    pub item: String,
    /// Optional loop-index variable name.
    pub index: Option<String>,
    /// Optional per-node iteration cap (already validated ≤ [`MAX_LOOP_ITERATIONS`]).
    pub max_iterations: Option<usize>,
}

impl NodeKind {
    /// The stable `kind` discriminant (matching the stored JSON tag) — recorded on
    /// each run step so the audit row carries the node's identity.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            NodeKind::Trigger { .. } => "trigger",
            NodeKind::Action { .. } => "action",
            NodeKind::Code { .. } => "code",
            NodeKind::Condition { .. } => "condition",
            NodeKind::ForEach { .. } => "for_each",
            NodeKind::LoopEnd { .. } => "loop_end",
        }
    }
}

impl Node {
    /// Whether this node is a graph entry point (a [`NodeKind::Trigger`]).
    #[must_use]
    pub fn is_trigger(&self) -> bool {
        matches!(self.kind, NodeKind::Trigger { .. })
    }
}

impl Graph {
    /// Parse a graph out of an automation's `spec` JSON: `Some(graph)` when `spec`
    /// is an object carrying a `"graph"` key that parses to a [`Graph`]; `None`
    /// otherwise (no spec, no `graph` key, or a non-graph spec — the executor then
    /// runs the legacy linear `actions` loop). A malformed `graph` value surfaces as
    /// `Some(Err(..))` so the run fails loudly rather than silently falling back.
    #[must_use]
    pub fn from_spec(spec: Option<&Value>) -> Option<Result<Graph, String>> {
        let graph = spec?.as_object()?.get("graph")?;
        Some(serde_json::from_value::<Graph>(graph.clone()).map_err(|e| e.to_string()))
    }

    /// The node with `id`, if any.
    #[must_use]
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// The out-edges leaving `id`, optionally restricted to a single `from_port`
    /// (e.g. `Some("true")` for a condition's true-branch). `None` keeps every
    /// out-edge regardless of port.
    pub fn out_edges<'a>(
        &'a self,
        id: &'a str,
        from_port: Option<&'a str>,
    ) -> impl Iterator<Item = &'a Edge> + 'a {
        self.edges
            .iter()
            .filter(move |e| e.from == id && from_port.is_none_or(|p| e.from_port == p))
    }

    /// The ids of nodes with an edge **into** `id` (its upstream/data inputs), in
    /// edge order, de-duplicated.
    #[must_use]
    pub fn upstream(&self, id: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        self.edges
            .iter()
            .filter(|e| e.to == id)
            .filter(|e| seen.insert(e.from.clone()))
            .map(|e| e.from.clone())
            .collect()
    }

    /// The Trigger nodes' triggers, in node order (Phase A compiles these into the
    /// automation's `triggers` column so existing dispatch matching still fires it).
    #[must_use]
    pub fn trigger_specs(&self) -> Vec<Trigger> {
        self.nodes
            .iter()
            .filter_map(|n| match &n.kind {
                NodeKind::Trigger { trigger } => Some(trigger.clone()),
                _ => None,
            })
            .collect()
    }

    /// Authoring-time validation (SOUL §11): node ids are unique; every edge
    /// references existing endpoints; the graph has ≥1 Trigger node; and it is
    /// acyclic (a DAG). The cheap check a create/update path runs before persisting.
    ///
    /// # Errors
    /// A human-readable message for the first violation found (duplicate id,
    /// dangling edge endpoint, no trigger, or a cycle).
    pub fn validate(&self) -> Result<(), String> {
        // Unique node ids.
        let mut ids = HashSet::new();
        for n in &self.nodes {
            if !ids.insert(n.id.as_str()) {
                return Err(format!("duplicate node id '{}'", n.id));
            }
        }
        // Every edge endpoint references an existing node.
        for e in &self.edges {
            if !ids.contains(e.from.as_str()) {
                return Err(format!("edge from unknown node '{}'", e.from));
            }
            if !ids.contains(e.to.as_str()) {
                return Err(format!("edge to unknown node '{}'", e.to));
            }
        }
        // At least one trigger node (else the graph can never fire).
        let trigger_nodes: Vec<&Node> = self.nodes.iter().filter(|n| n.is_trigger()).collect();
        if trigger_nodes.is_empty() {
            return Err("graph has no trigger node".to_string());
        }
        // A trigger heads the graph: the executor seeds it as an entry (in-degree 0),
        // never driven by an upstream edge. An edge *into* a trigger is therefore
        // meaningless — the node would be both a seed and an apparent dependency —
        // so reject it at authoring time rather than persist a confusing graph.
        let trigger_ids: HashSet<&str> = trigger_nodes.iter().map(|n| n.id.as_str()).collect();
        for e in &self.edges {
            if trigger_ids.contains(e.to.as_str()) {
                return Err(format!(
                    "edge into trigger node '{}' — a trigger heads the graph and cannot have an \
                     incoming edge",
                    e.to
                ));
            }
        }
        // A **collect** trigger heads its own graph (SOUL §10/§28). A collect run's
        // per-item payload is not a matchable `TriggerEvent`, so the executor seeds
        // **every** trigger node — a second trigger would have its branch wrongly
        // driven by each collected item (and the collect branch wrongly driven by the
        // other trigger). Require a collect graph to have exactly one trigger node.
        let has_collect = trigger_nodes
            .iter()
            .any(|n| matches!(&n.kind, NodeKind::Trigger { trigger } if trigger.is_collect()));
        if has_collect && trigger_nodes.len() > 1 {
            return Err(
                "a graph with a collect trigger (collect_email/collect_calendar) must have exactly \
                 one trigger node — a collect source heads its own graph"
                    .to_string(),
            );
        }
        // A collect trigger's `commit_on` (SOUL §10/§28) is a **node reference**,
        // not a DAG edge — so it isn't checked by the edge-endpoint pass above. It
        // must name an existing **Action** node (the write whose success gates the
        // cursor advance); a typo would otherwise silently never commit, re-collecting
        // every item forever. It is deliberately a data field on the trigger (inert
        // to `topo_order`), since a literal write→trigger edge would close a cycle.
        for n in &self.nodes {
            if let NodeKind::Trigger { trigger } = &n.kind {
                if let Some(target) = trigger.commit_on() {
                    match self.node(target) {
                        None => {
                            return Err(format!(
                                "collect trigger '{}' commit_on references unknown node '{target}'",
                                n.id
                            ));
                        }
                        Some(t) if !matches!(t.kind, NodeKind::Action { .. }) => {
                            return Err(format!(
                                "collect trigger '{}' commit_on must reference an action (write) node, not '{target}'",
                                n.id
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        // A Condition node routes execution solely through its "true"/"false"
        // out-ports: the executor follows a condition's out-edge only when the
        // edge's `from_port` equals the branch the condition took. An out-edge
        // from a condition on any other port — including the default "" (an edge
        // drawn without picking a branch) — is therefore dead: it can never fire.
        // Reject it at authoring time rather than persist a branch that silently
        // never runs. (A non-condition node ignores `from_port`, so this only
        // constrains conditions.)
        let condition_ids: HashSet<&str> = self
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::Condition { .. }))
            .map(|n| n.id.as_str())
            .collect();
        for e in &self.edges {
            if condition_ids.contains(e.from.as_str())
                && e.from_port != "true"
                && e.from_port != "false"
            {
                return Err(format!(
                    "edge from condition node '{}' uses port '{}' — a condition routes only \
                     through its 'true'/'false' out-ports",
                    e.from, e.from_port
                ));
            }
        }
        // Acyclic — `topo_order` fails with "cycle" if not.
        self.topo_order().map(|_| ())?;
        // Loop regions (SOUL §11): every ForEach pairs with a LoopEnd, its body is
        // non-empty and isolated, and it nests nothing. Runs after the acyclic check
        // so the body-topo ordering inside is well-defined.
        self.for_each_regions().map(|_| ())
    }

    /// Non-fatal authoring diagnostics (SOUL §11): problems that don't stop a graph
    /// from being saved or run, but almost always mean it won't do what the author
    /// intended — a node no trigger can reach (so it never runs), a trigger wired to
    /// nothing, or a condition branch left unconnected (so that outcome does nothing).
    ///
    /// The soft complement of [`validate`](Graph::validate): `validate` **rejects**
    /// graphs that *cannot* run; `warnings` **flags** graphs that run but look wrong,
    /// so a create/edit path can surface them without blocking the save. Returns one
    /// human-readable line per issue, in node order (empty when the graph is clean).
    ///
    /// Assumes a structurally sound graph (unique ids, every edge references a real
    /// node) — call it after [`validate`](Graph::validate) succeeds. Reachability
    /// follows out-edges from every Trigger node; a terminal action (a leaf with no
    /// out-edge) is normal and never warned about.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        // Forward-reachable set from the graph's entry points (its Trigger nodes).
        let mut reachable: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = self
            .nodes
            .iter()
            .filter(|n| n.is_trigger())
            .map(|n| n.id.as_str())
            .collect();
        while let Some(id) = stack.pop() {
            if !reachable.insert(id) {
                continue;
            }
            for e in &self.edges {
                if e.from == id {
                    stack.push(e.to.as_str());
                }
            }
        }

        let mut out = Vec::new();
        for n in &self.nodes {
            // A non-trigger node no trigger can reach never runs — the classic
            // "node not connected" mistake. Skip the finer checks below for a dead
            // node (its unwired branches are already implied by unreachability).
            if !n.is_trigger() && !reachable.contains(n.id.as_str()) {
                out.push(format!(
                    "node '{}' is not connected to any trigger and will never run",
                    n.id
                ));
                continue;
            }
            // A trigger with no out-edges fires but drives nothing downstream.
            if n.is_trigger() && !self.edges.iter().any(|e| e.from == n.id) {
                out.push(format!(
                    "trigger node '{}' has no outgoing edges — it fires but runs nothing",
                    n.id
                ));
            }
            // A condition routes through its "true"/"false" ports; an unwired branch
            // silently does nothing when the condition takes that outcome.
            if matches!(n.kind, NodeKind::Condition { .. }) {
                for branch in ["true", "false"] {
                    if !self
                        .edges
                        .iter()
                        .any(|e| e.from == n.id && e.from_port == branch)
                    {
                        out.push(format!(
                            "condition node '{}' has no '{branch}' branch — that outcome runs nothing",
                            n.id
                        ));
                    }
                }
            }
        }
        out
    }

    /// A topological ordering of the node ids (Kahn's algorithm): every node
    /// precedes the nodes it has edges into. Assumes ids are unique (so call
    /// [`Graph::validate`] first, or accept that a duplicate id collapses).
    ///
    /// # Errors
    /// `"cycle"` if the graph is not acyclic (some node never reaches in-degree 0).
    pub fn topo_order(&self) -> Result<Vec<String>, String> {
        // In-degree per node id, seeded from the node list (preserve node order).
        let mut indeg: HashMap<&str, usize> =
            self.nodes.iter().map(|n| (n.id.as_str(), 0)).collect();
        // Adjacency: from -> [to]. Only count edges whose endpoints both exist, so a
        // dangling edge (already rejected by `validate`) can't underflow a degree.
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for e in &self.edges {
            if indeg.contains_key(e.from.as_str()) && indeg.contains_key(e.to.as_str()) {
                adj.entry(e.from.as_str()).or_default().push(e.to.as_str());
                *indeg.get_mut(e.to.as_str()).unwrap() += 1;
            }
        }
        // Seed the queue with the in-degree-0 nodes, in node order (stable output).
        let mut queue: VecDeque<&str> = self
            .nodes
            .iter()
            .filter(|n| indeg[n.id.as_str()] == 0)
            .map(|n| n.id.as_str())
            .collect();
        let mut order = Vec::with_capacity(self.nodes.len());
        while let Some(id) = queue.pop_front() {
            order.push(id.to_string());
            if let Some(tos) = adj.get(id) {
                for &to in tos {
                    let d = indeg.get_mut(to).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(to);
                    }
                }
            }
        }
        if order.len() == self.nodes.len() {
            Ok(order)
        } else {
            Err("cycle".to_string())
        }
    }

    /// Node ids reachable from `start` by following out-edges, **not expanding
    /// past** `boundary` (`boundary` is included if reached, but its own out-edges
    /// aren't followed). Used to bound a loop region at its `LoopEnd`.
    fn reachable_forward(&self, start: &str, boundary: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id == boundary {
                continue;
            }
            for e in self.edges.iter().filter(|e| e.from == id) {
                stack.push(e.to.clone());
            }
        }
        seen
    }

    /// Node ids reachable from `start` by following in-edges backward, not
    /// expanding past `boundary` (the `ForEach` head).
    fn reachable_backward(&self, start: &str, boundary: &str) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut stack = vec![start.to_string()];
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id == boundary {
                continue;
            }
            for e in self.edges.iter().filter(|e| e.to == id) {
                stack.push(e.from.clone());
            }
        }
        seen
    }

    /// Resolve every loop region (SOUL §11): pair each [`ForEach`](NodeKind::ForEach)
    /// with its [`LoopEnd`](NodeKind::LoopEnd), compute the body between them (in
    /// region-topo order), and validate the region is well-formed — a non-empty,
    /// isolated body containing no trigger and no nested loop. Both the authoring
    /// check (via [`validate`](Graph::validate)) and what the executor walks.
    ///
    /// # Errors
    /// A human-readable message for the first malformed region (unpaired ends,
    /// empty/oversized body, a boundary-crossing edge, a nested loop, a bad loop
    /// variable, or an over-cap `max_iterations`).
    pub fn for_each_regions(&self) -> Result<Vec<ForEachRegion>, String> {
        let loop_ends: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|n| matches!(n.kind, NodeKind::LoopEnd { .. }))
            .collect();
        // Region-topo order is the full topo order filtered to the body (safe:
        // callers run this after the acyclic check).
        let topo = self.topo_order().unwrap_or_default();

        let mut regions = Vec::new();
        let mut used_ends: HashSet<String> = HashSet::new();

        for f in self.nodes.iter() {
            let NodeKind::ForEach {
                source,
                item,
                index,
                max_iterations,
            } = &f.kind
            else {
                continue;
            };
            // Loop variables.
            if item.trim().is_empty() {
                return Err(format!(
                    "for_each node '{}' has an empty `item` variable",
                    f.id
                ));
            }
            if source.trim().is_empty() {
                return Err(format!(
                    "for_each node '{}' has an empty `source` path",
                    f.id
                ));
            }
            if let Some(ix) = index {
                if ix.trim().is_empty() {
                    return Err(format!(
                        "for_each node '{}' has an empty `index` variable",
                        f.id
                    ));
                }
                if ix == item {
                    return Err(format!(
                        "for_each node '{}' uses the same name '{ix}' for `item` and `index`",
                        f.id
                    ));
                }
            }
            if let Some(m) = max_iterations {
                if *m > MAX_LOOP_ITERATIONS {
                    return Err(format!(
                        "for_each node '{}' max_iterations {m} exceeds the cap {MAX_LOOP_ITERATIONS}",
                        f.id
                    ));
                }
            }
            // Pair with exactly one LoopEnd.
            let mut ends = loop_ends
                .iter()
                .filter(|l| matches!(&l.kind, NodeKind::LoopEnd { for_each } if for_each == &f.id));
            let end = match (ends.next(), ends.next()) {
                (None, _) => {
                    return Err(format!(
                        "for_each node '{}' has no matching loop_end (add a loop_end with for_each = '{}')",
                        f.id, f.id
                    ))
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "for_each node '{}' has multiple loop_end nodes referencing it",
                        f.id
                    ))
                }
                (Some(one), None) => *one,
            };
            used_ends.insert(end.id.clone());

            // Body = forward-reachable(ForEach) ∩ backward-reachable(LoopEnd), minus
            // the two ends, in region-topo order.
            let forward = self.reachable_forward(&f.id, &end.id);
            let backward = self.reachable_backward(&end.id, &f.id);
            let body: Vec<String> = topo
                .iter()
                .filter(|id| {
                    id.as_str() != f.id
                        && id.as_str() != end.id
                        && forward.contains(*id)
                        && backward.contains(*id)
                })
                .cloned()
                .collect();
            if body.is_empty() {
                return Err(format!(
                    "for_each node '{}' has an empty body — put at least one node between it and loop_end '{}'",
                    f.id, end.id
                ));
            }
            if body.len() > MAX_LOOP_BODY_NODES {
                return Err(format!(
                    "for_each node '{}' body has {} nodes, exceeding the cap {MAX_LOOP_BODY_NODES}",
                    f.id,
                    body.len()
                ));
            }
            let body_set: HashSet<&str> = body.iter().map(String::as_str).collect();

            // No trigger / nested loop inside the body.
            for id in &body {
                match self.node(id).map(|n| &n.kind) {
                    Some(NodeKind::Trigger { .. }) => {
                        return Err(format!(
                            "for_each region '{}' contains trigger node '{id}' — triggers head the graph, not a loop body",
                            f.id
                        ))
                    }
                    Some(NodeKind::ForEach { .. } | NodeKind::LoopEnd { .. }) => {
                        return Err(format!(
                            "for_each region '{}' contains nested loop node '{id}' — nested loops are not supported",
                            f.id
                        ))
                    }
                    _ => {}
                }
            }
            // Edge isolation: nothing crosses the region boundary except
            // ForEach→body and body→LoopEnd.
            for e in &self.edges {
                let from_in = body_set.contains(e.from.as_str());
                let to_in = body_set.contains(e.to.as_str());
                if to_in && !from_in && e.from != f.id {
                    return Err(format!(
                        "for_each region '{}' body node '{}' has an incoming edge from outside the region ('{}')",
                        f.id, e.to, e.from
                    ));
                }
                if from_in && !to_in && e.to != end.id {
                    return Err(format!(
                        "for_each region '{}' body node '{}' has an outgoing edge leaving the region (to '{}')",
                        f.id, e.from, e.to
                    ));
                }
                if e.from == f.id && !to_in {
                    return Err(format!(
                        "for_each node '{}' has an out-edge to '{}' which is not in its loop body",
                        f.id, e.to
                    ));
                }
                if e.to == end.id && !from_in {
                    return Err(format!(
                        "loop_end '{}' has an in-edge from '{}' which is not in the loop body",
                        end.id, e.from
                    ));
                }
            }

            regions.push(ForEachRegion {
                for_each: f.id.clone(),
                loop_end: end.id.clone(),
                body,
                source: source.clone(),
                item: item.clone(),
                index: index.clone(),
                max_iterations: *max_iterations,
            });
        }

        // No orphan LoopEnd (one that names a non-existent or non-ForEach node).
        for l in &loop_ends {
            if !used_ends.contains(&l.id) {
                let target = match &l.kind {
                    NodeKind::LoopEnd { for_each } => for_each.as_str(),
                    _ => "",
                };
                return Err(format!(
                    "loop_end '{}' references for_each '{target}' which is not a for_each node",
                    l.id
                ));
            }
        }

        Ok(regions)
    }
}

/// Build the per-node step `action` JSON recorded on a run (SOUL §11): the node's
/// identity (`node` id + `kind` tag) plus its spec, so the durable audit row
/// reconstructs which graph node ran without a schema change. The Trigger/Action
/// payload is inlined; Code/Condition carry their `runtime`/`source`.
#[must_use]
pub(crate) fn step_action_json(node: &Node) -> Value {
    let mut obj = Map::new();
    obj.insert("node".into(), Value::String(node.id.clone()));
    obj.insert("kind".into(), Value::String(node.kind.tag().to_string()));
    match &node.kind {
        NodeKind::Trigger { trigger } => {
            obj.insert(
                "trigger".into(),
                serde_json::to_value(trigger).unwrap_or(Value::Null),
            );
        }
        NodeKind::Action { action } => {
            obj.insert(
                "action".into(),
                serde_json::to_value(action).unwrap_or(Value::Null),
            );
        }
        NodeKind::Code { runtime, source } | NodeKind::Condition { runtime, source } => {
            obj.insert("runtime".into(), Value::String(runtime.clone()));
            obj.insert("source".into(), Value::String(source.clone()));
        }
        NodeKind::ForEach {
            source,
            item,
            index,
            max_iterations,
        } => {
            obj.insert("source".into(), Value::String(source.clone()));
            obj.insert("item".into(), Value::String(item.clone()));
            if let Some(i) = index {
                obj.insert("index".into(), Value::String(i.clone()));
            }
            if let Some(m) = max_iterations {
                obj.insert("max_iterations".into(), Value::from(*m));
            }
        }
        NodeKind::LoopEnd { for_each } => {
            obj.insert("for_each".into(), Value::String(for_each.clone()));
        }
    }
    Value::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph(v: Value) -> Graph {
        serde_json::from_value(v).expect("graph json")
    }

    #[test]
    fn nodes_and_edges_round_trip_with_defaults() {
        // Position defaults to origin; edge ports default to "".
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" },
                  "position": { "x": 10.0, "y": 20.0 } }
            ],
            "edges": [ { "from": "t", "to": "a" } ]
        }));
        assert_eq!(g.nodes.len(), 2);
        let t = g.node("t").unwrap();
        assert!(t.is_trigger());
        assert_eq!(t.position, Position::default());
        assert_eq!(g.node("a").unwrap().position, Position { x: 10.0, y: 20.0 });
        let e = &g.edges[0];
        assert_eq!(e.from_port, "");
        assert_eq!(e.to_port, "");
        assert_eq!(g.node(&e.from).unwrap().kind.tag(), "trigger");
        // Re-serializing keeps the flattened kind tag.
        let back = serde_json::to_value(&g).unwrap();
        assert_eq!(back["nodes"][0]["kind"], json!("trigger"));
        assert_eq!(back["nodes"][1]["kind"], json!("action"));
    }

    #[test]
    fn validate_accepts_a_linear_dag_and_topo_orders_it() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "b", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [ { "from": "t", "to": "a" }, { "from": "a", "to": "b" } ]
        }));
        assert!(g.validate().is_ok());
        assert_eq!(g.topo_order().unwrap(), vec!["t", "a", "b"]);
        // Upstream + out-edges helpers.
        assert_eq!(g.upstream("a"), vec!["t".to_string()]);
        assert_eq!(g.upstream("t"), Vec::<String>::new());
        assert_eq!(g.out_edges("t", None).count(), 1);
        assert_eq!(g.out_edges("t", Some("true")).count(), 0);
    }

    #[test]
    fn topo_orders_a_diamond_with_dependencies_before_dependents() {
        // t -> a, t -> b, a -> c, b -> c (a diamond): c must come last.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "b", "kind": "action", "action": { "kind": "notify" } },
                { "id": "c", "kind": "action", "action": { "kind": "create_note" } }
            ],
            "edges": [
                { "from": "t", "to": "a" }, { "from": "t", "to": "b" },
                { "from": "a", "to": "c" }, { "from": "b", "to": "c" }
            ]
        }));
        let order = g.topo_order().unwrap();
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("t") < pos("a") && pos("t") < pos("b"));
        assert!(pos("a") < pos("c") && pos("b") < pos("c"));
        assert_eq!(g.upstream("c").len(), 2);
    }

    #[test]
    fn a_cycle_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "b", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [
                { "from": "t", "to": "a" },
                { "from": "a", "to": "b" },
                { "from": "b", "to": "a" }
            ]
        }));
        assert_eq!(g.topo_order(), Err("cycle".to_string()));
        assert_eq!(g.validate(), Err("cycle".to_string()));
    }

    #[test]
    fn a_dangling_edge_endpoint_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } }
            ],
            "edges": [ { "from": "t", "to": "ghost" } ]
        }));
        assert_eq!(
            g.validate(),
            Err("edge to unknown node 'ghost'".to_string())
        );

        let g2 = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } }
            ],
            "edges": [ { "from": "ghost", "to": "t" } ]
        }));
        assert_eq!(
            g2.validate(),
            Err("edge from unknown node 'ghost'".to_string())
        );
    }

    #[test]
    fn an_edge_into_a_trigger_is_rejected() {
        // A trigger heads the graph; wiring an action back into it is meaningless.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [ { "from": "a", "to": "t" } ]
        }));
        assert_eq!(
            g.validate(),
            Err(
                "edge into trigger node 't' — a trigger heads the graph and cannot have an \
                 incoming edge"
                    .to_string()
            )
        );
    }

    #[test]
    fn condition_out_edge_on_a_non_branch_port_is_rejected() {
        // A condition routes only through "true"/"false"; the executor never
        // follows a condition edge on any other port, so it's a dead branch.
        // Default port ("") — an edge drawn without picking a branch.
        let default_port = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [ { "from": "t", "to": "c" }, { "from": "c", "to": "a" } ]
        }));
        assert_eq!(
            default_port.validate(),
            Err(
                "edge from condition node 'c' uses port '' — a condition routes only \
                 through its 'true'/'false' out-ports"
                    .to_string()
            )
        );
        // An arbitrary non-branch port is rejected the same way.
        let bogus_port = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [
                { "from": "t", "to": "c" },
                { "from": "c", "to": "a", "from_port": "maybe" }
            ]
        }));
        assert!(bogus_port.validate().is_err());
        // The valid true/false form still passes (guarding against over-rejection).
        let ok = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "y", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "n", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [
                { "from": "t", "to": "c" },
                { "from": "c", "to": "y", "from_port": "true" },
                { "from": "c", "to": "n", "from_port": "false" }
            ]
        }));
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn duplicate_node_ids_are_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "t", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": []
        }));
        assert_eq!(g.validate(), Err("duplicate node id 't'".to_string()));
    }

    #[test]
    fn a_graph_without_a_trigger_node_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": []
        }));
        assert_eq!(g.validate(), Err("graph has no trigger node".to_string()));
        assert!(g.trigger_specs().is_empty());
    }

    #[test]
    fn trigger_specs_clones_trigger_nodes_in_order() {
        let g = graph(json!({
            "nodes": [
                { "id": "t1", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/a" } },
                { "id": "x", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "t2", "kind": "trigger",
                  "trigger": { "kind": "task_moved", "board": "b", "to_column": "done" } }
            ],
            "edges": [ { "from": "t1", "to": "x" } ]
        }));
        let specs = g.trigger_specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].kind(), "webhook");
        assert_eq!(specs[1].kind(), "task_moved");
    }

    #[test]
    fn condition_branch_ports_filter_out_edges() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "yes", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "no", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [
                { "from": "t", "to": "c" },
                { "from": "c", "to": "yes", "from_port": "true" },
                { "from": "c", "to": "no", "from_port": "false" }
            ]
        }));
        assert!(g.validate().is_ok());
        let true_targets: Vec<&str> = g
            .out_edges("c", Some("true"))
            .map(|e| e.to.as_str())
            .collect();
        let false_targets: Vec<&str> = g
            .out_edges("c", Some("false"))
            .map(|e| e.to.as_str())
            .collect();
        assert_eq!(true_targets, vec!["yes"]);
        assert_eq!(false_targets, vec!["no"]);
        assert_eq!(g.out_edges("c", None).count(), 2);
        // The condition node carries runtime + source.
        match &g.node("c").unwrap().kind {
            NodeKind::Condition { runtime, source } => {
                assert_eq!(runtime, "js");
                assert_eq!(source, "true");
            }
            other => panic!("expected condition, got {other:?}"),
        }
    }

    #[test]
    fn from_spec_extracts_a_graph_under_the_graph_key_only() {
        // No spec / no graph key → None (executor runs the legacy linear loop).
        assert!(Graph::from_spec(None).is_none());
        assert!(Graph::from_spec(Some(&json!({ "other": 1 }))).is_none());
        assert!(Graph::from_spec(Some(&json!("scalar"))).is_none());
        // A graph key → Some(Ok(graph)).
        let spec = json!({ "graph": {
            "nodes": [ { "id": "t", "kind": "trigger",
                         "trigger": { "kind": "webhook", "path": "/h" } } ],
            "edges": []
        }});
        let g = Graph::from_spec(Some(&spec)).unwrap().unwrap();
        assert_eq!(g.nodes.len(), 1);
        // A malformed graph → Some(Err(..)) (fail loud, no silent fallback).
        let bad = json!({ "graph": { "nodes": [ { "id": "t", "kind": "nope" } ] } });
        assert!(Graph::from_spec(Some(&bad)).unwrap().is_err());
    }

    #[test]
    fn step_action_json_encodes_node_identity_and_spec() {
        let trig = Node {
            id: "t".into(),
            kind: NodeKind::Trigger {
                trigger: Trigger::Webhook { path: "/h".into() },
            },
            position: Position::default(),
        };
        let j = step_action_json(&trig);
        assert_eq!(j["node"], json!("t"));
        assert_eq!(j["kind"], json!("trigger"));
        assert_eq!(j["trigger"]["kind"], json!("webhook"));

        let code = Node {
            id: "c".into(),
            kind: NodeKind::Code {
                runtime: "js".into(),
                source: "x".into(),
            },
            position: Position::default(),
        };
        let j = step_action_json(&code);
        assert_eq!(j["kind"], json!("code"));
        assert_eq!(j["runtime"], json!("js"));
        assert_eq!(j["source"], json!("x"));
    }

    /// A canonical loop: trigger → source action → for_each → body → loop_end → done.
    fn loop_graph() -> Graph {
        graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "src", "kind": "action", "action": { "kind": "web_search" } },
                { "id": "fe", "kind": "for_each", "source": "inputs.src.results", "item": "article", "index": "i" },
                { "id": "body", "kind": "action", "action": { "kind": "sql_query" } },
                { "id": "end", "kind": "loop_end", "for_each": "fe" },
                { "id": "done", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [
                { "from": "t", "to": "src" },
                { "from": "src", "to": "fe" },
                { "from": "fe", "to": "body" },
                { "from": "body", "to": "end" },
                { "from": "end", "to": "done" }
            ]
        }))
    }

    #[test]
    fn for_each_region_validates_and_resolves() {
        let g = loop_graph();
        assert!(g.validate().is_ok(), "{:?}", g.validate());
        let regions = g.for_each_regions().unwrap();
        assert_eq!(regions.len(), 1);
        let r = &regions[0];
        assert_eq!(r.for_each, "fe");
        assert_eq!(r.loop_end, "end");
        assert_eq!(r.body, vec!["body".to_string()]);
        assert_eq!(r.source, "inputs.src.results");
        assert_eq!(r.item, "article");
        assert_eq!(r.index.as_deref(), Some("i"));
    }

    #[test]
    fn for_each_body_is_topo_ordered_and_multi_node() {
        // fe → a → b → end : body is [a, b] in order.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x" },
                { "id": "a", "kind": "action", "action": { "kind": "fetch_url" } },
                { "id": "b", "kind": "action", "action": { "kind": "sql_query" } },
                { "id": "end", "kind": "loop_end", "for_each": "fe" }
            ],
            "edges": [
                { "from": "t", "to": "fe" },
                { "from": "fe", "to": "a" },
                { "from": "a", "to": "b" },
                { "from": "b", "to": "end" }
            ]
        }));
        assert!(g.validate().is_ok(), "{:?}", g.validate());
        assert_eq!(
            g.for_each_regions().unwrap()[0].body,
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn for_each_without_loop_end_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x" },
                { "id": "body", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [ { "from": "t", "to": "fe" }, { "from": "fe", "to": "body" } ]
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn orphan_loop_end_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "end", "kind": "loop_end", "for_each": "ghost" }
            ],
            "edges": []
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn for_each_empty_body_is_rejected() {
        // fe directly to its loop_end: no body nodes between them.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x" },
                { "id": "end", "kind": "loop_end", "for_each": "fe" }
            ],
            "edges": [ { "from": "t", "to": "fe" }, { "from": "fe", "to": "end" } ]
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn for_each_body_edge_leaving_region_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x" },
                { "id": "body", "kind": "action", "action": { "kind": "notify" } },
                { "id": "end", "kind": "loop_end", "for_each": "fe" },
                { "id": "leak", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [
                { "from": "t", "to": "fe" },
                { "from": "fe", "to": "body" },
                { "from": "body", "to": "end" },
                { "from": "body", "to": "leak" }
            ]
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn for_each_item_equals_index_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x", "index": "x" },
                { "id": "body", "kind": "action", "action": { "kind": "notify" } },
                { "id": "end", "kind": "loop_end", "for_each": "fe" }
            ],
            "edges": [ { "from": "t", "to": "fe" }, { "from": "fe", "to": "body" }, { "from": "body", "to": "end" } ]
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn nested_for_each_is_rejected() {
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "fe", "kind": "for_each", "source": "trigger.items", "item": "x" },
                { "id": "fe2", "kind": "for_each", "source": "trigger.inner", "item": "y" },
                { "id": "body2", "kind": "action", "action": { "kind": "notify" } },
                { "id": "end2", "kind": "loop_end", "for_each": "fe2" },
                { "id": "end", "kind": "loop_end", "for_each": "fe" }
            ],
            "edges": [
                { "from": "t", "to": "fe" },
                { "from": "fe", "to": "fe2" },
                { "from": "fe2", "to": "body2" },
                { "from": "body2", "to": "end2" },
                { "from": "end2", "to": "end" }
            ]
        }));
        assert!(g.validate().is_err());
    }

    #[test]
    fn warnings_are_empty_for_a_wired_graph() {
        // trigger → action, fully connected: nothing to warn about.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [ { "from": "t", "to": "a" } ]
        }));
        assert!(g.validate().is_ok());
        assert!(g.warnings().is_empty(), "{:?}", g.warnings());
        // A canonical loop is clean too (body reachable, trigger wired).
        assert!(
            loop_graph().warnings().is_empty(),
            "{:?}",
            loop_graph().warnings()
        );
    }

    #[test]
    fn warnings_flag_a_node_not_connected_to_any_trigger() {
        // 't' fires but is wired to nothing; 'a' is an island no trigger reaches.
        // Both are legal to save (validate passes) but neither does useful work.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "a", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": []
        }));
        assert!(g.validate().is_ok(), "disconnected nodes still save");
        let w = g.warnings();
        assert!(
            w.iter().any(|m| m.contains("node 'a' is not connected")),
            "{w:?}"
        );
        assert!(
            w.iter()
                .any(|m| m.contains("trigger node 't' has no outgoing edges")),
            "{w:?}"
        );
    }

    #[test]
    fn warnings_flag_a_condition_missing_a_branch() {
        // The 'true' branch is wired, the 'false' branch is not — the false outcome
        // silently does nothing.
        let g = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "yes", "kind": "action", "action": { "kind": "summarize" } }
            ],
            "edges": [
                { "from": "t", "to": "c" },
                { "from": "c", "to": "yes", "from_port": "true" }
            ]
        }));
        assert!(g.validate().is_ok());
        let w = g.warnings();
        assert!(
            w.iter()
                .any(|m| m.contains("condition node 'c' has no 'false' branch")),
            "{w:?}"
        );
        assert!(
            !w.iter().any(|m| m.contains("no 'true' branch")),
            "the wired branch is not flagged: {w:?}"
        );

        // Both branches wired → no condition warning (guards against over-warning).
        let both = graph(json!({
            "nodes": [
                { "id": "t", "kind": "trigger", "trigger": { "kind": "webhook", "path": "/h" } },
                { "id": "c", "kind": "condition", "runtime": "js", "source": "true" },
                { "id": "yes", "kind": "action", "action": { "kind": "summarize" } },
                { "id": "no", "kind": "action", "action": { "kind": "notify" } }
            ],
            "edges": [
                { "from": "t", "to": "c" },
                { "from": "c", "to": "yes", "from_port": "true" },
                { "from": "c", "to": "no", "from_port": "false" }
            ]
        }));
        assert!(both.validate().is_ok());
        assert!(both.warnings().is_empty(), "{:?}", both.warnings());
    }

    #[test]
    fn for_each_step_action_json_round_trips() {
        let node = Node {
            id: "fe".into(),
            kind: NodeKind::ForEach {
                source: "inputs.src.results".into(),
                item: "article".into(),
                index: Some("i".into()),
                max_iterations: Some(50),
            },
            position: Position::default(),
        };
        let j = step_action_json(&node);
        assert_eq!(j["kind"], json!("for_each"));
        assert_eq!(j["source"], json!("inputs.src.results"));
        assert_eq!(j["item"], json!("article"));
        assert_eq!(j["index"], json!("i"));
        assert_eq!(j["max_iterations"], json!(50));
        // Round-trips through serde back into a ForEach node.
        let n2: Node = serde_json::from_value(json!({
            "id": "fe", "kind": "for_each", "source": "a.b", "item": "x"
        }))
        .unwrap();
        assert_eq!(n2.kind.tag(), "for_each");
    }
}
