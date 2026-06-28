//! LaTeX math → **MathML**, in pure Rust, no JavaScript.
//!
//! MathML is rendered natively by every current browser (Chrome 109+, Firefox,
//! Safari 16.4+), so emitting `<math>…</math>` gives real, selectable, font-scaled
//! math with zero client-side script. [`to_mathml`] parses a useful subset of TeX
//! math and returns a `<math>` element string that is safe to inject (every text
//! atom is HTML-escaped; the element structure is ours, never the input's).
//!
//! ## Supported
//! Identifiers, numbers, operators; `^`/`_` scripts; `\frac`, `\sqrt`/`\sqrt[n]`,
//! `\left…\right` fences, `\sum`/`\prod`/`\int`/`\lim` with limits, Greek letters
//! and a large symbol table, `\text{}`, `\mathbb/\mathcal/\mathbf/\mathrm/\mathit`,
//! function names (`\sin`…), spacing, and `matrix`/`pmatrix`/`bmatrix`/`vmatrix`/
//! `cases` environments. Unknown commands degrade to literal text — never a panic.

use std::fmt::Write as _;

use crate::escape::escape_text;

/// Recursion guard so adversarial input (`{{{{…`, `\frac\frac…`) can't blow the
/// stack — beyond this depth, parsing stops gracefully.
const MAX_DEPTH: usize = 64;

/// Whether a fenced-code info string marks a display-math block (`math`/`latex`/`tex`).
pub(crate) fn is_math_lang(lang: &str) -> bool {
    lang.eq_ignore_ascii_case("math")
        || lang.eq_ignore_ascii_case("latex")
        || lang.eq_ignore_ascii_case("tex")
}

/// Render LaTeX math `latex` to a MathML `<math>` string. `display` selects block
/// (`display="block"`) vs inline styling.
pub fn to_mathml(latex: &str, display: bool) -> String {
    let chars: Vec<char> = latex.chars().collect();
    let mut p = MathParser {
        c: &chars,
        i: 0,
        depth: 0,
    };
    let nodes = p.sequence(Stop::End);
    let mut out = String::with_capacity(latex.len() * 4 + 48);
    out.push_str(if display {
        "<math xmlns=\"http://www.w3.org/1998/Math/MathML\" display=\"block\">"
    } else {
        "<math xmlns=\"http://www.w3.org/1998/Math/MathML\">"
    });
    render_seq(&nodes, &mut out);
    out.push_str("</math>");
    out
}

