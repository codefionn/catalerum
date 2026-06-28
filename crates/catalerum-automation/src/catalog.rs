//! The **node-type catalog** (SOUL §11): authoritative, embeddable documentation
//! for every automation node type an author (a human in the visual editor, or an
//! LLM agent authoring a graph over the [`create_automation`]/[`update_automation`]
//! tools) can place in a [`Graph`](crate::graph::Graph).
//!
//! A node graph is built from four [`NodeKind`](crate::graph::NodeKind)s —
//! `trigger`, `action`, `code`, `condition` — and the `trigger`/`action` kinds each
//! fan out into a typed set ([`Trigger`](crate::Trigger)'s 9 kinds,
//! [`ActionKind`](crate::ActionKind)'s 32). Rather than make a caller infer each
//! one's fields from the source, this module ships a [`NodeDoc`] per node type:
//! a title, a one-line summary, a description written *for an author* (what it does,
//! when to use it, the gotchas), the typed [`NodeParam`]s, a ready-to-paste example
//! graph node, and intent keywords. The data is a curated JSON document compiled in
//! via [`include_str!`] and parsed once.
//!
//! It is **pure data** — no I/O, no engine state. The semantic search over these
//! docs (so an agent can find the node it needs by intent instead of reading all 39)
//! is the in-memory embedding index in `catalerum-api` (`node_index`), which embeds
//! each doc's [`NodeDoc::embed_text`]; this crate only owns the corpus.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One documented parameter of a node type — the shape a field takes in the node's
/// JSON payload (a trigger's fields, an action's `params`, or a code node's
/// `runtime`/`source`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeParam {
    /// The JSON key (e.g. `cron`, `title`, `to_column`).
    pub name: String,
    /// A loose type hint for authors: `string`, `integer`, `boolean`, `object`,
    /// `array`, `string[]`, … (intentionally not a strict JSON-Schema type).
    pub ty: String,
    /// Whether the field must be present for the node to be valid.
    pub required: bool,
    /// What the field means + how to fill it.
    pub description: String,
}

/// Documentation for one automation node type (a `trigger`/`action` of a given
/// `kind`, or the `code`/`condition` node kinds). Serialized verbatim to the REST
/// surface and embedded for semantic search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeDoc {
    /// Stable identifier: `"trigger.<kind>"`, `"action.<kind>"`, `"code"`, or
    /// `"condition"`. The key callers (search, the UI) address a node type by.
    pub id: String,
    /// The owning [`NodeKind`](crate::graph::NodeKind) tag: `trigger` / `action` /
    /// `code` / `condition`.
    pub node_kind: String,
    /// The inner kind tag for a `trigger`/`action` (e.g. `schedule`,
    /// `create_note`); empty for `code`/`condition`.
    pub kind: String,
    /// Human-readable label (e.g. "Cron schedule trigger").
    pub title: String,
    /// One-line summary.
    pub summary: String,
    /// A few sentences written for an author: what the node does, when to reach for
    /// it, and the gotchas.
    pub description: String,
    /// The node's typed parameters.
    pub params: Vec<NodeParam>,
    /// A ready-to-paste example **graph node** (`{id, kind, …, position}`) — a valid
    /// [`Node`](crate::graph::Node).
    pub example: Value,
    /// Intent/synonym words that broaden semantic-search recall.
    pub keywords: Vec<String>,
}

impl NodeDoc {
    /// The text embedded for semantic search: title, id, summary, description, and
    /// keywords carry complementary signal (the id/keywords help short intent
    /// queries; the description helps natural-language ones).
    #[must_use]
    pub fn embed_text(&self) -> String {
        format!(
            "{title} ({id}): {summary} {description} Keywords: {keywords}",
            title = self.title,
            id = self.id,
            summary = self.summary,
            description = self.description,
            keywords = self.keywords.join(", "),
        )
    }
}

/// The curated catalog, parsed once from the compiled-in JSON. A parse failure is a
/// build-time authoring bug in `catalog.json`, so it panics on first access (the
/// embedded document is not user input).
static CATALOG: LazyLock<Vec<NodeDoc>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("catalog.json"))
        .expect("embedded automation node catalog (catalog.json) must be valid JSON")
});

/// Every documented automation node type, in a stable display order (triggers,
/// then actions, then `code`/`condition`).
#[must_use]
pub fn catalog() -> &'static [NodeDoc] {
    &CATALOG
}

