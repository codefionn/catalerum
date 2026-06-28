//! Recursive-descent parser: tokens → a desugared [`Program`].
//!
//! Grammar (EBNF-ish):
//! ```text
//! program := clause+                       % exactly one clause is the ?- query
//! clause  := rule | query
//! rule    := atom ":-" body "."
//! query   := "?-" body "."
//! body    := literal { "," literal }
//! literal := atom | term cmp_op term
//! atom    := ident "(" [ term { "," term } ] ")"
//! term    := var | string | number
//! ```
//! Label/edge **sugar** and the `ieq`/`like` builtins are rewritten here: a body
//! `note(X)` becomes `node(X, "Note")`, `references(F,T)` becomes
//! `edge(F, "REFERENCES", T)`, and `ieq(A,B)` becomes a comparison literal. Rule
//! **heads** are never desugared (a reserved head is rejected later by
//! [`crate::validate`]). Bare facts (`foo(X).` with no `:-`) are rejected.

use crate::ast::{Atom, CmpOp, Literal, Program, Query, Rule, Term, Value};
use crate::error::LogicError;
use crate::lex::{lex, Tok, Token};
use crate::schema;

/// Parse `src` into a desugared [`Program`] (syntax only — call
/// [`crate::validate`] for safety/well-formedness). Errors on malformed syntax,
/// sugar-arity mistakes, a missing/duplicate `?-` goal, or a bare fact.
pub fn parse_program(src: &str) -> Result<Program, LogicError> {
    let tokens = lex(src)?;
    let mut p = Parser {
        tokens: &tokens,
        pos: 0,
        anon: 0,
    };
    p.program()
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    anon: usize,
}