/// A parsed math node.
#[derive(Debug, Clone)]
enum M {
    Row(Vec<M>),
    /// `<mi>` identifier; `normal` forces upright (function names, `\mathrm`).
    Ident(String, bool),
    Number(String),
    /// `<mo>` operator; `stretchy` for fence delimiters.
    Op(String, bool),
    Text(String),
    Frac(Box<M>, Box<M>),
    Sqrt(Box<M>),
    Root(Box<M>, Box<M>),
    /// base, sub, sup (either may be absent); `limits` ⇒ under/over instead of sub/sup.
    Scripts {
        base: Box<M>,
        sub: Option<Box<M>>,
        sup: Option<Box<M>>,
        limits: bool,
    },
    Fenced {
        left: String,
        body: Box<M>,
        right: String,
    },
    Table {
        left: &'static str,
        right: &'static str,
        rows: Vec<Vec<M>>,
        /// `<mtable>` `columnalign` (alignment-env `&` columns, or an `array`'s
        /// `{rcl}` column spec); empty = default (centered).
        col_align: String,
    },
    Styled(&'static str, Box<M>),
    /// An accent over (`\hat`, `\vec`, `\bar`, …) or line under (`\underline`) the
    /// base: `<mover/munder accent="true">`. `stretchy` for the wide forms.
    Accent {
        base: Box<M>,
        sym: &'static str,
        over: bool,
        stretchy: bool,
    },
    /// `\binom{n}{k}` — a binomial coefficient: `(n / k)` with no fraction bar.
    Binom(Box<M>, Box<M>),
    /// `\boxed{…}` — its content framed in a border (models box final answers).
    Boxed(Box<M>),
    /// `\overline{…}` / `\underline{…}` — a rule over/under the whole content. A
    /// CSS border spans the element width, so unlike a `<mover>` macron it
    /// **stretches** across a multi-character group. `under` ⇒ underline.
    Rule {
        base: Box<M>,
        under: bool,
    },
    /// `\overset{mark}{base}` / `\underset{mark}{base}` — an arbitrary expression
    /// stacked over/under the base (`<mover>`/`<munder>`, full-size, not an accent).
    Stack {
        base: Box<M>,
        mark: Box<M>,
        over: bool,
    },
    Space(&'static str),
    Phantom,
}

#[derive(Clone, Copy, PartialEq)]
enum Stop {
    End,
    Brace,
    Right,
    Cell,
}

struct MathParser<'a> {
    c: &'a [char],
    i: usize,
    depth: usize,
}

impl MathParser<'_> {
    fn peek(&self) -> Option<char> {
        self.c.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.i += 1;
        }
        ch
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.i += 1;
        }
    }

    /// Parse a run of atoms until a stop token. `&`/`\\` end a cell; `}` ends a
    /// brace group; `\right` ends a fence; EOF ends everything.
    fn sequence(&mut self, stop: Stop) -> Vec<M> {
        let mut out = Vec::new();
        if self.depth >= MAX_DEPTH {
            return out;
        }
        loop {
            self.skip_ws();
            match self.peek() {
                None => break,
                Some('}') if stop == Stop::Brace => break,
                Some('&') if stop == Stop::Cell => break,
                Some('\\') if stop == Stop::Cell && self.starts_with("\\\\") => break,
                Some('\\') if stop == Stop::Right && self.starts_with_cmd("right") => break,
                Some('\\') if stop == Stop::Cell && self.starts_with_cmd("end") => break,
                Some('\\') if self.starts_with_cmd("end") && stop != Stop::Brace => break,
                _ => {}
            }
            let Some(atom) = self.atom() else { break };
            out.push(self.scripts(atom));
        }
        out
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(k, ch)| self.c.get(self.i + k).copied() == Some(ch))
    }

    /// Whether the upcoming token is `\<name>` (name not followed by a letter).
    fn starts_with_cmd(&self, name: &str) -> bool {
        if self.peek() != Some('\\') {
            return false;
        }
        for (k, ch) in name.chars().enumerate() {
            if self.c.get(self.i + 1 + k).copied() != Some(ch) {
                return false;
            }
        }
        !matches!(self.c.get(self.i + 1 + name.chars().count()), Some(c) if c.is_alphabetic())
    }

    /// One atom (a single base unit, before scripts).
    fn atom(&mut self) -> Option<M> {
        self.skip_ws();
        let ch = self.peek()?;
        match ch {
            '{' => {
                self.bump();
                self.depth += 1;
                let inner = self.sequence(Stop::Brace);
                self.depth = self.depth.saturating_sub(1);
                if self.peek() == Some('}') {
                    self.bump();
                }
                Some(group(inner))
            }
            '}' => None,
            '\\' => self.command(),
            '^' | '_' => Some(M::Phantom), // a script with no base — empty base
            '0'..='9' => Some(self.number()),
            '+' | '-' | '*' | '=' | '<' | '>' | '/' | '(' | ')' | '[' | ']' | '|' | ',' | ';'
            | ':' | '.' | '!' | '?' => {
                self.bump();
                Some(M::Op(ch.to_string(), false))
            }
            _ => {
                self.bump();
                if ch.is_alphabetic() {
                    Some(M::Ident(ch.to_string(), false))
                } else {
                    Some(M::Op(ch.to_string(), false))
                }
            }
        }
    }

    fn number(&mut self) -> M {
        let mut s = String::new();
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || c == '.') {
            s.push(self.bump().expect("peeked"));
        }
        M::Number(s)
    }

    /// A `\command` (already at the backslash).
    fn command(&mut self) -> Option<M> {
        self.bump(); // backslash
        let Some(first) = self.peek() else {
            return Some(M::Op("\\".into(), false));
        };
        if !first.is_alphabetic() {
            // Escaped single char: `\{`, `\%`, `\,` handled in spacing below, etc.
            self.bump();
            return Some(match first {
                '{' | '}' | '%' | '$' | '#' | '_' | '&' => M::Op(first.to_string(), false),
                ' ' => M::Space("0.25em"),
                ',' => M::Space("0.17em"),
                ';' => M::Space("0.28em"),
                ':' => M::Space("0.22em"),
                '!' => M::Space("-0.17em"),
                _ => M::Op(first.to_string(), false),
            });
        }
        let mut name = String::new();
        while matches!(self.peek(), Some(c) if c.is_alphabetic()) {
            name.push(self.bump().expect("peeked"));
        }
        Some(self.command_body(&name))
    }

    fn command_body(&mut self, name: &str) -> M {
        match name {
            "frac" | "dfrac" | "tfrac" => {
                let num = self.group_arg();
                let den = self.group_arg();
                M::Frac(Box::new(num), Box::new(den))
            }
            "sqrt" => {
                if self.peek() == Some('[') {
                    let idx = self.optional_arg();
                    let rad = self.group_arg();
                    M::Root(Box::new(rad), Box::new(idx))
                } else {
                    M::Sqrt(Box::new(self.group_arg()))
                }
            }
            // Accents (over the base) and `\underline` (a line under it).
            "hat" => self.accent("^", true, false),
            "widehat" => self.accent("^", true, true),
            "tilde" => self.accent("~", true, false),
            "widetilde" => self.accent("~", true, true),
            "bar" => self.accent("\u{00AF}", true, false),
            // `\overline`/`\underline` use a stretching CSS rule (not a fixed-width
            // macron) so the line spans the whole group, not just its centre.
            "overline" => M::Rule {
                base: Box::new(self.group_arg()),
                under: false,
            },
            "underline" => M::Rule {
                base: Box::new(self.group_arg()),
                under: true,
            },
            "vec" => self.accent("\u{20D7}", true, false),
            "overrightarrow" => self.accent("\u{20D7}", true, true),
            "dot" => self.accent("\u{02D9}", true, false),
            "ddot" => self.accent("\u{00A8}", true, false),
            "check" => self.accent("\u{02C7}", true, false),
            "breve" => self.accent("\u{02D8}", true, false),
            "acute" => self.accent("\u{00B4}", true, false),
            "grave" => self.accent("`", true, false),
            // Stretchy braces over/under the base.
            "overbrace" => self.accent("\u{23DE}", true, true),
            "underbrace" => self.accent("\u{23DF}", false, true),
            // `\overset{mark}{base}` / `\underset{mark}{base}` — stack an arbitrary
            // expression (not a fixed accent char) over/under the base.
            "overset" | "underset" => {
                let mark = self.group_arg();
                let base = self.group_arg();
                M::Stack {
                    base: Box::new(base),
                    mark: Box::new(mark),
                    over: name == "overset",
                }
            }
            "binom" | "dbinom" | "tbinom" => {
                let n = self.group_arg();
                let k = self.group_arg();
                M::Binom(Box::new(n), Box::new(k))
            }
            "boxed" => M::Boxed(Box::new(self.group_arg())),
            // Modular arithmetic: `\pmod{n}` → " (mod n)"; `\bmod` → the upright
            // "mod" operator. The `p`/`b` are LaTeX's parenthesised/binary
            // designations and are not shown (so `\bmod` must not render "bmod").
            "pmod" => {
                let n = self.group_arg();
                M::Row(vec![
                    M::Space("0.44em"),
                    M::Op("(".to_string(), false),
                    M::Ident("mod".to_string(), true),
                    M::Space("0.22em"),
                    n,
                    M::Op(")".to_string(), false),
                ])
            }
            "bmod" => M::Ident("mod".to_string(), true),
            "text" | "mbox" | "operatorname" => M::Text(self.raw_group()),
            "mathbb" => M::Styled("double-struck", Box::new(self.group_arg())),
            "mathcal" => M::Styled("script", Box::new(self.group_arg())),
            "mathfrak" => M::Styled("fraktur", Box::new(self.group_arg())),
            "mathbf" | "boldsymbol" => M::Styled("bold", Box::new(self.group_arg())),
            "mathit" => M::Styled("italic", Box::new(self.group_arg())),
            "mathrm" | "mathsf" | "mathtt" => M::Styled("normal", Box::new(self.group_arg())),
            "left" => self.fenced(),
            "begin" => self.environment(),
            "quad" => M::Space("1em"),
            "qquad" => M::Space("2em"),
            "," => M::Space("0.17em"),
            ";" => M::Space("0.28em"),
            "!" => M::Space("-0.17em"),
            "limits" | "nolimits" | "displaystyle" | "textstyle" => M::Phantom,
            _ => {
                if let Some((text, normal, is_op)) = symbol(name) {
                    if is_op {
                        M::Op(text.to_string(), false)
                    } else {
                        M::Ident(text.to_string(), normal)
                    }
                } else if is_function(name) {
                    M::Ident(name.to_string(), true)
                } else {
                    // Unknown command → show its name literally (degrade, no panic).
                    M::Ident(name.to_string(), true)
                }
            }
        }
    }

    /// Parse `\left<delim> … \right<delim>`.
    fn fenced(&mut self) -> M {
        let left = self.delimiter();
        self.depth += 1;
        let body = self.sequence(Stop::Right);
        self.depth = self.depth.saturating_sub(1);
        let mut right = String::new();
        if self.starts_with_cmd("right") {
            self.bump_cmd("right");
            right = self.delimiter();
        }
        M::Fenced {
            left,
            body: Box::new(group(body)),
            right,
        }
    }

    fn delimiter(&mut self) -> String {
        self.skip_ws();
        match self.peek() {
            Some('\\') => {
                // `\{`, `\|`, `\langle` …
                self.bump();
                if matches!(self.peek(), Some(c) if c.is_alphabetic()) {
                    let mut name = String::new();
                    while matches!(self.peek(), Some(c) if c.is_alphabetic()) {
                        name.push(self.bump().expect("peeked"));
                    }
                    symbol(&name)
                        .map(|(t, _, _)| t.to_string())
                        .unwrap_or_default()
                } else {
                    self.bump().map(|c| c.to_string()).unwrap_or_default()
                }
            }
            Some('.') => {
                self.bump();
                String::new() // `\left.` → no delimiter
            }
            Some(c) => {
                self.bump();
                c.to_string()
            }
            None => String::new(),
        }
    }

    fn environment(&mut self) -> M {
        let env = self.raw_group();
        let (left, right) = match env.as_str() {
            "pmatrix" => ("(", ")"),
            "bmatrix" => ("[", "]"),
            "Bmatrix" => ("{", "}"),
            "vmatrix" => ("|", "|"),
            "Vmatrix" => ("\u{2016}", "\u{2016}"),
            "cases" => ("{", ""),
            _ => ("", ""),
        };
        // Alignment environments align their `&`-separated columns right/left (so
        // `a &= b` lines up at the `=`); `cases` left-aligns both the value and the
        // condition column (piecewise functions); `array` carries an explicit
        // `{rcl}` column spec right after the name (consume it, else it leaks in as a
        // cell); other environments stay centered.
        let col_align = match env.as_str() {
            "aligned" | "align" | "align*" | "aligned*" | "split" | "eqnarray" => {
                "right left".to_string()
            }
            "cases" => "left left".to_string(),
            "array" => array_columnalign(&self.raw_group()),
            _ => String::new(),
        };
        let mut rows: Vec<Vec<M>> = Vec::new();
        let mut row: Vec<M> = Vec::new();
        self.depth += 1;
        loop {
            if self.depth >= MAX_DEPTH {
                break;
            }
            let cell = self.sequence(Stop::Cell);
            row.push(group(cell));
            self.skip_ws();
            match self.peek() {
                Some('&') => {
                    self.bump();
                }
                Some('\\') if self.starts_with("\\\\") => {
                    self.i += 2;
                    rows.push(std::mem::take(&mut row));
                }
                Some('\\') if self.starts_with_cmd("end") => {
                    self.bump_cmd("end");
                    let _ = self.raw_group();
                    break;
                }
                _ => break,
            }
        }
        self.depth = self.depth.saturating_sub(1);
        if !row.is_empty() {
            rows.push(row);
        }
        M::Table {
            left,
            right,
            rows,
            col_align,
        }
    }

    fn bump_cmd(&mut self, name: &str) {
        // Assumes `starts_with_cmd(name)`; consume `\name`.
        self.i += 1 + name.chars().count();
    }

    /// An accent/`\underline` over (or under) its single `{…}` argument.
    fn accent(&mut self, sym: &'static str, over: bool, stretchy: bool) -> M {
        M::Accent {
            base: Box::new(self.group_arg()),
            sym,
            over,
            stretchy,
        }
    }

    /// A `{…}`-delimited argument, returned as a single node.
    fn group_arg(&mut self) -> M {
        self.skip_ws();
        if self.peek() == Some('{') {
            self.bump();
            self.depth += 1;
            let inner = self.sequence(Stop::Brace);
            self.depth = self.depth.saturating_sub(1);
            if self.peek() == Some('}') {
                self.bump();
            }
            group(inner)
        } else {
            // A single token is the argument (e.g. `\frac12` = `\frac{1}{2}`, `x^2`).
            self.single_token()
        }
    }

    /// Exactly one token as an argument: a whole `\command`, or one character (so
    /// `\frac12` splits into `1` and `2`, matching TeX).
    fn single_token(&mut self) -> M {
        self.skip_ws();
        match self.peek() {
            Some('\\') => self.command().unwrap_or(M::Phantom),
            Some('{') => self.group_arg(),
            Some(c) if c.is_ascii_digit() => {
                self.bump();
                M::Number(c.to_string())
            }
            Some(c) if c.is_alphabetic() => {
                self.bump();
                M::Ident(c.to_string(), false)
            }
            Some(c) => {
                self.bump();
                M::Op(c.to_string(), false)
            }
            None => M::Phantom,
        }
    }

    /// An optional `[…]` argument (for `\sqrt[n]`), as a single node.
    fn optional_arg(&mut self) -> M {
        if self.peek() != Some('[') {
            return M::Phantom;
        }
        self.bump();
        let mut inner = Vec::new();
        self.depth += 1;
        while let Some(c) = self.peek() {
            if c == ']' {
                break;
            }
            let Some(a) = self.atom() else { break };
            inner.push(self.scripts(a));
        }
        self.depth = self.depth.saturating_sub(1);
        if self.peek() == Some(']') {
            self.bump();
        }
        group(inner)
    }

    /// Raw text of a `{…}` group (for `\text`, `\begin{…}`), no math parsing.
    fn raw_group(&mut self) -> String {
        self.skip_ws();
        let mut s = String::new();
        if self.peek() == Some('{') {
            self.bump();
            let mut depth = 1usize;
            while let Some(c) = self.bump() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                s.push(c);
            }
        }
        s
    }

    /// Attach `^`/`_` scripts to `base`.
    fn scripts(&mut self, base: M) -> M {
        let limits = is_limits_base(&base);
        let mut sub = None;
        let mut sup = None;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('^') => {
                    self.bump();
                    sup = Some(Box::new(self.group_arg()));
                }
                Some('_') => {
                    self.bump();
                    sub = Some(Box::new(self.group_arg()));
                }
                _ => break,
            }
        }
        if sub.is_none() && sup.is_none() {
            base
        } else {
            M::Scripts {
                base: Box::new(base),
                sub,
                sup,
                limits,
            }
        }
    }
}

