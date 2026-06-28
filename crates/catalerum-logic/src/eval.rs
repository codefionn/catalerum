//! The bottom-up **semi-naive** Datalog evaluator over an in-memory EDB.
//!
//! A [`Facts`] set holds the three base relations for exactly one workspace (the
//! fact loader guarantees the scope; there is no way to name a workspace in the
//! language). [`eval`] computes the least fixpoint of the program's rules, then
//! evaluates the query goal over the result and projects it to the declared
//! columns (set-deduped). It is pure and synchronous — async callers wrap it in
//! `spawn_blocking` + a `timeout`, and may also pass a [`deadline`](EvalLimits::deadline).
//!
//! Termination is structural (no function symbols ⇒ finite active domain), so the
//! [`EvalLimits`] caps are defense-in-depth against pathological programs (a
//! Cartesian goal, a rule explosion), not the correctness guarantee.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde_json::Value as Json;

use crate::ast::{split_body, Atom, CmpOp, Program, Term, Value};
use crate::error::LogicError;

/// The extensional (base) fact set for one workspace: `node`/`edge`/`prop`
/// relations, each a set of ground tuples.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    rels: HashMap<String, HashSet<Vec<Value>>>,
}

impl Facts {
    /// An empty fact set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a raw tuple into relation `pred` (used by the loader-side adapter).
    pub fn add(&mut self, pred: &str, tuple: Vec<Value>) {
        self.rels.entry(pred.to_string()).or_default().insert(tuple);
    }

    /// Add a `node(id, label)` fact.
    pub fn node(&mut self, id: impl Into<String>, label: impl Into<String>) {
        self.add("node", vec![Value(id.into()), Value(label.into())]);
    }

    /// Add an `edge(from, edge_type, to)` fact.
    pub fn edge(
        &mut self,
        from: impl Into<String>,
        edge_type: impl Into<String>,
        to: impl Into<String>,
    ) {
        self.add(
            "edge",
            vec![
                Value(from.into()),
                Value(edge_type.into()),
                Value(to.into()),
            ],
        );
    }

    /// Add a `prop(id, key, value)` fact.
    pub fn prop(
        &mut self,
        id: impl Into<String>,
        key: impl Into<String>,
        value: impl Into<String>,
    ) {
        self.add(
            "prop",
            vec![Value(id.into()), Value(key.into()), Value(value.into())],
        );
    }

    /// The relation named `pred`, if any.
    #[must_use]
    fn get(&self, pred: &str) -> Option<&HashSet<Vec<Value>>> {
        self.rels.get(pred)
    }

    /// Total number of base facts across all relations.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rels.values().map(HashSet::len).sum()
    }

    /// Whether the fact set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rels.values().all(HashSet::is_empty)
    }
}

/// Resource caps for one evaluation — backstops, since termination is structural.
#[derive(Clone, Debug)]
pub struct EvalLimits {
    /// Reject if the loaded EDB is larger than this.
    pub max_facts: usize,
    /// Cap on derived IDB facts and on any intermediate join size (guards a
    /// Cartesian goal or a rule explosion).
    pub max_derived: usize,
    /// Cap on semi-naive fixpoint rounds.
    pub max_iterations: usize,
    /// Optional wall-clock deadline, checked each round.
    pub deadline: Option<Instant>,
}

impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            max_facts: 1_000_000,
            max_derived: 500_000,
            max_iterations: 1_000,
            deadline: None,
        }
    }
}

impl EvalLimits {
    /// The default limits with a wall-clock `deadline` set `after` from now.
    #[must_use]
    pub fn with_deadline(after: std::time::Duration) -> Self {
        Self {
            deadline: Some(Instant::now() + after),
            ..Self::default()
        }
    }
}

/// The result of a query: named columns and set-deduped rows (each cell a JSON
/// string — the single string value domain). Maps 1:1 onto the graph route's
/// `GraphQueryResponse`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvalOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
}

/// A partial variable→value assignment during a join.
type Binding = HashMap<String, Value>;