fn perr(line: usize, msg: impl Into<String>) -> LogicError {
    LogicError::Parse(format!("line {line}: {}", msg.into()))
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn line(&self) -> usize {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(1, |t| t.line)
    }

    fn bump(&mut self) -> Option<&'a Tok> {
        let t = self.tokens.get(self.pos).map(|t| &t.tok);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn expect(&mut self, want: &Tok, what: &str) -> Result<(), LogicError> {
        match self.bump() {
            Some(t) if t == want => Ok(()),
            Some(other) => Err(perr(
                self.line(),
                format!("expected {what}, found {other:?}"),
            )),
            None => Err(perr(
                self.line(),
                format!("expected {what}, found end of input"),
            )),
        }
    }

    fn program(&mut self) -> Result<Program, LogicError> {
        let mut rules = Vec::new();
        let mut query: Option<Query> = None;
        while self.peek().is_some() {
            if matches!(self.peek(), Some(Tok::QueryArrow)) {
                self.bump();
                let body = self.body()?;
                self.expect(&Tok::Dot, "'.' after the query goal")?;
                if query.is_some() {
                    return Err(perr(
                        self.line(),
                        "a program may have only one '?-' query goal",
                    ));
                }
                let columns = query_columns(&body);
                query = Some(Query { body, columns });
            } else {
                rules.push(self.rule()?);
            }
        }
        let query =
            query.ok_or_else(|| LogicError::Parse("program has no '?-' query goal".to_string()))?;
        Ok(Program { rules, query })
    }

    fn rule(&mut self) -> Result<Rule, LogicError> {
        let head = self.raw_atom()?;
        // A bare atom with no neck is a fact — disallowed (users don't add facts;
        // the graph is the only source of facts).
        match self.peek() {
            Some(Tok::ColonDash) => {
                self.bump();
            }
            Some(Tok::Dot) => {
                return Err(perr(
                    self.line(),
                    format!(
                        "bare fact '{}(…).' is not allowed — write a rule 'head :- body.' or the '?-' query",
                        head.pred
                    ),
                ));
            }
            other => {
                return Err(perr(self.line(), format!("expected ':-', found {other:?}")));
            }
        }
        let body = self.body()?;
        self.expect(&Tok::Dot, "'.' after the rule body")?;
        Ok(Rule { head, body })
    }

    fn body(&mut self) -> Result<Vec<Literal>, LogicError> {
        let mut lits = vec![self.literal()?];
        while matches!(self.peek(), Some(Tok::Comma)) {
            self.bump();
            lits.push(self.literal()?);
        }
        Ok(lits)
    }

    fn literal(&mut self) -> Result<Literal, LogicError> {
        match self.peek() {
            // An atom (possibly a sugar/builtin, desugared below).
            Some(Tok::Ident(_)) => {
                let atom = self.raw_atom()?;
                self.desugar(atom)
            }
            // Otherwise an infix comparison `term cmp term`.
            Some(Tok::Var(_) | Tok::Str(_) | Tok::Num(_)) => {
                let left = self.term()?;
                let op = self.cmp_op()?;
                let right = self.term()?;
                Ok(Literal::Cmp { op, left, right })
            }
            other => Err(perr(
                self.line(),
                format!("expected a literal, found {other:?}"),
            )),
        }
    }

    /// Parse `ident ( term, … )` with no desugaring (used for heads and as the raw
    /// form a body literal is desugared from).
    fn raw_atom(&mut self) -> Result<Atom, LogicError> {
        let pred = match self.bump() {
            Some(Tok::Ident(name)) => name.clone(),
            other => {
                return Err(perr(
                    self.line(),
                    format!("expected a predicate name, found {other:?}"),
                ))
            }
        };
        self.expect(&Tok::LParen, "'(' after a predicate name")?;
        let mut args = Vec::new();
        if !matches!(self.peek(), Some(Tok::RParen)) {
            args.push(self.term()?);
            while matches!(self.peek(), Some(Tok::Comma)) {
                self.bump();
                args.push(self.term()?);
            }
        }
        self.expect(&Tok::RParen, "')' to close the argument list")?;
        Ok(Atom { pred, args })
    }

    /// Rewrite a body atom's sugar/builtin form into base atoms / comparisons.
    fn desugar(&self, atom: Atom) -> Result<Literal, LogicError> {
        let line = self.line();
        // ieq / like builtins → comparison literals.
        if schema::BUILTIN_PREDS.contains(&atom.pred.as_str()) {
            if atom.args.len() != 2 {
                return Err(perr(
                    line,
                    format!("{}(…) takes exactly 2 arguments", atom.pred),
                ));
            }
            let op = if atom.pred == "ieq" {
                CmpOp::IEq
            } else {
                CmpOp::Like
            };
            let mut it = atom.args.into_iter();
            let left = it.next().unwrap();
            let right = it.next().unwrap();
            return Ok(Literal::Cmp { op, left, right });
        }
        // Unary label sugar: note(X) → node(X, "Note").
        if let Some(label) = schema::label_sugar(&atom.pred) {
            if atom.args.len() != 1 {
                return Err(perr(
                    line,
                    format!("{}(…) takes exactly 1 argument (a node id)", atom.pred),
                ));
            }
            let x = atom.args.into_iter().next().unwrap();
            return Ok(atom_lit(Atom {
                pred: "node".to_string(),
                args: vec![x, Term::Const(Value(label.to_string()))],
            }));
        }
        // Binary edge sugar: references(F,T) → edge(F, "REFERENCES", T).
        if let Some(etype) = schema::edge_sugar(&atom.pred) {
            if atom.args.len() != 2 {
                return Err(perr(
                    line,
                    format!("{}(…) takes exactly 2 arguments (from, to)", atom.pred),
                ));
            }
            let mut it = atom.args.into_iter();
            let from = it.next().unwrap();
            let to = it.next().unwrap();
            return Ok(atom_lit(Atom {
                pred: "edge".to_string(),
                args: vec![from, Term::Const(Value(etype.to_string())), to],
            }));
        }
        Ok(atom_lit(atom))
    }

    fn term(&mut self) -> Result<Term, LogicError> {
        match self.bump() {
            Some(Tok::Var(v)) => {
                if v == "_" {
                    let name = format!("_G{}", self.anon);
                    self.anon += 1;
                    Ok(Term::Var(name))
                } else {
                    Ok(Term::Var(v.clone()))
                }
            }
            Some(Tok::Str(s)) => Ok(Term::Const(Value(s.clone()))),
            Some(Tok::Num(n)) => Ok(Term::Const(Value(canonical_number(n)))),
            other => Err(perr(
                self.line(),
                format!("expected a variable or constant, found {other:?}"),
            )),
        }
    }

    fn cmp_op(&mut self) -> Result<CmpOp, LogicError> {
        let op = match self.bump() {
            Some(Tok::Eq) => CmpOp::Eq,
            Some(Tok::Ne) => CmpOp::Ne,
            Some(Tok::Lt) => CmpOp::Lt,
            Some(Tok::Le) => CmpOp::Le,
            Some(Tok::Gt) => CmpOp::Gt,
            Some(Tok::Ge) => CmpOp::Ge,
            other => {
                return Err(perr(
                    self.line(),
                    format!("expected a comparison operator, found {other:?}"),
                ))
            }
        };
        Ok(op)
    }
}