/// Collapse a parsed sequence into one node (single child as-is, else an `mrow`).
fn group(mut v: Vec<M>) -> M {
    if v.len() == 1 {
        v.pop().expect("len 1")
    } else {
        M::Row(v)
    }
}

/// Big operators whose scripts become under/over (`\lim_{x}`, `\sum^n_{i}`).
/// Map an `array` column spec (`{rcl}`) to a MathML `columnalign` value: `l`→left,
/// `c`→center, `r`→right; rules (`|`), `p{…}` widths and other chars are ignored.
/// Empty when no recognised column letters are present (⇒ default centered).
fn array_columnalign(spec: &str) -> String {
    let mut cols = Vec::new();
    for ch in spec.chars() {
        match ch {
            'l' => cols.push("left"),
            'c' => cols.push("center"),
            'r' => cols.push("right"),
            _ => {}
        }
    }
    cols.join(" ")
}

fn is_limits_base(base: &M) -> bool {
    matches!(base, M::Op(s, _) if matches!(s.as_str(), "∑" | "∏" | "∐" | "⋃" | "⋂" | "⨁" | "⨂" | "⨀"))
        || matches!(base, M::Ident(s, true) if matches!(s.as_str(), "lim" | "max" | "min" | "sup" | "inf" | "limsup" | "liminf" | "gcd" | "det" | "Pr"))
}