/// Evaluate `program` against `facts` under `limits`. Assumes `program` has passed
/// [`crate::validate::check`] (so every derivation is ground and predicates/arities
/// are consistent).
pub fn eval(
    program: &Program,
    facts: &Facts,
    limits: &EvalLimits,
) -> Result<EvalOutput, LogicError> {
    if facts.len() > limits.max_facts {
        return Err(LogicError::Eval(format!(
            "workspace has too many facts ({} > cap {})",
            facts.len(),
            limits.max_facts
        )));
    }

    let head_preds: HashSet<String> = program.rules.iter().map(|r| r.head.pred.clone()).collect();
    let mut idb: HashMap<String, HashSet<Vec<Value>>> = head_preds
        .iter()
        .map(|p| (p.clone(), HashSet::new()))
        .collect();
    let mut derived = 0usize;

    // Seed round: evaluate every rule against the full relations (fires the base
    // cases; recursive rules find nothing yet since their IDB atoms are empty).
    let mut delta: HashMap<String, HashSet<Vec<Value>>> = head_preds
        .iter()
        .map(|p| (p.clone(), HashSet::new()))
        .collect();
    for rule in &program.rules {
        let tuples = derive_full(rule, facts, &idb, limits)?;
        for t in tuples {
            add_tuple(
                &mut idb,
                &mut delta,
                &rule.head.pred,
                t,
                &mut derived,
                limits,
            )?;
        }
    }

    // Semi-naive rounds: each round joins one IDB body atom against the previous
    // round's delta (the rest full), until no new fact is derived.
    let mut iterations = 0usize;
    while !delta.values().all(HashSet::is_empty) {
        iterations += 1;
        if iterations > limits.max_iterations {
            return Err(LogicError::Eval(format!(
                "exceeded iteration budget ({})",
                limits.max_iterations
            )));
        }
        check_deadline(limits)?;
        let mut next: HashMap<String, HashSet<Vec<Value>>> = head_preds
            .iter()
            .map(|p| (p.clone(), HashSet::new()))
            .collect();
        for rule in &program.rules {
            let tuples = derive_delta(rule, facts, &idb, &delta, limits)?;
            for t in tuples {
                add_tuple(
                    &mut idb,
                    &mut next,
                    &rule.head.pred,
                    t,
                    &mut derived,
                    limits,
                )?;
            }
        }
        delta = next;
    }

    // Evaluate the query goal over the final relations and project to columns.
    let (atoms, cmps) = split_body(&program.query.body);
    let bindings = join_atoms(&atoms, limits, |_, pred| rel_full(pred, facts, &idb))?;
    let mut seen: HashSet<Vec<Value>> = HashSet::new();
    let mut rows: Vec<Vec<Json>> = Vec::new();
    for b in bindings {
        if !cmps_hold(&cmps, &b) {
            continue;
        }
        let key: Vec<Value> = program
            .query
            .columns
            .iter()
            .map(|c| b.get(c).cloned().unwrap_or_else(|| Value(String::new())))
            .collect();
        if seen.insert(key.clone()) {
            rows.push(key.into_iter().map(|v| Json::String(v.0)).collect());
        }
    }
    Ok(EvalOutput {
        columns: program.query.columns.clone(),
        rows,
    })
}

/// Look up a predicate's full relation: IDB first (rule heads), else the EDB. The
/// two are disjoint (a head can never be a reserved base name).
fn rel_full<'a>(
    pred: &str,
    edb: &'a Facts,
    idb: &'a HashMap<String, HashSet<Vec<Value>>>,
) -> Option<&'a HashSet<Vec<Value>>> {
    idb.get(pred).or_else(|| edb.get(pred))
}

/// All satisfying head tuples of `rule` using full relations (seed round).
fn derive_full(
    rule: &crate::ast::Rule,
    edb: &Facts,
    idb: &HashMap<String, HashSet<Vec<Value>>>,
    limits: &EvalLimits,
) -> Result<Vec<Vec<Value>>, LogicError> {
    let (atoms, cmps) = split_body(&rule.body);
    let bindings = join_atoms(&atoms, limits, |_, pred| rel_full(pred, edb, idb))?;
    Ok(head_tuples(&rule.head, &bindings, &cmps))
}

/// Semi-naive: head tuples of `rule` where at least one IDB body atom draws from
/// `delta` (summed over each IDB atom position; the rest use the full relations).
/// A pure-EDB rule contributes nothing here (it was fully computed at seed).
fn derive_delta(
    rule: &crate::ast::Rule,
    edb: &Facts,
    idb: &HashMap<String, HashSet<Vec<Value>>>,
    delta: &HashMap<String, HashSet<Vec<Value>>>,
    limits: &EvalLimits,
) -> Result<Vec<Vec<Value>>, LogicError> {
    let (atoms, cmps) = split_body(&rule.body);
    let idb_positions: Vec<usize> = atoms
        .iter()
        .enumerate()
        .filter(|(_, a)| idb.contains_key(a.pred.as_str()))
        .map(|(i, _)| i)
        .collect();
    let mut out = Vec::new();
    for &j in &idb_positions {
        let bindings = join_atoms(&atoms, limits, |i, pred| {
            if i == j {
                delta.get(pred)
            } else {
                rel_full(pred, edb, idb)
            }
        })?;
        out.extend(head_tuples(&rule.head, &bindings, &cmps));
    }
    Ok(out)
}

