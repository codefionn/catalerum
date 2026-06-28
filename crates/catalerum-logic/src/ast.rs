//! The parsed, desugared program: values, terms, atoms, comparisons, rules, the
//! query goal, and the whole [`Program`].
//!
//! By the time a [`Program`] exists, all label/edge **sugar** has been rewritten
//! into `node`/`edge` atoms (see [`crate::parse`]), so downstream code only ever
//! sees the three base predicates plus user-defined (IDB) predicates.

/// A ground value. Every value is carried as a string: the fact loader stringifies
/// scalar node properties, node ids are uuid strings, and labels/edge-types are
/// fixed strings — so a single string domain keeps equality/hashing total and
/// deterministic. Comparison operators (`<`,`>`,…) parse both sides as numbers
/// when they can, else compare lexically (see [`crate::eval`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub String);

impl Value {
    /// The underlying string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value(s.to_owned())
    }
}

/// A term in an atom or comparison: a variable or a ground constant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Term {
    /// A logic variable. Anonymous `_` occurrences are rewritten to fresh unique
    /// names (`_G0`, `_G1`, …) at parse time so they never accidentally join, and
    /// they are excluded from the query's output columns.
    Var(String),
    /// A ground constant (string or number literal, both stored as a [`Value`]).
    Const(Value),
}

/// A positive literal: a predicate applied to terms. After desugaring, `pred` is
/// `node`/`edge`/`prop` or a user-defined (rule-head) predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atom {
    pub pred: String,
    pub args: Vec<Term>,
}

/// A comparison operator. `IEq`/`Like` are the case-insensitive string builtins
/// (written `ieq(A,B)` / `like(A,B)`); the rest are the infix operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Case-insensitive equality (`toLower(a) == toLower(b)`).
    IEq,
    /// Case-insensitive substring: `b` occurs within `a` (`a CONTAINS b`).
    Like,
}

/// A body literal: a positive [`Atom`] or a [`CmpOp`] comparison between two terms.
///
/// The `negated` flag is a forward hook for **stratified** negation (SOUL: a §18
/// follow-up); v1 always parses it `false` and the evaluator is a pure monotone
/// fixpoint. Keeping the field lets the AST and validator grow negation without a
/// breaking change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    Atom { atom: Atom, negated: bool },
    Cmp { op: CmpOp, left: Term, right: Term },
}

/// A rule `head :- body.` — a user-defined predicate derived from a conjunction of
/// body literals.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub head: Atom,
    pub body: Vec<Literal>,
}

/// The single query goal `?- body.` — a conjunction of body literals whose
/// satisfying bindings, projected onto `columns`, are the result rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub body: Vec<Literal>,
    /// The output columns: the distinct **non-anonymous** variables of `body`, in
    /// first-appearance order. Each names a result column.
    pub columns: Vec<String>,
}

/// A complete program: zero or more [`Rule`]s and exactly one [`Query`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Program {
    pub rules: Vec<Rule>,
    pub query: Query,
}

/// Split a body's literals into its positive atoms and its comparisons — the shape
/// the evaluator joins (atoms first) then filters (comparisons). Preserves order
/// within each group.
#[must_use]
pub fn split_body(body: &[Literal]) -> (Vec<&Atom>, Vec<(&CmpOp, &Term, &Term)>) {
    let mut atoms = Vec::new();
    let mut cmps = Vec::new();
    for lit in body {
        match lit {
            Literal::Atom { atom, .. } => atoms.push(atom),
            Literal::Cmp { op, left, right } => cmps.push((op, left, right)),
        }
    }
    (atoms, cmps)
}

/// The variables of a term (0 or 1), pushed onto `out` if not already present and
/// not anonymous (a fresh `_G…` name).
pub(crate) fn push_term_var(term: &Term, out: &mut Vec<String>) {
    if let Term::Var(v) = term {
        if !is_anonymous(v) && !out.contains(v) {
            out.push(v.clone());
        }
    }
}

/// Whether a variable name is a parser-generated anonymous (`_`) variable.
#[must_use]
pub(crate) fn is_anonymous(name: &str) -> bool {
    name.starts_with("_G")
}
