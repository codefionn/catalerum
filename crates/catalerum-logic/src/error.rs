//! The single error type surfaced across the crate's public API.

/// A failure parsing, validating, or evaluating a graph-Datalog program.
///
/// [`Parse`](LogicError::Parse) and [`Validate`](LogicError::Validate) are
/// **authoring-time** errors (bad syntax, an unsafe/ill-formed program) — they are
/// deterministic in the source text and independent of any graph data, so both the
/// HTTP route and the `GraphQuery` trigger reject a bad program the same way.
/// [`Eval`](LogicError::Eval) is a **run-time** guard: a well-formed program that
/// nonetheless blows a resource cap (too many derived facts / iterations, or a
/// deadline) against a particular workspace's facts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LogicError {
    /// The source text is not a syntactically valid program.
    #[error("parse error: {0}")]
    Parse(String),
    /// The program parses but is not a safe, well-formed Datalog program
    /// (range-restriction, arity, reserved-name, unknown-predicate, or size).
    #[error("invalid query: {0}")]
    Validate(String),
    /// Evaluation exceeded a resource cap (derived-fact budget, iteration budget,
    /// or the wall-clock deadline).
    #[error("query evaluation aborted: {0}")]
    Eval(String),
}