/// Filter `bindings` by the comparisons, then project each to a head tuple.
fn head_tuples(
    head: &Atom,
    bindings: &[Binding],
    cmps: &[(&CmpOp, &Term, &Term)],
) -> Vec<Vec<Value>> {
    bindings
        .iter()
        .filter(|b| cmps_hold(cmps, b))
        .map(|b| {
            head.args
                .iter()
                .map(|t| match t {
                    Term::Const(c) => c.clone(),
                    // Bound by range-restriction (validated); default guards misuse.
                    Term::Var(v) => b.get(v).cloned().unwrap_or_else(|| Value(String::new())),
                })
                .collect()
        })
        .collect()
}

/// Nested-loop join of positive `atoms`. `rel_at(i, pred)` supplies the relation
/// for the i-th atom (the semi-naive delta override lives here). Bounded by
/// `limits.max_derived` so a Cartesian join errors instead of exhausting memory.
fn join_atoms<'a, F>(
    atoms: &[&Atom],
    limits: &EvalLimits,
    rel_at: F,
) -> Result<Vec<Binding>, LogicError>
where
    F: Fn(usize, &str) -> Option<&'a HashSet<Vec<Value>>>,
{
    let mut bindings = vec![Binding::new()];
    for (i, atom) in atoms.iter().enumerate() {
        let Some(rel) = rel_at(i, atom.pred.as_str()) else {
            return Ok(Vec::new()); // an empty relation kills the whole conjunction
        };
        let mut next = Vec::new();
        for b in &bindings {
            for tuple in rel {
                if let Some(nb) = unify(&atom.args, tuple, b) {
                    next.push(nb);
                    if next.len() > limits.max_derived {
                        return Err(LogicError::Eval(format!(
                            "join too large (> {} intermediate rows) — narrow the query",
                            limits.max_derived
                        )));
                    }
                }
            }
        }
        bindings = next;
        if bindings.is_empty() {
            break;
        }
    }
    Ok(bindings)
}

/// Unify an atom's `args` against a `tuple` under `base`, returning the extended
/// binding or `None` on a clash.
fn unify(args: &[Term], tuple: &[Value], base: &Binding) -> Option<Binding> {
    if args.len() != tuple.len() {
        return None;
    }
    let mut b = base.clone();
    for (arg, val) in args.iter().zip(tuple) {
        match arg {
            Term::Const(c) => {
                if c != val {
                    return None;
                }
            }
            Term::Var(v) => match b.get(v) {
                Some(existing) if existing != val => return None,
                Some(_) => {}
                None => {
                    b.insert(v.clone(), val.clone());
                }
            },
        }
    }
    Some(b)
}

fn cmps_hold(cmps: &[(&CmpOp, &Term, &Term)], b: &Binding) -> bool {
    cmps.iter()
        .all(|(op, l, r)| sat_cmp(**op, resolve(l, b), resolve(r, b)))
}

fn resolve<'a>(t: &'a Term, b: &'a Binding) -> Option<&'a Value> {
    match t {
        Term::Const(c) => Some(c),
        Term::Var(v) => b.get(v),
    }
}

fn sat_cmp(op: CmpOp, l: Option<&Value>, r: Option<&Value>) -> bool {
    let (Some(l), Some(r)) = (l, r) else {
        return false;
    };
    match op {
        CmpOp::Eq => l == r,
        CmpOp::Ne => l != r,
        CmpOp::IEq => l.0.eq_ignore_ascii_case(&r.0),
        CmpOp::Like => l.0.to_lowercase().contains(&r.0.to_lowercase()),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
            let ord = compare_ord(l, r);
            match op {
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::Le => ord != std::cmp::Ordering::Greater,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                CmpOp::Ge => ord != std::cmp::Ordering::Less,
                _ => unreachable!(),
            }
        }
    }
}

/// Compare two values numerically when both parse as numbers, else lexically.
fn compare_ord(l: &Value, r: &Value) -> std::cmp::Ordering {
    if let (Ok(a), Ok(b)) = (l.0.parse::<f64>(), r.0.parse::<f64>()) {
        a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
    } else {
        l.0.cmp(&r.0)
    }
}

fn add_tuple(
    idb: &mut HashMap<String, HashSet<Vec<Value>>>,
    delta: &mut HashMap<String, HashSet<Vec<Value>>>,
    pred: &str,
    tuple: Vec<Value>,
    derived: &mut usize,
    limits: &EvalLimits,
) -> Result<(), LogicError> {
    let set = idb.get_mut(pred).expect("head predicate present in idb");
    if set.insert(tuple.clone()) {
        *derived += 1;
        if *derived > limits.max_derived {
            return Err(LogicError::Eval(format!(
                "exceeded derived-fact budget ({})",
                limits.max_derived
            )));
        }
        delta
            .get_mut(pred)
            .expect("head predicate present in delta")
            .insert(tuple);
    }
    Ok(())
}