fn atom_lit(atom: Atom) -> Literal {
    Literal::Atom {
        atom,
        negated: false,
    }
}

/// Canonicalise a numeric literal's text so `42`, `42.0`, `-0` compare/join
/// consistently as strings: integers keep integer form, reals keep their `f64`
/// round-trip. (Values are a single string domain; see [`crate::ast::Value`].)
fn canonical_number(text: &str) -> String {
    if let Ok(i) = text.parse::<i64>() {
        return i.to_string();
    }
    match text.parse::<f64>() {
        Ok(f) => f.to_string(),
        Err(_) => text.to_string(),
    }
}

/// The query's output columns: the distinct non-anonymous variables of its body,
/// in first-appearance order.
fn query_columns(body: &[Literal]) -> Vec<String> {
    let mut cols = Vec::new();
    for lit in body {
        match lit {
            Literal::Atom { atom, .. } => {
                for t in &atom.args {
                    crate::ast::push_term_var(t, &mut cols);
                }
            }
            Literal::Cmp { left, right, .. } => {
                crate::ast::push_term_var(left, &mut cols);
                crate::ast::push_term_var(right, &mut cols);
            }
        }
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desugars_label_edge_and_builtins() {
        let p = parse_program(
            r#"result(M, T) :- topic(X), references(M, X), prop(M, "title", T), ieq(T, "hi").
               ?- result(M, T)."#,
        )
        .unwrap();
        let r = &p.rules[0];
        // topic(X) → node(X, "Topic")
        assert_eq!(
            r.body[0],
            Literal::Atom {
                atom: Atom {
                    pred: "node".into(),
                    args: vec![Term::Var("X".into()), Term::Const(Value("Topic".into()))]
                },
                negated: false
            }
        );
        // references(M, X) → edge(M, "REFERENCES", X)
        assert_eq!(
            r.body[1],
            Literal::Atom {
                atom: Atom {
                    pred: "edge".into(),
                    args: vec![
                        Term::Var("M".into()),
                        Term::Const(Value("REFERENCES".into())),
                        Term::Var("X".into())
                    ]
                },
                negated: false
            }
        );
        // ieq(T, "hi") → Cmp(IEq, T, "hi")
        assert_eq!(
            r.body[3],
            Literal::Cmp {
                op: CmpOp::IEq,
                left: Term::Var("T".into()),
                right: Term::Const(Value("hi".into()))
            }
        );
        assert_eq!(p.query.columns, vec!["M".to_string(), "T".to_string()]);
    }

    #[test]
    fn anonymous_vars_are_fresh_and_excluded_from_columns() {
        let p = parse_program("?- node(X, _), edge(X, _, _).").unwrap();
        assert_eq!(p.query.columns, vec!["X".to_string()]);
    }

    #[test]
    fn infix_comparison_and_negative_number() {
        let p = parse_program("?- prop(N, \"n\", V), V >= -3.").unwrap();
        assert!(matches!(
            p.query.body[1],
            Literal::Cmp { op: CmpOp::Ge, .. }
        ));
    }

    #[test]
    fn rejects_bare_fact_and_missing_query() {
        assert!(parse_program("foo(X).").is_err());
        assert!(parse_program("h(X) :- node(X, \"Note\").").is_err()); // no ?- goal
        assert!(parse_program("?- node(X). ?- node(Y).").is_err()); // two goals
    }

    #[test]
    fn rejects_sugar_arity_mistakes() {
        assert!(parse_program("?- note(X, Y).").is_err());
        assert!(parse_program("?- references(X).").is_err());
        assert!(parse_program("?- ieq(X).").is_err());
    }

    #[test]
    fn cypher_input_fails_to_parse() {
        assert!(parse_program("MATCH (n) RETURN n").is_err());
    }
}
