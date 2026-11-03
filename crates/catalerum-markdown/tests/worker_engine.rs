//! The Web Worker's request engine: one-shot renders and delta-fed stream sessions.

use catalerum_markdown::worker::{Engine, Reply, Request};
use catalerum_markdown::{to_html, StreamRenderer};

#[test]
fn render_matches_to_html() {
    let mut e = Engine::new();
    let md = "# Hi\n\n- **a**\n- `b`\n\n```mermaid\npie\n\"x\": 1\n```";
    assert_eq!(
        e.handle(Request::Render { id: 7, markdown: md }),
        Some(Reply::Rendered { id: 7, html: to_html(md) })
    );
}

#[test]
fn append_deltas_match_a_full_text_stream_renderer() {
    let full = "# Title\n\nSome **bold** text\n\n```rust\nfn x() {}\n\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\ntail *open";
    let mut e = Engine::new();
    let mut html = String::new();
    let mut committed = 0;
    // Feed it a few bytes at a time, splitting only on char boundaries.
    let mut start = 0;
    while start < full.len() {
        let mut end = (start + 3).min(full.len());
        while !full.is_char_boundary(end) {
            end += 1;
        }
        if let Some(Reply::Committed { stream, html: chunk, committed: c }) =
            e.handle(Request::Append { stream: 1, delta: &full[start..end] })
        {
            assert_eq!(stream, 1);
            assert!(c > committed, "replies only when the stable prefix grows");
            html.push_str(&chunk);
            committed = c;
        }
        start = end;
    }
    let mut oracle = StreamRenderer::new();
    let tail = oracle.update(full);
    assert_eq!(html, oracle.html());
    assert_eq!(&full[committed..], tail);
    assert_eq!(tail, "tail *open");
}

#[test]
fn sessions_are_independent_and_droppable() {
    let mut e = Engine::new();
    assert!(e.handle(Request::Append { stream: 1, delta: "a\n\n" }).is_some());
    assert!(e.handle(Request::Append { stream: 2, delta: "b" }).is_none());
    assert_eq!(e.open_streams(), 2);
    assert_eq!(e.handle(Request::Drop { stream: 1 }), None);
    assert_eq!(e.open_streams(), 1);
    // A dropped id starts from scratch if reused.
    assert_eq!(
        e.handle(Request::Append { stream: 1, delta: "c\n\n" }),
        Some(Reply::Committed { stream: 1, html: to_html("c\n\n"), committed: 3 })
    );
}
