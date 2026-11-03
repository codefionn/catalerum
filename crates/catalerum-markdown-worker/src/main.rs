//! Dedicated Web Worker that renders Markdown for the catalerum workbench.
//!
//! The JS glue is wasm-bindgen's `no-modules` build, started by Trunk's loader
//! shim (`importScripts` + `wasm_bindgen(...)`). `main` runs once the module is
//! ready: it installs the message handler, then posts `[READY]` so the
//! main-thread client (`catalerum-web/src/md_worker.rs`) knows messages will no
//! longer be dropped. Each request is `[op, id, text]`; the logic lives in
//! [`catalerum_markdown::worker::Engine`].

#[cfg(target_arch = "wasm32")]
fn main() {
    use std::cell::RefCell;

    use catalerum_markdown::worker::{op, Engine, Reply, Request};
    use js_sys::Array;
    use wasm_bindgen::prelude::*;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let engine = RefCell::new(Engine::new());
    let reply_scope = scope.clone();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
        let Ok(msg) = ev.data().dyn_into::<Array>() else {
            return;
        };
        let num = |i: u32| msg.get(i).as_f64().map_or(0, |n| n as u32);
        let text = msg.get(2).as_string().unwrap_or_default();
        let (kind, id) = (num(0), num(1));
        let request = match kind {
            op::RENDER => Request::Render { id, markdown: &text },
            op::APPEND => Request::Append { stream: id, delta: &text },
            op::DROP => Request::Drop { stream: id },
            _ => return,
        };
        let reply = match engine.borrow_mut().handle(request) {
            Some(Reply::Rendered { id, html }) => {
                Array::of3(&op::RENDER.into(), &id.into(), &html.into())
            }
            Some(Reply::Committed { stream, html, committed }) => Array::of4(
                &op::APPEND.into(),
                &stream.into(),
                &html.into(),
                &(committed as f64).into(),
            ),
            None => return,
        };
        // Posting only fails for uncloneable data; arrays of strings/numbers are fine.
        let _ = reply_scope.post_message(&reply);
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
    let _ = scope.post_message(&Array::of1(&op::READY.into()));
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {}