fn check_deadline(limits: &EvalLimits) -> Result<(), LogicError> {
    if let Some(dl) = limits.deadline {
        if Instant::now() >= dl {
            return Err(LogicError::Eval("evaluation deadline exceeded".to_string()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_program;

    fn run(src: &str, facts: &Facts) -> EvalOutput {
        let p = parse_program(src).expect("parse");
        crate::validate::check(&p).expect("valid");
        eval(&p, facts, &EvalLimits::default()).expect("eval")
    }

    fn sorted(mut out: EvalOutput) -> Vec<Vec<String>> {
        let mut rows: Vec<Vec<String>> = out
            .rows
            .drain(..)
            .map(|r| {
                r.into_iter()
                    .map(|c| c.as_str().unwrap().to_string())
                    .collect()
            })
            .collect();
        rows.sort();
        rows
    }

    fn graph() -> Facts {
        // n1 REFERENCES topics t_plan, t_budget; n2 REFERENCES t_plan.
        let mut f = Facts::new();
        f.node("n1", "Note");
        f.node("n2", "Note");
        f.node("t_plan", "Topic");
        f.node("t_budget", "Topic");
        f.prop("n1", "title", "Sprint plan");
        f.prop("n2", "title", "Roadmap");
        f.prop("t_plan", "display_name", "Planning");
        f.prop("t_budget", "display_name", "Budget");
        f.edge("n1", "REFERENCES", "t_plan");
        f.edge("n1", "REFERENCES", "t_budget");
        f.edge("n2", "REFERENCES", "t_plan");
        f
    }

    #[test]
    fn notes_by_topic_case_insensitive() {
        let out = run(
            r#"result(M, T) :- topic(X), prop(X, "display_name", N), ieq(N, "planning"),
                             references(M, X), note(M), prop(M, "title", T).
               ?- result(M, T)."#,
            &graph(),
        );
        assert_eq!(out.columns, vec!["M", "T"]);
        assert_eq!(
            sorted(out),
            vec![
                vec!["n1".to_string(), "Sprint plan".to_string()],
                vec!["n2".to_string(), "Roadmap".to_string()],
            ]
        );
    }

    #[test]
    fn set_semantics_dedup() {
        // Two topics shared paths could yield dup M; output is deduped.
        let out = run("?- references(M, _), note(M).", &graph());
        assert_eq!(
            sorted(out),
            vec![vec!["n1".to_string()], vec!["n2".to_string()]]
        );
    }

    #[test]
    fn recursion_transitive_closure_terminates() {
        // A chain a→b→c→d over RELATES_TO; reachability is the closure.
        let mut f = Facts::new();
        for id in ["a", "b", "c", "d"] {
            f.node(id, "Topic");
        }
        f.edge("a", "RELATES_TO", "b");
        f.edge("b", "RELATES_TO", "c");
        f.edge("c", "RELATES_TO", "d");
        let out = run(
            "reach(X, Y) :- relates_to(X, Y).\n\
             reach(X, Y) :- relates_to(X, Z), reach(Z, Y).\n\
             ?- reach(\"a\", Y).",
            &f,
        );
        assert_eq!(
            sorted(out),
            vec![
                vec!["b".to_string()],
                vec!["c".to_string()],
                vec!["d".to_string()]
            ]
        );
    }

    #[test]
    fn comparison_ne_and_numeric() {
        let mut f = Facts::new();
        f.node("n1", "Note");
        f.node("n2", "Note");
        f.prop("n1", "rank", "2");
        f.prop("n2", "rank", "10");
        // Numeric comparison: 10 > 2 (not lexicographic, where "10" < "2").
        let out = run("?- node(N, \"Note\"), prop(N, \"rank\", R), R > \"5\".", &f);
        assert_eq!(sorted(out), vec![vec!["n2".to_string(), "10".to_string()]]);
    }

    #[test]
    fn derived_budget_aborts_cartesian() {
        let mut f = Facts::new();
        for i in 0..100 {
            f.node(format!("n{i}"), "Note");
        }
        let p = parse_program("?- node(X, _), node(Y, _), node(Z, _).").unwrap();
        crate::validate::check(&p).unwrap();
        let limits = EvalLimits {
            max_derived: 1_000,
            ..EvalLimits::default()
        };
        assert!(matches!(eval(&p, &f, &limits), Err(LogicError::Eval(_))));
    }

    #[test]
    fn empty_facts_yield_no_rows() {
        let out = run("?- note(M).", &Facts::new());
        assert!(out.rows.is_empty());
    }
}
