//! Static well-formedness checks over a parsed [`Program`] — the authoring-time
//! guard the HTTP route and the `GraphQuery` trigger both run. A program that
//! passes is **safe to evaluate**: every derived tuple is ground, predicates and
//! arities are consistent, no reserved name is redefined, and the program is
//! within size caps.
//!
//! Termination is *structural*, not checked here: the language has no function
//! symbols, so the active domain (constants in the facts ∪ constants in the
//! program) is finite, the Herbrand base is finite, and the monotone immediate-
//! consequence operator reaches its least fixpoint in finitely many steps
//! (see [`crate::eval`]). Range-restriction (checked here) is what guarantees each
//! step only ever derives ground facts.

use std::collections::{HashMap, HashSet};

use crate::ast::{split_body, Atom, Literal, Program, Term};
use crate::error::LogicError;
use crate::schema;

/// Max rules in one program.
const MAX_RULES: usize = 64;
/// Max literals in one rule/query body.
const MAX_BODY_LITERALS: usize = 32;
/// Max literals across the whole program.
const MAX_TOTAL_LITERALS: usize = 512;

fn verr(msg: impl Into<String>) -> LogicError {
    LogicError::Validate(msg.into())
}

/// Check `program` is a safe, well-formed Datalog program. Returns the first
/// violation found.
pub fn check(program: &Program) -> Result<(), LogicError> {
    check_size(program)?;
    check_reserved_heads(program)?;
    check_arities(program)?;
    check_known_predicates(program)?;
    check_range_restriction(program)?;
    Ok(())
}

fn check_size(program: &Program) -> Result<(), LogicError> {
    if program.rules.len() > MAX_RULES {
        return Err(verr(format!("too many rules (max {MAX_RULES})")));
    }
    let mut total = program.query.body.len();
    if program.query.body.len() > MAX_BODY_LITERALS {
        return Err(verr(format!(
            "query body too large (max {MAX_BODY_LITERALS} literals)"
        )));
    }
    for r in &program.rules {
        if r.body.len() > MAX_BODY_LITERALS {
            return Err(verr(format!(
                "rule '{}' body too large (max {MAX_BODY_LITERALS} literals)",
                r.head.pred
            )));
        }
        total += r.body.len();
    }
    if total > MAX_TOTAL_LITERALS {
        return Err(verr(format!(
            "program too large (max {MAX_TOTAL_LITERALS} literals)"
        )));
    }
    Ok(())
}

fn check_reserved_heads(program: &Program) -> Result<(), LogicError> {
    for r in &program.rules {
        if schema::is_reserved_pred(&r.head.pred) {
            return Err(verr(format!(
                "cannot define reserved predicate '{}' — it is a base relation, a \
                 label/edge shorthand, or a builtin",
                r.head.pred
            )));
        }
    }
    Ok(())
}

/// Every predicate has one arity across the whole program; base relations match
/// their fixed arity (`node/2`, `edge/3`, `prop/3`).
fn check_arities(program: &Program) -> Result<(), LogicError> {
    let mut all_atoms: Vec<&Atom> = Vec::new();
    for r in &program.rules {
        all_atoms.push(&r.head);
        all_atoms.extend(body_atoms(&r.body));
    }
    all_atoms.extend(body_atoms(&program.query.body));

    let mut seen: HashMap<String, usize> = HashMap::new();
    for atom in all_atoms {
        let arity = atom.args.len();
        if let Some(base) = schema::base_arity(&atom.pred) {
            if arity != base {
                return Err(verr(format!(
                    "'{}' takes {base} argument(s), got {arity}",
                    atom.pred
                )));
            }
        }
        match seen.get(&atom.pred) {
            Some(&prev) if prev != arity => {
                return Err(verr(format!(
                    "predicate '{}' used with {prev} and {arity} arguments",
                    atom.pred
                )));
            }
            _ => {
                seen.insert(atom.pred.clone(), arity);
            }
        }
    }
    Ok(())
}

