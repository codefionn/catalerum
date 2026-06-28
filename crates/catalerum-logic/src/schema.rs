//! The closed graph vocabulary the language exposes (SOUL §6.3).
//!
//! These MUST stay in lockstep with `catalerum_graph::NodeLabel::as_cypher()` /
//! `EdgeType::as_cypher()` — the fact loader emits exactly these strings
//! (`labels(n)` and `type(r)`), and the label/edge **sugar** predicates desugar to
//! them. `catalerum-graph` owns a drift-guard test that asserts the two tables
//! agree variant-for-variant, since it can import both crates.

/// The 16 node-label strings, exactly as Neo4j stores them (`labels(n)` values).
pub const LABELS: [&str; 16] = [
    "Person",
    "Org",
    "Topic",
    "Project",
    "Place",
    "Event",
    "File",
    "Note",
    "Task",
    "Conversation",
    "Calendar",
    "Bucket",
    "Email",
    "Memory",
    "Document",
    "Message",
];

/// The 9 relationship-type strings, exactly as Neo4j stores them (`type(r)`,
/// UPPER_SNAKE).
pub const EDGE_TYPES: [&str; 9] = [
    "ATTENDS",
    "ABOUT",
    "MENTIONS",
    "STORED_IN",
    "SCHEDULED_IN",
    "FOLLOWS",
    "RELATES_TO",
    "DERIVED_FROM",
    "REFERENCES",
];

/// The three base (extensional) relations — the only relations the fact loader
/// materializes, and the only predicate names a body atom may reference besides a
/// rule head. `(name, arity)`.
pub const BASE_RELATIONS: [(&str, usize); 3] = [("node", 2), ("edge", 3), ("prop", 3)];

/// The two comparison builtins written in atom form (`ieq(A,B)` / `like(A,B)`).
/// Recognised by the parser and rewritten into comparison literals — never real
/// predicates.
pub const BUILTIN_PREDS: [&str; 2] = ["ieq", "like"];

/// The label string a unary label-sugar predicate desugars to, e.g.
/// `note` → `"Note"`. `None` if `name` is not one of the 12 label sugars.
#[must_use]
pub fn label_sugar(name: &str) -> Option<&'static str> {
    LABELS
        .iter()
        .copied()
        .find(|l| l.eq_ignore_ascii_case(name))
}

/// The relationship-type string a binary edge-sugar predicate desugars to, e.g.
/// `references` → `"REFERENCES"`, `scheduled_in` → `"SCHEDULED_IN"`. `None` if
/// `name` is not one of the 9 edge sugars. Matching folds the UPPER_SNAKE edge
/// type to the lower_snake sugar name.
#[must_use]
pub fn edge_sugar(name: &str) -> Option<&'static str> {
    EDGE_TYPES
        .iter()
        .copied()
        .find(|e| e.eq_ignore_ascii_case(name))
}

/// Whether `name` is a reserved predicate name (a base relation, a label/edge
/// sugar, or a comparison builtin) — none of which may appear as a **rule head**.
#[must_use]
pub fn is_reserved_pred(name: &str) -> bool {
    BASE_RELATIONS.iter().any(|(n, _)| *n == name)
        || BUILTIN_PREDS.contains(&name)
        || label_sugar(name).is_some()
        || edge_sugar(name).is_some()
}

/// The fixed arity of a base relation, if `name` is one.
#[must_use]
pub fn base_arity(name: &str) -> Option<usize> {
    BASE_RELATIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, a)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sugar_maps_are_case_insensitive_and_closed() {
        assert_eq!(label_sugar("note"), Some("Note"));
        assert_eq!(label_sugar("Topic"), Some("Topic"));
        assert_eq!(label_sugar("widget"), None);
        assert_eq!(edge_sugar("references"), Some("REFERENCES"));
        assert_eq!(edge_sugar("scheduled_in"), Some("SCHEDULED_IN"));
        assert_eq!(edge_sugar("owns"), None);
    }

    #[test]
    fn reserved_covers_base_sugar_and_builtins() {
        for (n, _) in BASE_RELATIONS {
            assert!(is_reserved_pred(n));
        }
        assert!(is_reserved_pred("note"));
        assert!(is_reserved_pred("references"));
        assert!(is_reserved_pred("ieq"));
        assert!(!is_reserved_pred("result"));
        assert_eq!(base_arity("edge"), Some(3));
        assert_eq!(base_arity("result"), None);
    }
}
