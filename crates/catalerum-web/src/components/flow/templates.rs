use super::*;

// --- defaults + menu data ---

/// The default trigger payload a fresh trigger node carries (a manual webhook).
pub(super) fn default_trigger() -> Value {
    serde_json::json!({ "kind": "webhook", "path": "/hook" })
}

/// The default action payload a fresh action node carries.
pub(super) fn default_action() -> Value {
    serde_json::json!({ "kind": "summarize" })
}

/// The default payload a fresh **Agent** node carries: a tool-calling `llm_agent`
/// seeded with chatbot-friendly guidance + the tools a channel bot wants (reply +
/// memory). Authored over the existing agent infrastructure.
pub(super) fn default_agent_action() -> Value {
    serde_json::json!({
        "kind": "llm_agent",
        "system": "You are a helpful assistant in a chat channel. Read the incoming message, use your tools when useful, and write a concise reply. Your response is delivered back to the user on their channel automatically.",
        "tools": ["recall", "remember"]
    })
}

pub(super) fn default_decision_action() -> Value {
    serde_json::json!({
        "kind": "llm_decision",
        "questions": {"urgent": {"type": "noul", "instructions": "Does this need an immediate response?"}}
    })
}

/// The default payload a fresh **Classifier** node carries: a JSON-steered
/// `llm_agent` that scores three common sentiment buckets — edit the outcomes to
/// fit the task.
pub(super) fn default_classifier_action() -> Value {
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
pub(super) type TemplateNode = (&'static str, FlowKind, f64, f64);

/// The **Channel chatbot** starter: a `channel_message` trigger wired into an Agent
/// node — drop it in, set the channel, and you have a tool-calling bot that replies
/// on the channel.
pub(super) fn chatbot_template() -> (Vec<TemplateNode>, Vec<(usize, usize, &'static str)>) {
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
pub(super) fn scheduled_template() -> (Vec<TemplateNode>, Vec<(usize, usize, &'static str)>) {
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
pub(super) fn instantiate_template(
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
pub(super) fn duplicate_node(g: FlowGraph, id: &str, counter: u64) -> (FlowGraph, u64, Option<String>) {
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
pub(super) const KNOWN_TOOLS: &[(&str, &[&str])] = &[
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
            "list_weekly_plans",
            "apply_weekly_plan",
            "unapply_weekly_plan",
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
pub(super) const TRIGGER_KINDS: &[(&str, &str)] = &[
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
pub(super) const ACTION_KINDS: &[(&str, &str)] = &[
    ("summarize", "Summarize"),
    ("create_chat_thread", "Create chat thread"),
    ("notify", "Notify"),
    ("llm_agent", "LLM agent"),
    ("llm_decision", "Decision model"),
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
pub(super) const RUNTIMES: &[(&str, &str)] = &[
    ("js", "JavaScript"),
    ("shell", "Shell"),
    ("python", "Python"),
];
