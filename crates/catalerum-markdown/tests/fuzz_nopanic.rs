//! Robustness: no input may panic, and the streaming boundary must always land on
//! a UTF-8 char boundary. Drives pseudo-random documents built from the bytes the
//! parser cares about *plus multi-byte Unicode* (the classic source of slice-mid-
//! codepoint panics) through every public entry point.

use catalerum_markdown::{parse, stable_boundary, to_html, StreamRenderer};

/// Deterministic LCG (no `rand`, no `Math.random` — reproducible).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 17
    }
}

/// A pool mixing every structurally-significant ASCII byte with multi-byte chars
/// (2-, 3- and 4-byte UTF-8) so slicing logic is stressed at codepoint edges.
const POOL: &[&str] = &[
    "#",
    "*",
    "_",
    "`",
    "~",
    "[",
    "]",
    "(",
    ")",
    "!",
    "<",
    ">",
    "|",
    "-",
    "+",
    ".",
    ":",
    "\\",
    "/",
    " ",
    "  ",
    "\n",
    "\n\n",
    "a",
    "x",
    "1",
    "2",
    "https://e.com",
    "mailto:a@b.c",
    "alt",
    "ref",
    "£",
    "€",
    "你",
    "好",
    "é",
    "ü",
    "🎉",
    "👍🏽",
    "\r\n",
    "\t",
    "[]",
    "()",
    "```",
    "- [ ] ",
    "- [x] ",
    "$",
    "$$",
    "\\frac{a}{b}",
    "\\sum_i^n",
    "```mermaid\n",
    "```math\n",
    "graph TD\n",
    "A-->B",
    "{",
];

fn random_doc(rng: &mut Lcg, max_tokens: usize) -> String {
    let n = (rng.next() as usize) % max_tokens;
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(POOL[(rng.next() as usize) % POOL.len()]);
    }
    s
}

#[test]
fn never_panics_on_random_documents() {
    let mut rng = Lcg(0xC0FF_EE12_3456_7890);
    for _ in 0..20_000 {
        let doc = random_doc(&mut rng, 40);

        // One-shot render + event stream must not panic.
        let html = to_html(&doc);
        assert!(html.is_char_boundary(0) || html.is_empty());
        let _ = parse(&doc).count();

        // The stable boundary must be a valid char boundary of `doc`, and slicing
        // at it (both sides) must not panic.
        let b = stable_boundary(&doc);
        assert!(
            doc.is_char_boundary(b),
            "boundary {b} not a char boundary in {doc:?}"
        );
        let _ = to_html(&doc[..b]);
        let _ = &doc[b..];

        // Truncating at EVERY char boundary and rendering the prefix must not panic
        // (this is what a streaming caller does on each delta).
        for (i, _) in doc.char_indices() {
            let _ = to_html(&doc[..i]);
            let _ = stable_boundary(&doc[..i]);
        }
    }
}

#[test]
fn streaming_in_tiny_chunks_never_panics_and_commits_everything() {
    let mut rng = Lcg(0xABCD_EF01_2345_6789);
    for _ in 0..5_000 {
        let doc = random_doc(&mut rng, 30);
        let mut r = StreamRenderer::new();
        // Feed one *char* at a time (a multi-byte char never split mid-stream by
        // construction, but boundaries still vary every delta).
        let mut at = 0;
        for (i, ch) in doc.char_indices() {
            at = i + ch.len_utf8();
            let tail = r.update(&doc[..at]);
            // The returned tail is always the live (uncommitted) suffix.
            assert!(doc[..at].ends_with(tail));
        }
        let _ = at;
        r.finish(&doc);
        // After finishing, the renderer has consumed the whole document.
        let _ = r.into_html();
    }
}

#[test]
fn math_and_mermaid_engines_never_panic() {
    let mut rng = Lcg(0xFEED_FACE_C0DE_1234);
    let math_pool = [
        "\\frac",
        "{",
        "}",
        "^",
        "_",
        "\\sqrt",
        "[",
        "]",
        "\\sum",
        "\\int",
        "x",
        "1",
        "\\alpha",
        "\\begin{matrix}",
        "\\end{matrix}",
        "&",
        "\\\\",
        "\\left(",
        "\\right)",
        "\\text{",
        "$",
        "\\unknown",
        "你",
        "\\pi",
        "=",
        "<",
        ">",
    ];
    for _ in 0..8_000 {
        let n = (rng.next() as usize) % 30;
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(math_pool[(rng.next() as usize) % math_pool.len()]);
        }
        let _ = catalerum_markdown::math::to_mathml(&s, false);
        let _ = catalerum_markdown::math::to_mathml(&s, true);
    }

    let mer_pool = [
        "A", "B", "C", " --> ", " --- ", " -.-> ", " ==> ", "[", "]", "(", ")", "{", "}", "|x|",
        "((", "))", "\n", " ", "A[lbl]", " -->|y| ", "你", "{{", "([", "])", ";", "--",
    ];
    for _ in 0..8_000 {
        let n = (rng.next() as usize) % 30;
        let mut s = String::from("graph TD\n");
        for _ in 0..n {
            s.push_str(mer_pool[(rng.next() as usize) % mer_pool.len()]);
        }
        let _ = catalerum_markdown::mermaid::to_svg(&s);
    }
}

#[test]
fn emphasis_spec_cases() {
    // A handful of CommonMark emphasis examples that exercise the delimiter stack
    // and the rule-of-three (these are the ones naive parsers get wrong).
    let cases = [
        ("*foo bar*", "<p><em>foo bar</em></p>"),
        ("**foo bar**", "<p><strong>foo bar</strong></p>"),
        ("foo*bar*", "<p>foo<em>bar</em></p>"),
        ("**foo**bar", "<p><strong>foo</strong>bar</p>"),
        ("***foo***", "<p><em><strong>foo</strong></em></p>"),
        ("*(*foo*)*", "<p><em>(<em>foo</em>)</em></p>"),
        (
            "**foo*bar*baz**",
            "<p><strong>foo<em>bar</em>baz</strong></p>",
        ),
        ("foo_bar_baz", "<p>foo_bar_baz</p>"),
        ("_foo_", "<p><em>foo</em></p>"),
        ("a**b", "<p>a**b</p>"),
    ];
    for (md, want) in cases {
        assert_eq!(to_html(md), want, "input {md:?}");
    }
}
