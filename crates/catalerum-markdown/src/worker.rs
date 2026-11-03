//! The render engine behind the web workbench's Markdown Web Worker.
//!
//! The browser main thread hands Markdown to a dedicated worker
//! (`catalerum-markdown-worker`) so parsing a long reply, a mermaid diagram or a
//! big note never blocks input or paint. This module is the worker's whole
//! brain, kept free of any JS binding so it is tested natively and reused as the
//! web client's in-thread fallback when no worker can start:
//!
//! - [`Request::Render`] is a one-shot `to_html`.
//! - [`Request::Append`] feeds a delta into a long-lived stream session keyed by
//!   id (one per streaming chat reply). The session holds the full text and a
//!   [`StreamRenderer`], so only deltas cross the thread boundary, and it
//!   answers only when a new stable block commits.
//! - [`Request::Drop`] frees a session.
//!
//! The wire encoding (a JS array of `[op, id, text]`) lives with the two JS-side
//! ends. [`op`] holds the opcodes they share.

use std::collections::HashMap;

use crate::StreamRenderer;

/// Opcodes shared by the worker and its main-thread client.
pub mod op {
    /// Request `[RENDER, id, markdown]` → reply `[RENDER, id, html]`.
    pub const RENDER: u32 = 0;
    /// Request `[APPEND, stream, delta]` → reply `[APPEND, stream, html_chunk, committed]`
    /// (sent only when the stable prefix grew).
    pub const APPEND: u32 = 1;
    /// Request `[DROP, stream]`. No reply.
    pub const DROP: u32 = 2;
    /// Reply `[READY]`, posted once the worker's wasm has initialised and its
    /// message handler is installed. Messages posted before it would be lost.
    pub const READY: u32 = 3;
}

/// A decoded request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request<'a> {
    Render { id: u32, markdown: &'a str },
    Append { stream: u32, delta: &'a str },
    Drop { stream: u32 },
}

/// A reply to post back to the main thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// The full HTML for a [`Request::Render`].
    Rendered { id: u32, html: String },
    /// A stream session's stable prefix grew. `html` is the newly committed HTML
    /// only (append it to what came before); `committed` is the byte length of
    /// the stream text it covers, so the caller shows `text[committed..]` as the
    /// still-open plain-text tail.
    Committed {
        stream: u32,
        html: String,
        committed: usize,
    },
}

#[derive(Default)]
struct Session {
    text: String,
    renderer: StreamRenderer,
}

/// Request handler. Holds the open stream sessions.
#[derive(Default)]
pub struct Engine {
    sessions: HashMap<u32, Session>,
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle one request, returning the reply to post, if any.
    pub fn handle(&mut self, request: Request<'_>) -> Option<Reply> {
        match request {
            Request::Render { id, markdown } => Some(Reply::Rendered {
                id,
                html: crate::to_html(markdown),
            }),
            Request::Append { stream, delta } => {
                let session = self.sessions.entry(stream).or_default();
                session.text.push_str(delta);
                let before = session.renderer.html().len();
                let tail = session.renderer.update(&session.text).len();
                let html = &session.renderer.html()[before..];
                (!html.is_empty()).then(|| Reply::Committed {
                    stream,
                    html: html.to_string(),
                    committed: session.text.len() - tail,
                })
            }
            Request::Drop { stream } => {
                self.sessions.remove(&stream);
                None
            }
        }
    }

    /// Number of open stream sessions.
    pub fn open_streams(&self) -> usize {
        self.sessions.len()
    }
}
