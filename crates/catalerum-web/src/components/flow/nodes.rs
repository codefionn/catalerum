use super::*;

/// Resolve an edge's two endpoint anchor points against the current graph, or `None`
/// if either node is gone (a stale edge mid-edit). A commit gate lands on the target
/// node's **bottom edge**, not its input port — the data edge already occupies the
/// input, and stacking both on one anchor drew two indistinguishable overlapping
/// arrows (it's a gate tapping the node, not a second input).
pub(super) fn edge_endpoints(g: &FlowGraph, e: &FlowEdge) -> Option<((f64, f64), (f64, f64))> {
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
pub(super) fn node_view(
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
pub(super) fn hit_to_flow_kind(hit: &NodeTypeHit) -> Option<(FlowKind, &'static str)> {
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
pub(super) fn node_kind_badge(hit: &NodeTypeHit) -> &'static str {
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
pub(super) fn kind_meta(kind: &FlowKind) -> (&'static str, &'static str, &'static str) {
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
pub(super) fn is_agent_action(action: &Value) -> bool {
    action.get("kind").and_then(Value::as_str) == Some("llm_agent")
}

/// Classifier nodes carry an outcomes list and use chat scores or native decisions.
pub(super) fn is_classifier(action: &Value) -> bool {
    matches!(
        action.get("kind").and_then(Value::as_str),
        Some("llm_agent" | "llm_decision")
    ) && action.get("outcomes").and_then(Value::as_array).is_some()
}

/// A one-line node subtitle for the box: the trigger/action kind, or the code
/// runtime — a glanceable summary of the node's payload.
pub(super) fn node_subtitle(n: &FlowNode) -> String {
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