fn is_function(name: &str) -> bool {
    matches!(
        name,
        "sin"
            | "cos"
            | "tan"
            | "cot"
            | "sec"
            | "csc"
            | "sinh"
            | "cosh"
            | "tanh"
            | "arcsin"
            | "arccos"
            | "arctan"
            | "log"
            | "ln"
            | "lg"
            | "exp"
            | "lim"
            | "max"
            | "min"
            | "sup"
            | "inf"
            | "limsup"
            | "liminf"
            | "gcd"
            | "det"
            | "deg"
            | "dim"
            | "ker"
            | "hom"
            | "arg"
            | "Pr"
            | "mod"
            | "bmod"
    )
}

// ---- rendering -----------------------------------------------------------------

fn render_seq(nodes: &[M], out: &mut String) {
    for n in nodes {
        render(n, out);
    }
}

fn render(node: &M, out: &mut String) {
    match node {
        M::Row(v) => {
            out.push_str("<mrow>");
            render_seq(v, out);
            out.push_str("</mrow>");
        }
        M::Ident(s, normal) => {
            if *normal {
                out.push_str("<mi mathvariant=\"normal\">");
            } else {
                out.push_str("<mi>");
            }
            escape_text(out, s);
            out.push_str("</mi>");
        }
        M::Number(s) => {
            out.push_str("<mn>");
            escape_text(out, s);
            out.push_str("</mn>");
        }
        M::Op(s, stretchy) => {
            if *stretchy {
                out.push_str("<mo stretchy=\"true\">");
            } else {
                out.push_str("<mo>");
            }
            escape_text(out, s);
            out.push_str("</mo>");
        }
        M::Text(s) => {
            out.push_str("<mtext>");
            escape_text(out, s);
            out.push_str("</mtext>");
        }
        M::Frac(a, b) => {
            out.push_str("<mfrac>");
            render_arg(a, out);
            render_arg(b, out);
            out.push_str("</mfrac>");
        }
        M::Sqrt(a) => {
            out.push_str("<msqrt>");
            render(a, out);
            out.push_str("</msqrt>");
        }
        M::Root(rad, idx) => {
            out.push_str("<mroot>");
            render_arg(rad, out);
            render_arg(idx, out);
            out.push_str("</mroot>");
        }
        M::Scripts {
            base,
            sub,
            sup,
            limits,
        } => render_scripts(base, sub.as_deref(), sup.as_deref(), *limits, out),
        M::Fenced { left, body, right } => {
            out.push_str("<mrow>");
            if !left.is_empty() {
                out.push_str("<mo stretchy=\"true\">");
                escape_text(out, left);
                out.push_str("</mo>");
            }
            render(body, out);
            if !right.is_empty() {
                out.push_str("<mo stretchy=\"true\">");
                escape_text(out, right);
                out.push_str("</mo>");
            }
            out.push_str("</mrow>");
        }
        M::Table {
            left,
            right,
            rows,
            col_align,
        } => {
            out.push_str("<mrow>");
            if !left.is_empty() {
                out.push_str("<mo stretchy=\"true\">");
                escape_text(out, left);
                out.push_str("</mo>");
            }
            if col_align.is_empty() {
                out.push_str("<mtable>");
            } else {
                let _ = write!(out, "<mtable columnalign=\"{col_align}\">");
            }
            for row in rows {
                out.push_str("<mtr>");
                for cell in row {
                    out.push_str("<mtd>");
                    render(cell, out);
                    out.push_str("</mtd>");
                }
                out.push_str("</mtr>");
            }
            out.push_str("</mtable>");
            if !right.is_empty() {
                out.push_str("<mo stretchy=\"true\">");
                escape_text(out, right);
                out.push_str("</mo>");
            }
            out.push_str("</mrow>");
        }
        M::Styled(variant, inner) => {
            // Apply mathvariant to identifier leaves under `inner`.
            out.push_str("<mrow>");
            render_styled(inner, variant, out);
            out.push_str("</mrow>");
        }
        M::Accent {
            base,
            sym,
            over,
            stretchy,
        } => {
            let tag = if *over { "mover" } else { "munder" };
            let _ = write!(out, "<{tag} accent=\"true\">");
            render_arg(base, out);
            if *stretchy {
                out.push_str("<mo stretchy=\"true\">");
            } else {
                out.push_str("<mo>");
            }
            escape_text(out, sym);
            out.push_str("</mo>");
            let _ = write!(out, "</{tag}>");
        }
        M::Stack { base, mark, over } => {
            // overset/underset: the mark is a full-size expression (no accent flag).
            let tag = if *over { "mover" } else { "munder" };
            let _ = write!(out, "<{tag}>");
            render_arg(base, out);
            render_arg(mark, out);
            let _ = write!(out, "</{tag}>");
        }
        M::Binom(n, k) => {
            // A binomial: parentheses around a bar-less fraction.
            out.push_str("<mrow><mo stretchy=\"true\">(</mo><mfrac linethickness=\"0\">");
            render_arg(n, out);
            render_arg(k, out);
            out.push_str("</mfrac><mo stretchy=\"true\">)</mo></mrow>");
        }
        M::Boxed(inner) => {
            // A CSS border on the row frames the content (MathML Core honours the
            // `style` global attribute); the style string is a fixed literal.
            out.push_str("<mrow style=\"border:1px solid currentColor;padding:0.15em 0.35em\">");
            render(inner, out);
            out.push_str("</mrow>");
        }
        M::Rule { base, under } => {
            // A single CSS border edge that spans the content width (stretches over
            // the whole group, unlike a fixed-width `<mover>` macron).
            let edge = if *under { "bottom" } else { "top" };
            let _ = write!(
                out,
                "<mrow style=\"border-{edge}:0.06em solid currentColor;padding-{edge}:0.1em\">"
            );
            render(base, out);
            out.push_str("</mrow>");
        }
        M::Space(w) => {
            let _ = write!(out, "<mspace width=\"{w}\"/>");
        }
        M::Phantom => {}
    }
}

