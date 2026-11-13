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

mod model;
mod nodes;
mod panel;
mod actions;
mod agents;
mod mutate;
mod templates;
pub use model::*;
use nodes::*;
use panel::*;
use actions::*;
use agents::*;
use mutate::*;
use templates::*;

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

// ---------------------------------------------------------------------------
// Tests — the pure core.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
