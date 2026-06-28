//! catalerum-logic — a small, safe **Datalog** query language over the derived
//! graph (SOUL §6.3/§18).
//!
//! This crate is the operator-facing query surface that replaces raw Cypher. It is
//! **store-agnostic and pure**: it parses a program, checks it is well-formed, and
//! evaluates it against an in-memory [`Facts`] set that a caller loads for exactly
//! one workspace. Because the language has **no way to name a workspace, no writes,
//! and no I/O**, and evaluation runs entirely in-process over the caller's facts,
//! cross-workspace reach, mutation, and injection are impossible *by construction* —
//! not by heuristic inspection of query text.
//!
//! # Language at a glance
//! Base relations: `node(Id, Label)`, `edge(From, EdgeType, To)`, `prop(Id, Key,
//! Value)`. Shorthand desugars from the closed §6.3 taxonomy: `note(X)` ≡
//! `node(X, "Note")`, `references(F, T)` ≡ `edge(F, "REFERENCES", T)`. Rules
//! `head :- body.`, one goal `?- body.`, comparisons `= != < <= > >=` plus
//! `ieq`/`like` (case-insensitive equality / substring). Output columns are the
//! goal's distinct variables; rows are set-deduped.
//!
//! ```
//! use catalerum_logic::{parse, eval, Facts, EvalLimits};
//! let program = parse(r#"?- note(N), prop(N, "title", T)."#).unwrap();
//! let mut facts = Facts::new();
//! facts.node("n1", "Note");
//! facts.prop("n1", "title", "Hello");
//! let out = eval(&program, &facts, &EvalLimits::default()).unwrap();
//! assert_eq!(out.columns, vec!["N", "T"]);
//! assert_eq!(out.rows.len(), 1);
//! ```
//!
//! # Termination
//! No function symbols ⇒ the active domain is finite ⇒ the least fixpoint is
//! reached in finitely many semi-naive rounds. The [`EvalLimits`] caps are
//! defense-in-depth, not the correctness guarantee.

#![forbid(unsafe_code)]

pub mod ast;
mod error;
mod eval;
mod lex;
mod parse;
pub mod schema;
mod validate;

pub use ast::Program;
pub use error::LogicError;
pub use eval::{eval, EvalLimits, EvalOutput, Facts};
pub use schema::{EDGE_TYPES, LABELS};

/// Parse and validate `src` into a ready-to-[`eval`] [`Program`]. This is the
/// single entry point callers use: it runs the lexer, parser (with desugaring),
/// and every static safety check, so the returned program is safe to evaluate.
///
/// # Errors
/// [`LogicError::Parse`] for a syntax error, [`LogicError::Validate`] for an
/// ill-formed or unsafe program.
pub fn parse(src: &str) -> Result<Program, LogicError> {
    let program = parse::parse_program(src)?;
    validate::check(&program)?;
    Ok(program)
}

/// Authoring-time check that `src` is a valid, safe program — the guard the HTTP
/// route and the `GraphQuery` trigger run before storing/executing a query. Equal
/// to [`parse`] discarding the program.
///
/// # Errors
/// As [`parse`].
pub fn validate(src: &str) -> Result<(), LogicError> {
    parse(src).map(|_| ())
}