/// Render `node` as exactly one MathML element, as `mfrac`/`msup`/… require. Every
/// `M` renders to a single element except `Phantom` (an absent script/arg base),
/// which becomes an empty `<mrow>` so the parent stays well-formed.
fn render_arg(node: &M, out: &mut String) {
    if matches!(node, M::Phantom) {
        out.push_str("<mrow></mrow>");
    } else {
        render(node, out);
    }
}

fn render_scripts(base: &M, sub: Option<&M>, sup: Option<&M>, limits: bool, out: &mut String) {
    let (tag, n) = match (sub.is_some(), sup.is_some(), limits) {
        (true, true, true) => ("munderover", 3),
        (true, true, false) => ("msubsup", 3),
        (true, false, true) => ("munder", 2),
        (true, false, false) => ("msub", 2),
        (false, true, true) => ("mover", 2),
        (false, true, false) => ("msup", 2),
        _ => ("mrow", 1),
    };
    let _ = write!(out, "<{tag}>");
    render_arg(base, out);
    if let Some(s) = sub {
        render_arg(s, out);
    }
    if let Some(s) = sup {
        render_arg(s, out);
    }
    let _ = n;
    let _ = write!(out, "</{tag}>");
}

fn render_styled(node: &M, variant: &str, out: &mut String) {
    match node {
        M::Row(v) => {
            for n in v {
                render_styled(n, variant, out);
            }
        }
        M::Ident(s, _) => {
            let _ = write!(out, "<mi mathvariant=\"{variant}\">");
            escape_text(out, s);
            out.push_str("</mi>");
        }
        M::Number(s) => {
            let _ = write!(out, "<mn mathvariant=\"{variant}\">");
            escape_text(out, s);
            out.push_str("</mn>");
        }
        other => render(other, out),
    }
}

