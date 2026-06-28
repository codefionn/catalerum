//! A hand-rolled tokenizer for graph-Datalog (in the crate's dependency-light,
//! hand-rolled style — no lexer-generator). Turns source text into [`Token`]s;
//! `%` starts a line comment. Unterminated strings and stray punctuation fail
//! closed with a line-numbered [`LogicError::Parse`].

use crate::error::LogicError;

/// A lexical token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    /// A lower-case-led identifier (predicate or builtin name).
    Ident(String),
    /// A variable: `_` alone, or an upper-case/underscore-led name.
    Var(String),
    /// A double-quoted string literal (unescaped contents).
    Str(String),
    /// A numeric literal, kept as its source text (canonicalised later).
    Num(String),
    LParen,
    RParen,
    Comma,
    Dot,
    /// `:-` (rule neck).
    ColonDash,
    /// `?-` (query goal).
    QueryArrow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A token plus the 1-based source line it started on (for error messages).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

fn err(line: usize, msg: impl Into<String>) -> LogicError {
    LogicError::Parse(format!("line {line}: {}", msg.into()))
}

/// Tokenize `src`. Returns [`LogicError::Parse`] on any illegal character sequence.
pub fn lex(src: &str) -> Result<Vec<Token>, LogicError> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut line = 1;
    let mut out = Vec::new();

    while i < chars.len() {
        let c = chars[i];
        match c {
            '\n' => {
                line += 1;
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '%' => {
                // Line comment to end of line (newline handled next loop).
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '(' => {
                out.push(Token {
                    tok: Tok::LParen,
                    line,
                });
                i += 1;
            }
            ')' => {
                out.push(Token {
                    tok: Tok::RParen,
                    line,
                });
                i += 1;
            }
            ',' => {
                out.push(Token {
                    tok: Tok::Comma,
                    line,
                });
                i += 1;
            }
            '.' => {
                out.push(Token {
                    tok: Tok::Dot,
                    line,
                });
                i += 1;
            }
            ':' => {
                if chars.get(i + 1) == Some(&'-') {
                    out.push(Token {
                        tok: Tok::ColonDash,
                        line,
                    });
                    i += 2;
                } else {
                    return Err(err(line, "stray ':' (expected ':-')"));
                }
            }
            '?' => {
                if chars.get(i + 1) == Some(&'-') {
                    out.push(Token {
                        tok: Tok::QueryArrow,
                        line,
                    });
                    i += 2;
                } else {
                    return Err(err(line, "stray '?' (expected '?-')"));
                }
            }
            '=' => {
                out.push(Token { tok: Tok::Eq, line });
                i += 1;
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token { tok: Tok::Ne, line });
                    i += 2;
                } else {
                    return Err(err(line, "stray '!' (expected '!=')"));
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token { tok: Tok::Le, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Lt, line });
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    out.push(Token { tok: Tok::Ge, line });
                    i += 2;
                } else {
                    out.push(Token { tok: Tok::Gt, line });
                    i += 1;
                }
            }
            '"' => {
                let (s, ni) = lex_string(&chars, i, line)?;
                out.push(Token {
                    tok: Tok::Str(s),
                    line,
                });
                i = ni;
            }
            '-' | '0'..='9' => {
                // A '-' is only legal as the sign of a numeric literal (no arithmetic).
                if c == '-' && !chars.get(i + 1).is_some_and(char::is_ascii_digit) {
                    return Err(err(line, "stray '-' (a '-' may only start a number)"));
                }
                let (n, ni) = lex_number(&chars, i);
                out.push(Token {
                    tok: Tok::Num(n),
                    line,
                });
                i = ni;
            }
            c if c == '_' || c.is_ascii_uppercase() => {
                let (name, ni) = lex_ident(&chars, i);
                out.push(Token {
                    tok: Tok::Var(name),
                    line,
                });
                i = ni;
            }
            c if c.is_ascii_lowercase() => {
                let (name, ni) = lex_ident(&chars, i);
                out.push(Token {
                    tok: Tok::Ident(name),
                    line,
                });
                i = ni;
            }
            other => return Err(err(line, format!("unexpected character '{other}'"))),
        }
    }
    Ok(out)
}

/// Read a `"…"` string body starting at the opening quote `chars[start]`.
fn lex_string(chars: &[char], start: usize, line: usize) -> Result<(String, usize), LogicError> {
    let mut i = start + 1; // past the opening quote
    let mut s = String::new();
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((s, i + 1)),
            '\\' => {
                let esc = chars.get(i + 1).copied();
                match esc {
                    Some('"') => s.push('"'),
                    Some('\\') => s.push('\\'),
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some(other) => {
                        return Err(err(line, format!("invalid string escape '\\{other}'")))
                    }
                    None => return Err(err(line, "unterminated string escape")),
                }
                i += 2;
            }
            '\n' => return Err(err(line, "unterminated string (newline in literal)")),
            c => {
                s.push(c);
                i += 1;
            }
        }
    }
    Err(err(line, "unterminated string literal"))
}

/// Read a numeric literal `[-]?digits[.digits]` starting at `chars[start]`.
fn lex_number(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    if chars[i] == '-' {
        i += 1;
    }
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i < chars.len() && chars[i] == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit) {
        i += 1;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
    }
    (chars[start..i].iter().collect(), i)
}

/// Read an identifier/variable `[A-Za-z_][A-Za-z0-9_]*` starting at `chars[start]`.
fn lex_ident(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
        i += 1;
    }
    (chars[start..i].iter().collect(), i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_a_simple_query() {
        assert_eq!(
            toks("?- node(X, \"Note\")."),
            vec![
                Tok::QueryArrow,
                Tok::Ident("node".into()),
                Tok::LParen,
                Tok::Var("X".into()),
                Tok::Comma,
                Tok::Str("Note".into()),
                Tok::RParen,
                Tok::Dot,
            ]
        );
    }

    #[test]
    fn lexes_operators_and_comments() {
        // Comment stripped; all comparison ops recognised (incl. two-char ones).
        assert_eq!(
            toks("% hi\nA != B <= C >= D < E > F = G"),
            vec![
                Tok::Var("A".into()),
                Tok::Ne,
                Tok::Var("B".into()),
                Tok::Le,
                Tok::Var("C".into()),
                Tok::Ge,
                Tok::Var("D".into()),
                Tok::Lt,
                Tok::Var("E".into()),
                Tok::Gt,
                Tok::Var("F".into()),
                Tok::Eq,
                Tok::Var("G".into()),
            ]
        );
    }

    #[test]
    fn lexes_numbers_and_neck() {
        assert_eq!(
            toks("h :- x(-3.5, 42)."),
            vec![
                Tok::Ident("h".into()),
                Tok::ColonDash,
                Tok::Ident("x".into()),
                Tok::LParen,
                Tok::Num("-3.5".into()),
                Tok::Comma,
                Tok::Num("42".into()),
                Tok::RParen,
                Tok::Dot,
            ]
        );
    }

    #[test]
    fn escapes_and_errors() {
        assert_eq!(toks(r#""a\"b""#), vec![Tok::Str("a\"b".into())]);
        assert!(lex("\"unterminated").is_err());
        assert!(lex(": x").is_err());
        assert!(lex("? x").is_err());
        assert!(lex("a ! b").is_err());
        assert!(lex("a - b").is_err());
        assert!(lex("#").is_err());
    }
}
