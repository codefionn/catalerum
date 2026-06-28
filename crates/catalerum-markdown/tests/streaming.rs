//! Streaming / incremental rendering behaviour.

use catalerum_markdown::{stable_boundary, to_html, StreamRenderer};

#[test]
fn boundary_after_blank_line() {
    // The stable prefix ends just after the blank line separating the two blocks.
    let full = "a\n\nb";
    assert_eq!(stable_boundary(full), 3); // "a\n\n"
}

#[test]
fn boundary_never_splits_an_open_fence() {
    // A blank line *inside* an unterminated fence is not a boundary.
    let full = "```\ncode\n\nmore";
    assert_eq!(stable_boundary(full), 0);
    // Once the fence closes and a blank follows, the whole block is stable.
    let full = "```\ncode\n```\n\nnext";
    assert_eq!(stable_boundary(full), "```\ncode\n```\n\n".len());
}

#[test]
fn update_renders_stable_prefix_and_returns_live_tail() {
    let mut r = StreamRenderer::new();
    let tail = r.update("# Title\n\nincomplete **bold");
    assert_eq!(r.html(), "<h1 id=\"title\">Title</h1>");
    assert_eq!(tail, "incomplete **bold");
    // The half-open emphasis never reaches the committed HTML.
    assert!(!r.html().contains("<strong>"));
}

#[test]
fn streaming_byte_by_byte_matches_full_render_for_block_separated_doc() {
    // Blocks separated by blank lines, no forward references, no loose lists: the
    // incremental result is identical to a one-shot render.
    let doc = "# H\n\npara one\n\npara two with `code`\n\n```rust\nlet x = 1;\n```\n\n- a\n- b\n";
    let mut r = StreamRenderer::new();
    let mut at = 0;
    while at < doc.len() {
        let mut end = at + 1;
        while !doc.is_char_boundary(end) {
            end += 1;
        }
        r.update(&doc[..end]);
        at = end;
    }
    r.finish(doc);
    assert_eq!(r.into_html(), to_html(doc));
}

#[test]
fn incremental_boundary_scan_matches_from_scratch() {
    // The resumed boundary scan (each delta scans only the new tail) must produce
    // byte-identical committed HTML to a renderer fed the same prefix from scratch,
    // at *every* prefix — across fences, blank lines, lists and multi-byte chars.
    let docs = [
        "# H\n\npara one\n\n```rust\nlet x=1;\n\n// blank inside fence\n```\n\nafter\n\n- a\n- b\n\n",
        "line\n\n```\ncode\n```\n\n> quote\n\nmore £€你 text\n\n| a | b |\n|-|-|\n| 1 | 2 |\n\n",
        "no blanks at all just one growing paragraph word by word here",
    ];
    for doc in docs {
        let mut inc = StreamRenderer::new();
        for (i, ch) in doc.char_indices() {
            let end = i + ch.len_utf8();
            inc.update(&doc[..end]);
            let mut once = StreamRenderer::new();
            once.update(&doc[..end]);
            assert_eq!(inc.html(), once.html(), "prefix {:?}", &doc[..end]);
        }
    }
}

#[test]
fn update_is_sound_when_buffer_shrinks() {
    // A reused renderer fed a shorter buffer (e.g. row reset) must not panic on the
    // internal `clamp(committed, len)`; the committed mark pulls back to the new end.
    let mut r = StreamRenderer::new();
    r.update("para one\n\npara two\n\n");
    // Shorter buffer than what was committed: must not panic; the returned tail is
    // a valid suffix of the new input (the committed mark pulls back to its end).
    let tail = r.update("x");
    assert!("x".ends_with(tail), "tail {tail:?} not a suffix of \"x\"");
    r.finish("x");
}

#[test]
fn finish_commits_unterminated_tail() {
    let mut r = StreamRenderer::new();
    r.update("```\nx");
    assert_eq!(r.html(), ""); // nothing stable yet
    r.finish("```\nx");
    assert!(
        r.html().contains("<pre><code>x\n</code></pre>"),
        "{}",
        r.html()
    );
}
