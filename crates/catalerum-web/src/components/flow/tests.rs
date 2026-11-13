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
fn decision_form_preserves_structured_questions_and_clears_optional_values() {
    let original = serde_json::json!({"kind":"llm_decision","model":"old","state":"old input","editor":"keep"});
    let questions = r#"{"team":{"type":"choice","instructions":"Which team?","criteria":{"engineering":"Bugs","support":"Usage"}},"severity":{"type":"score","instructions":"How severe?","criteria":["Low","High"]}}"#;
    let result = decision_params(
        &original,
        " new ",
        questions,
        r#"{"ticket":"{{ trigger.text }}"}"#,
    )
    .unwrap();
    assert_eq!(result["model"], "new");
    assert_eq!(result["state"]["ticket"], "{{ trigger.text }}");
    assert_eq!(result["questions"]["team"]["type"], "choice");
    assert_eq!(
        result["questions"]["severity"]["criteria"],
        serde_json::json!(["Low", "High"])
    );
    assert_eq!(result["editor"], "keep");
    let cleared = decision_params(&result, "", questions, "").unwrap();
    assert!(cleared.get("model").is_none());
    assert!(cleared.get("state").is_none());
    assert!(decision_params(&original, "", "[]", "").is_err());
    assert!(decision_params(&original, "", questions, "true").is_err());
    assert!(decision_params(&original, "", "{", "").is_err());
    assert_eq!(
        default_decision_action()["questions"]["urgent"]["type"],
        "noul"
    );
}

#[test]
fn classifier_decisions_preserve_input_and_round_trip_engine_changes() {
    let legacy = serde_json::json!({"kind":"llm_agent","outcomes":["urgent"],"instructions":"Read the email.","state":{"subject":"{{ trigger.subject }}"},"model":"chat-model"});
    let native = classifier_update(
        &legacy,
        &["urgent".into()],
        "Read the email.",
        "decisions",
        "  decision-model  ",
    );
    assert_eq!(native["kind"], "llm_decision");
    assert_eq!(native["model"], "decision-model");
    assert_eq!(native["state"], legacy["state"]);
    assert_eq!(native["questions"]["urgent"]["type"], "noul");
    assert!(native["questions"]["urgent"]["instructions"]
        .as_str()
        .unwrap()
        .contains("Read the email."));
    assert!(native.get("system").is_none());
    assert!(native.get("output").is_none());
    assert!(is_classifier(&native));
    assert!(!is_agent_action(&native));
    assert_eq!(
        classifier_form_from_params(&native),
        (vec!["urgent".into()], "Read the email.".into())
    );
    let chat = classifier_update(&native, &["urgent".into()], "Read the email.", "llm", "");
    assert_eq!(chat["kind"], "llm_agent");
    assert!(chat.get("questions").is_none());
    assert!(chat.get("model").is_none());
    assert_eq!(chat["state"], legacy["state"]);
    assert!(is_classifier(&chat));
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