/// The doc for one node type by its [`NodeDoc::id`] (e.g. `"trigger.schedule"`,
/// `"action.create_note"`, `"code"`), if it exists.
#[must_use]
pub fn get(id: &str) -> Option<&'static NodeDoc> {
    CATALOG.iter().find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;
    use crate::{Action, Trigger};
    use std::collections::HashSet;

    /// The trigger kinds the catalog must cover — kept in lockstep with
    /// [`Trigger::kind`]; a new trigger variant without a doc trips this test.
    const TRIGGER_KINDS: &[&str] = &[
        "calendar_event",
        "storage_object",
        "schedule",
        "webhook",
        "graph_query",
        "channel_message",
        "collect_email",
        "collect_calendar",
        "collect_sql",
        "task_moved",
        "trigger",
    ];

    /// The action kinds the catalog must cover — kept in lockstep with
    /// [`crate::ActionKind`]; a new action variant without a doc trips this test.
    const ACTION_KINDS: &[&str] = &[
        "llm_agent",
        "run_profile",
        "run_skill",
        "create_event",
        "update_event",
        "write_email",
        "write_event",
        "label_email",
        "mark_email_read",
        "move_object",
        "write_object",
        "run_command",
        "open_terminal",
        "terminal_write",
        "terminal_read",
        "persist_terminal",
        "close_terminal",
        "create_note",
        "edit_note",
        "create_task",
        "move_task",
        "create_chat_thread",
        "notify",
        "summarize",
        "index_document",
        "reindex_objects",
        "fetch_url",
        "web_search",
        "html_to_markdown",
        "extract_html",
        "sql_query",
        "webhook",
    ];

    #[test]
    fn catalog_parses_and_ids_are_unique() {
        let docs = catalog();
        assert!(!docs.is_empty(), "catalog must not be empty");
        let ids: HashSet<&str> = docs.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids.len(), docs.len(), "node-type ids must be unique");
        // 10 triggers + 32 actions + code + condition + for_each + loop_end.
        assert_eq!(docs.len(), TRIGGER_KINDS.len() + ACTION_KINDS.len() + 4);
    }

    #[test]
    fn covers_every_trigger_and_action_kind_plus_code_and_condition() {
        let docs = catalog();
        let by_id = |id: &str| docs.iter().any(|d| d.id == id);
        for k in TRIGGER_KINDS {
            assert!(by_id(&format!("trigger.{k}")), "missing trigger doc: {k}");
        }
        for k in ACTION_KINDS {
            assert!(by_id(&format!("action.{k}")), "missing action doc: {k}");
        }
        assert!(by_id("code"), "missing code node doc");
        assert!(by_id("condition"), "missing condition node doc");
        assert!(by_id("for_each"), "missing for_each node doc");
        assert!(by_id("loop_end"), "missing loop_end node doc");

        // …and no doc for a kind that doesn't exist (keeps the catalog honest).
        for d in docs {
            match d.node_kind.as_str() {
                "trigger" => assert!(
                    TRIGGER_KINDS.contains(&d.kind.as_str()),
                    "stray trigger {}",
                    d.id
                ),
                "action" => assert!(
                    ACTION_KINDS.contains(&d.kind.as_str()),
                    "stray action {}",
                    d.id
                ),
                "code" | "condition" | "for_each" | "loop_end" => assert!(d.kind.is_empty()),
                other => panic!("unknown node_kind {other} in {}", d.id),
            }
        }
    }

    #[test]
    fn every_example_is_a_valid_graph_node_matching_its_doc() {
        for d in catalog() {
            // The example must round-trip into a real graph Node…
            let node: Node = serde_json::from_value(d.example.clone())
                .unwrap_or_else(|e| panic!("{}: example is not a valid graph Node: {e}", d.id));
            // …whose NodeKind tag matches the doc's node_kind…
            assert_eq!(node.kind.tag(), d.node_kind, "{}: node_kind mismatch", d.id);
            // …and whose inner trigger/action kind matches the doc's kind.
            match &node.kind {
                crate::graph::NodeKind::Trigger { trigger } => {
                    assert_eq!(trigger.kind(), d.kind, "{}: trigger kind mismatch", d.id);
                }
                crate::graph::NodeKind::Action { action } => {
                    let kind = serde_json::to_value(action.kind).ok();
                    let kind = kind.as_ref().and_then(Value::as_str).unwrap_or_default();
                    assert_eq!(kind, d.kind, "{}: action kind mismatch", d.id);
                }
                _ => assert!(d.kind.is_empty()),
            }
        }
    }

    #[test]
    fn trigger_and_action_examples_parse_into_their_typed_specs() {
        // The example payloads aren't just shaped right — they're valid typed specs
        // the engine accepts (so an author can paste one and it persists).
        for d in catalog() {
            match d.node_kind.as_str() {
                "trigger" => {
                    let t = &d.example["trigger"];
                    serde_json::from_value::<Trigger>(t.clone())
                        .unwrap_or_else(|e| panic!("{}: bad Trigger example: {e}", d.id));
                }
                "action" => {
                    let a = &d.example["action"];
                    serde_json::from_value::<Action>(a.clone())
                        .unwrap_or_else(|e| panic!("{}: bad Action example: {e}", d.id));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn embed_text_includes_identifying_signal() {
        let sched = get("trigger.schedule").expect("schedule doc");
        let text = sched.embed_text();
        assert!(text.contains("trigger.schedule"));
        assert!(text.contains(&sched.title));
        assert!(text.to_lowercase().contains("cron"));
    }

    #[test]
    fn get_resolves_and_rejects() {
        assert!(get("action.create_note").is_some());
        assert!(get("code").is_some());
        assert!(get("nope.nope").is_none());
    }
}