/// Every body atom references a base relation or a defined rule head — a typo'd
/// predicate is rejected rather than silently deriving nothing.
fn check_known_predicates(program: &Program) -> Result<(), LogicError> {
    let mut known: HashSet<&str> = HashSet::new();
    known.insert("node");
    known.insert("edge");
    known.insert("prop");
    for r in &program.rules {
        known.insert(r.head.pred.as_str());
    }
    let check = |body: &[Literal], ctx: &str| -> Result<(), LogicError> {
        for a in body_atoms(body) {
            if !known.contains(a.pred.as_str()) {
                return Err(verr(format!(
                    "unknown predicate '{}' in {ctx} — not a base relation \
                     (node/edge/prop), a shorthand, or a defined rule head",
                    a.pred
                )));
            }
        }
        Ok(())
    };
    for r in &program.rules {
        check(&r.body, &format!("rule '{}'", r.head.pred))?;
    }
    check(&program.query.body, "the query")
}

/// Safety: every head variable and every comparison variable must be bound by a
/// positive body atom.
fn check_range_restriction(program: &Program) -> Result<(), LogicError> {
    for r in &program.rules {
        let bound = positive_body_vars(&r.body);
        for v in atom_vars(&r.head) {
            if !bound.contains(&v) {
                return Err(verr(format!(
                    "unsafe rule '{}': head variable '{v}' is not bound by any body atom",
                    r.head.pred
                )));
            }
        }
        check_cmp_vars(&r.body, &bound, &format!("rule '{}'", r.head.pred))?;
    }
    let bound = positive_body_vars(&program.query.body);
    check_cmp_vars(&program.query.body, &bound, "the query")
}

fn check_cmp_vars(body: &[Literal], bound: &HashSet<String>, ctx: &str) -> Result<(), LogicError> {
    let (_, cmps) = split_body(body);
    for (_, l, r) in cmps {
        for t in [l, r] {
            if let Term::Var(v) = t {
                if !bound.contains(v) {
                    return Err(verr(format!(
                        "unsafe comparison in {ctx}: variable '{v}' is not bound by any atom"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn body_atoms(body: &[Literal]) -> impl Iterator<Item = &Atom> {
    body.iter().filter_map(|l| match l {
        Literal::Atom { atom, .. } => Some(atom),
        Literal::Cmp { .. } => None,
    })
}

fn atom_vars(atom: &Atom) -> Vec<String> {
    atom.args
        .iter()
        .filter_map(|t| match t {
            Term::Var(v) => Some(v.clone()),
            Term::Const(_) => None,
        })
        .collect()
}

fn positive_body_vars(body: &[Literal]) -> HashSet<String> {
    let mut set = HashSet::new();
    for a in body_atoms(body) {
        for v in atom_vars(a) {
            set.insert(v);
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_program;

    fn ok(src: &str) {
        let p = parse_program(src).expect("parse");
        check(&p).expect("valid");
    }
    fn bad(src: &str) -> String {
        let p = parse_program(src).expect("parse");
        check(&p).expect_err("should be invalid").to_string()
    }

    #[test]
    fn accepts_recursive_and_joined_programs() {
        ok("reach(X, Y) :- relates_to(X, Y).\n\
            reach(X, Y) :- relates_to(X, Z), reach(Z, Y).\n\
            ?- reach(\"a\", Y), topic(Y).");
        ok(
            "result(M, T) :- topic(X), prop(X, \"display_name\", N), ieq(N, \"Planning\"),\n\
                            references(M, X), note(M), prop(M, \"title\", T).\n\
            ?- result(M, T).",
        );
    }

    #[test]
    fn rejects_reserved_head() {
        assert!(bad("note(X) :- node(X, \"Note\").\n?- note(X).").contains("reserved"));
    }

    #[test]
    fn rejects_unsafe_head_and_comparison() {
        assert!(bad("p(X, Y) :- node(X, \"Note\").\n?- p(X, Y).").contains("not bound"));
        assert!(bad("?- node(X, \"Note\"), Y > \"3\".").contains("not bound"));
    }

    #[test]
    fn rejects_unknown_predicate_and_arity() {
        assert!(bad("?- widget(X).").contains("unknown predicate"));
        assert!(bad("?- node(X).").contains("takes 2"));
        assert!(bad("p(X) :- node(X, \"Note\").\n?- p(X, Y).").contains("arguments"));
    }
}