/// Symbol table: command name → (unicode, force-upright-identifier, is-operator).
fn symbol(name: &str) -> Option<(&'static str, bool, bool)> {
    // (text, normal-ident, is_op)
    let id = |s| Some((s, false, false));
    let op = |s| Some((s, false, true));
    match name {
        // lowercase Greek
        "alpha" => id("\u{3b1}"),
        "beta" => id("\u{3b2}"),
        "gamma" => id("\u{3b3}"),
        "delta" => id("\u{3b4}"),
        "epsilon" | "varepsilon" => id("\u{3b5}"),
        "zeta" => id("\u{3b6}"),
        "eta" => id("\u{3b7}"),
        "theta" | "vartheta" => id("\u{3b8}"),
        "iota" => id("\u{3b9}"),
        "kappa" => id("\u{3ba}"),
        "lambda" => id("\u{3bb}"),
        "mu" => id("\u{3bc}"),
        "nu" => id("\u{3bd}"),
        "xi" => id("\u{3be}"),
        "pi" | "varpi" => id("\u{3c0}"),
        "rho" | "varrho" => id("\u{3c1}"),
        "sigma" | "varsigma" => id("\u{3c3}"),
        "tau" => id("\u{3c4}"),
        "upsilon" => id("\u{3c5}"),
        "phi" | "varphi" => id("\u{3c6}"),
        "chi" => id("\u{3c7}"),
        "psi" => id("\u{3c8}"),
        "omega" => id("\u{3c9}"),
        // uppercase Greek
        "Gamma" => id("\u{393}"),
        "Delta" => id("\u{394}"),
        "Theta" => id("\u{398}"),
        "Lambda" => id("\u{39b}"),
        "Xi" => id("\u{39e}"),
        "Pi" => id("\u{3a0}"),
        "Sigma" => id("\u{3a3}"),
        "Upsilon" => id("\u{3a5}"),
        "Phi" => id("\u{3a6}"),
        "Psi" => id("\u{3a8}"),
        "Omega" => id("\u{3a9}"),
        // big operators (is_op so limits attach as under/over)
        "sum" => op("\u{2211}"),
        "prod" => op("\u{220f}"),
        "coprod" => op("\u{2210}"),
        "int" => op("\u{222b}"),
        "oint" => op("\u{222e}"),
        "iint" => op("\u{222c}"),
        "bigcup" => op("\u{22c3}"),
        "bigcap" => op("\u{22c2}"),
        "bigoplus" => op("\u{2a01}"),
        "bigotimes" => op("\u{2a02}"),
        // binary / relations
        "times" => op("\u{d7}"),
        "div" => op("\u{f7}"),
        "cdot" => op("\u{22c5}"),
        "pm" => op("\u{b1}"),
        "mp" => op("\u{2213}"),
        "ast" => op("\u{2217}"),
        "star" => op("\u{22c6}"),
        "circ" => op("\u{2218}"),
        "bullet" => op("\u{2219}"),
        "oplus" => op("\u{2295}"),
        "otimes" => op("\u{2297}"),
        "odot" => op("\u{2299}"),
        "ominus" => op("\u{2296}"),
        "oslash" => op("\u{2298}"),
        "leq" | "le" => op("\u{2264}"),
        "geq" | "ge" => op("\u{2265}"),
        "neq" | "ne" => op("\u{2260}"),
        "approx" => op("\u{2248}"),
        "equiv" => op("\u{2261}"),
        "cong" => op("\u{2245}"),
        "sim" => op("\u{223c}"),
        "simeq" => op("\u{2243}"),
        "propto" => op("\u{221d}"),
        "ll" => op("\u{226a}"),
        "gg" => op("\u{226b}"),
        "prec" => op("\u{227a}"),
        "succ" => op("\u{227b}"),
        "preceq" => op("\u{227c}"),
        "succeq" => op("\u{227d}"),
        "subset" => op("\u{2282}"),
        "supset" => op("\u{2283}"),
        "subseteq" => op("\u{2286}"),
        "supseteq" => op("\u{2287}"),
        "in" => op("\u{2208}"),
        "notin" => op("\u{2209}"),
        "ni" => op("\u{220b}"),
        "cup" => op("\u{222a}"),
        "cap" => op("\u{2229}"),
        "setminus" => op("\u{2216}"),
        "wedge" | "land" => op("\u{2227}"),
        "vee" | "lor" => op("\u{2228}"),
        "neg" | "lnot" => op("\u{ac}"),
        "forall" => op("\u{2200}"),
        "exists" => op("\u{2203}"),
        "nexists" => op("\u{2204}"),
        // arrows
        "to" | "rightarrow" | "righarrow" => op("\u{2192}"),
        "leftarrow" | "gets" => op("\u{2190}"),
        "leftrightarrow" => op("\u{2194}"),
        "Rightarrow" | "implies" => op("\u{21d2}"),
        "Leftarrow" => op("\u{21d0}"),
        "Leftrightarrow" | "iff" => op("\u{21d4}"),
        "mapsto" => op("\u{21a6}"),
        "uparrow" => op("\u{2191}"),
        "downarrow" => op("\u{2193}"),
        "longrightarrow" => op("\u{27f6}"),
        "longleftarrow" => op("\u{27f5}"),
        "longleftrightarrow" => op("\u{27f7}"),
        "Longrightarrow" => op("\u{27f9}"),
        "Longleftarrow" => op("\u{27f8}"),
        "Longleftrightarrow" => op("\u{27fa}"),
        // misc symbols (identifiers)
        "infty" => id("\u{221e}"),
        "partial" => id("\u{2202}"),
        "nabla" => id("\u{2207}"),
        "emptyset" | "varnothing" => id("\u{2205}"),
        "aleph" => id("\u{2135}"),
        "hbar" => id("\u{210f}"),
        "ell" => id("\u{2113}"),
        "Re" => id("\u{211c}"),
        "Im" => id("\u{2111}"),
        "wp" => id("\u{2118}"),
        "angle" => id("\u{2220}"),
        "triangle" => id("\u{25b3}"),
        "cdots" => op("\u{22ef}"),
        "ldots" | "dots" => op("\u{2026}"),
        "vdots" => op("\u{22ee}"),
        "ddots" => op("\u{22f1}"),
        "prime" => op("\u{2032}"),
        "dagger" => op("\u{2020}"),
        "perp" => op("\u{22a5}"),
        "parallel" => op("\u{2225}"),
        "mid" => op("\u{2223}"),
        "nmid" => op("\u{2224}"),
        "top" => id("\u{22a4}"),
        "bot" => id("\u{22a5}"),
        "models" => op("\u{22a8}"),
        "vdash" => op("\u{22a2}"),
        "dashv" => op("\u{22a3}"),
        "langle" => op("\u{27e8}"),
        "rangle" => op("\u{27e9}"),
        "lfloor" => op("\u{230a}"),
        "rfloor" => op("\u{230b}"),
        "lceil" => op("\u{2308}"),
        "rceil" => op("\u{2309}"),
        "backslash" => op("\\"),
        "%" => op("%"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ml(s: &str) -> String {
        to_mathml(s, false)
    }

    #[test]
    fn basic_atoms() {
        assert!(ml("x").contains("<mi>x</mi>"));
        assert!(ml("42").contains("<mn>42</mn>"));
        assert!(ml("a+b").contains("<mi>a</mi><mo>+</mo><mi>b</mi>"));
    }

    #[test]
    fn scripts() {
        assert!(ml("x^2").contains("<msup><mi>x</mi><mn>2</mn></msup>"));
        assert!(ml("a_i").contains("<msub><mi>a</mi><mi>i</mi></msub>"));
        assert!(ml("x_i^2").contains("<msubsup>"));
    }

    #[test]
    fn fraction_and_sqrt() {
        assert!(ml("\\frac{a}{b}").contains("<mfrac>"));
        assert!(ml("\\sqrt{x}").contains("<msqrt><mi>x</mi></msqrt>"));
        assert!(ml("\\sqrt[3]{x}").contains("<mroot>"));
        // `\frac12` takes single-token args (TeX: `\frac{1}{2}`).
        assert!(
            ml("\\frac12").contains("<mfrac><mn>1</mn><mn>2</mn></mfrac>"),
            "{}",
            ml("\\frac12")
        );
    }

    #[test]
    fn accents_render_as_mover() {
        // `\hat{x}` → an accented mover, not the literal text "hat".
        let m = ml("\\hat{x}");
        assert!(
            m.contains("<mover accent=\"true\"><mi>x</mi><mo>^</mo></mover>"),
            "{m}"
        );
        assert!(!m.contains(">hat<"), "must not leak the command name: {m}");
        // `\vec` uses the combining vector arrow; `\bar` the macron.
        assert!(ml("\\vec{v}").contains("\u{20D7}"), "{}", ml("\\vec{v}"));
        assert!(ml("\\bar{y}").contains("\u{00AF}"), "{}", ml("\\bar{y}"));
        // Wide hat is stretchy.
        assert!(ml("\\widehat{AB}").contains("<mo stretchy=\"true\">^</mo>"));
        // An accent still takes a script: `\hat{x}^2` wraps the mover in an msup.
        assert!(
            ml("\\hat{x}^2").contains("<msup><mover"),
            "{}",
            ml("\\hat{x}^2")
        );
    }

    #[test]
    fn overline_underline_use_a_stretching_css_rule() {
        // A CSS border spans the whole group (so it stretches, unlike a macron).
        let o = ml("\\overline{AB}");
        assert!(o.contains("border-top:0.06em solid currentColor"), "{o}");
        assert!(
            o.contains(">A</mi>") || o.contains(">A<"),
            "content kept: {o}"
        );
        let u = ml("\\underline{xy}");
        assert!(u.contains("border-bottom:0.06em solid currentColor"), "{u}");
        // `\bar` (a single-char accent) still uses the `<mover>` macron, not a rule.
        assert!(
            ml("\\bar{y}").contains("<mover accent=\"true\">"),
            "{}",
            ml("\\bar{y}")
        );
    }

    #[test]
    fn overset_underset_and_braces() {
        // overset/underset stack a full-size expression over/under the base.
        let o = ml("\\overset{!}{=}");
        assert!(
            o.contains("<mover>") && o.contains("<mo>=</mo>") && o.contains("<mo>!</mo>"),
            "{o}"
        );
        let u = ml("\\underset{x}{lim}");
        assert!(u.contains("<munder>"), "{u}");
        // over/underbrace are stretchy accents using the curly-bracket glyphs.
        assert!(
            ml("\\overbrace{a+b}").contains("\u{23DE}"),
            "{}",
            ml("\\overbrace{a+b}")
        );
        let ub = ml("\\underbrace{a+b}");
        assert!(
            ub.contains("<munder accent=\"true\">") && ub.contains("\u{23DF}"),
            "{ub}"
        );
    }

    #[test]
    fn binom_is_barless_fraction_in_parens() {
        let m = ml("\\binom{n}{k}");
        assert!(m.contains("<mfrac linethickness=\"0\">"), "{m}");
        assert!(m.contains("<mn>n</mn>") || m.contains("<mi>n</mi>"), "{m}");
        assert!(
            m.contains("<mo stretchy=\"true\">(</mo>") && m.contains(">)</mo>"),
            "{m}"
        );
    }

    #[test]
    fn greek_and_symbols() {
        assert!(ml("\\alpha").contains("\u{3b1}"));
        assert!(ml("\\pi r^2").contains("\u{3c0}"));
        assert!(ml("a \\leq b").contains("\u{2264}"));
        assert!(ml("\\infty").contains("\u{221e}"));
    }

    #[test]
    fn modular_arithmetic_pmod_and_bmod() {
        // `\pmod{n}` renders a parenthesised "(mod n)".
        let p = ml("a \\equiv b \\pmod{n}");
        assert!(
            p.contains(">mod</mi>"),
            "pmod shows the 'mod' operator: {p}"
        );
        assert!(
            p.contains(">(</mo>") && p.contains(">)</mo>"),
            "parenthesised: {p}"
        );
        assert!(p.contains(">n</mi>"), "the modulus argument: {p}");
        // `\bmod` renders the bare "mod" — never the literal command name "bmod".
        let b = ml("a \\bmod b");
        assert!(b.contains(">mod</mi>"), "bmod → mod: {b}");
        assert!(
            !b.contains("bmod"),
            "must not leak the literal command: {b}"
        );
    }

    #[test]
    fn extended_symbol_coverage() {
        // Previously-missing symbols now resolve instead of degrading to their
        // literal command name (e.g. `\mid` → "mid").
        assert!(ml("\\Upsilon").contains("\u{3a5}"), "uppercase Upsilon");
        assert!(
            ml("\\{ x \\mid x > 0 \\}").contains("\u{2223}"),
            "set-builder mid"
        );
        assert!(ml("a \\nmid b").contains("\u{2224}"));
        assert!(ml("\\top \\bot").contains("\u{22a4}") && ml("\\top \\bot").contains("\u{22a5}"));
        assert!(
            ml("\\Gamma \\models \\phi").contains("\u{22a8}"),
            "models (⊨)"
        );
        assert!(
            ml("\\Gamma \\vdash \\phi").contains("\u{22a2}"),
            "vdash (⊢)"
        );
        assert!(ml("a \\preceq b").contains("\u{227c}") && ml("a \\succ b").contains("\u{227b}"));
        assert!(ml("A \\odot B").contains("\u{2299}"));
        assert!(ml("p \\Longrightarrow q").contains("\u{27f9}"));
        // The command name must not leak into the output when it now resolves.
        assert!(!ml("a \\mid b").contains("mid"), "resolved, not literal");
    }

    #[test]
    fn sum_uses_under_over() {
        let m = to_mathml("\\sum_{i=1}^{n} i", true);
        assert!(m.contains("<munderover>"), "{m}");
        assert!(m.contains("display=\"block\""));
    }

    #[test]
    fn left_right_fences() {
        let m = ml("\\left( \\frac{a}{b} \\right)");
        assert!(m.contains("stretchy=\"true\""), "{m}");
    }

    #[test]
    fn matrix_environment() {
        let m = ml("\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}");
        assert!(m.contains("<mtable>"), "{m}");
        assert_eq!(m.matches("<mtr>").count(), 2, "{m}");
        assert_eq!(m.matches("<mtd>").count(), 4, "{m}");
    }

    #[test]
    fn aligned_environment_aligns_columns_at_the_ampersand() {
        let m = ml("\\begin{aligned} a &= b \\\\ c &= d \\end{aligned}");
        assert!(m.contains("<mtable columnalign=\"right left\">"), "{m}");
        assert_eq!(m.matches("<mtr>").count(), 2, "{m}");
        // A matrix environment stays centered (no columnalign).
        let mat = ml("\\begin{pmatrix} a & b \\end{pmatrix}");
        assert!(
            mat.contains("<mtable>") && !mat.contains("columnalign"),
            "{mat}"
        );
    }

    #[test]
    fn cases_environment_is_left_aligned_with_a_brace() {
        let m = ml("\\begin{cases} 0 & x < 0 \\\\ 1 & x \\geq 0 \\end{cases}");
        // Left-aligned columns (piecewise) and a single opening brace, two rows.
        assert!(m.contains("<mtable columnalign=\"left left\">"), "{m}");
        assert!(
            m.contains("<mo stretchy=\"true\">{</mo>"),
            "opening brace: {m}"
        );
        assert_eq!(m.matches("<mtr>").count(), 2, "{m}");
    }

    #[test]
    fn boxed_frames_its_content() {
        let m = ml("\\boxed{x = 42}");
        assert!(m.contains("border:1px solid"), "border applied: {m}");
        assert!(m.contains("<mn>42</mn>"), "content rendered: {m}");
        // The command name is not leaked as literal text.
        assert!(!m.contains(">boxed<"), "leaked command name: {m}");
    }

    #[test]
    fn array_environment_honours_column_spec() {
        // `{rcl}` → per-column alignment, and the spec is consumed (not a cell).
        let m = ml("\\begin{array}{rcl} x & = & y \\\\ p & = & q \\end{array}");
        assert!(
            m.contains("<mtable columnalign=\"right center left\">"),
            "{m}"
        );
        assert_eq!(m.matches("<mtd>").count(), 6, "2 rows × 3 cols: {m}");
        // None of r/c/l appears as content (the column spec didn't leak in).
        assert!(
            !m.contains("<mi>r</mi>") && !m.contains("<mi>c</mi>") && !m.contains("<mi>l</mi>"),
            "spec leaked as content: {m}"
        );
    }

    #[test]
    fn text_and_styles() {
        assert!(ml("\\text{if } x").contains("<mtext>if </mtext>"));
        assert!(ml("\\mathbb{R}").contains("mathvariant=\"double-struck\""));
    }

    #[test]
    fn escapes_dangerous_content() {
        // `<` as an operator and any stray markup is escaped, never raw.
        let m = ml("a < b");
        assert!(m.contains("&lt;"), "{m}");
        assert!(!m.contains("<script"));
        let m = ml("\\text{<script>}");
        assert!(m.contains("&lt;script&gt;"), "{m}");
    }

    #[test]
    fn never_panics_on_malformed() {
        for s in [
            "\\frac{",
            "\\frac",
            "^",
            "_{",
            "\\sqrt[",
            "\\left(",
            "\\begin{pmatrix} a &",
            "}}}}",
            "\\unknowncmd x",
            "\\\\\\\\",
            "{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{",
        ] {
            let _ = to_mathml(s, false);
            let _ = to_mathml(s, true);
        }
    }
}
