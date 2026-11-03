//! Main-thread client for the Markdown Web Worker.
//!
//! All Markdown → HTML rendering in the workbench goes through here so parsing
//! never runs on the UI thread. The worker is `catalerum-markdown-worker`, a
//! separate small wasm bundle that Trunk builds next to the app; its hashed
//! loader URL arrives in the `catalerum-md-worker` meta tag (see
//! `inject-md-worker.sh`).
//!
//! Two kinds of work:
//!
//! - [`html_or_request`] renders a whole document. Finished HTML lands in a
//!   bounded cache, so the reactive closures that re-run on every row change get
//!   a synchronous hit after the first render.
//! - [`MdStream`] is a streaming chat reply. Only deltas cross to the worker,
//!   which answers when a new stable block commits.
//!
//! When no worker can start (no meta tag, script 404, worker error, or no
//! `READY` within [`READY_TIMEOUT_MS`]), the client falls back to rendering
//! in-thread with the same [`Engine`] and replays everything still pending.
//! Natively (unit tests) it is always in-thread, so results are synchronous.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use catalerum_markdown::worker::{Engine, Reply, Request};

/// How long the worker may take to load before we give up and render in-thread.
#[cfg(target_arch = "wasm32")]
const READY_TIMEOUT_MS: u32 = 10_000;

/// Cache budget, counted as Markdown + HTML bytes.
const CACHE_BUDGET: usize = 4 << 20;

/// What a stream session's callback receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    /// Throw away the HTML received so far before appending `html`.
    pub replace: bool,
    /// Newly committed HTML, to append.
    pub html: String,
    /// Byte length of the stream text the HTML so far covers. Show
    /// `text[committed..]` as the plain-text tail.
    pub committed: usize,
}

type RenderCallback = Box<dyn FnOnce(Rc<str>)>;

struct Inflight {
    markdown: Rc<str>,
    waiters: Vec<RenderCallback>,
}

struct StreamEntry {
    /// The full text sent so far, kept to re-feed an in-thread engine on failover.
    sent: String,
    on_chunk: Rc<dyn Fn(StreamChunk)>,
}

enum Backend {
    /// No request yet; the worker spawns on first use (or [`warm`]).
    Idle,
    /// Worker spawned, waiting for `READY`. Messages queue here meanwhile.
    #[cfg(target_arch = "wasm32")]
    Starting {
        worker: web_sys::Worker,
        queue: Vec<wasm_bindgen::JsValue>,
    },
    #[cfg(target_arch = "wasm32")]
    Ready(web_sys::Worker),
    /// Rendering on this thread.
    Local(Engine),
}

/// Deferred work to run once the client borrow is released, so callbacks may
/// call back into the client.
enum Deliver {
    Render(Vec<RenderCallback>, Rc<str>),
    Chunk(Rc<dyn Fn(StreamChunk)>, StreamChunk),
}

impl Deliver {
    fn run(self) {
        match self {
            Deliver::Render(waiters, html) => {
                for w in waiters {
                    w(html.clone());
                }
            }
            Deliver::Chunk(f, chunk) => f(chunk),
        }
    }
}

#[derive(Default)]
struct HtmlCache {
    map: HashMap<Rc<str>, Rc<str>>,
    order: VecDeque<Rc<str>>,
    bytes: usize,
}

impl HtmlCache {
    fn get(&self, md: &str) -> Option<Rc<str>> {
        self.map.get(md).cloned()
    }

    fn insert(&mut self, md: Rc<str>, html: Rc<str>) {
        let size = md.len() + html.len();
        if size > CACHE_BUDGET || self.map.contains_key(&md) {
            return;
        }
        while self.bytes + size > CACHE_BUDGET {
            let Some(old) = self.order.pop_front() else { break };
            if let Some(h) = self.map.remove(&old) {
                self.bytes -= old.len() + h.len();
            }
        }
        self.bytes += size;
        self.order.push_back(md.clone());
        self.map.insert(md, html);
    }
}

struct Client {
    backend: Backend,
    next_id: u32,
    inflight: HashMap<u32, Inflight>,
    inflight_by_md: HashMap<Rc<str>, u32>,
    streams: HashMap<u32, StreamEntry>,
    cache: HtmlCache,
}

thread_local! {
    static CLIENT: RefCell<Client> = RefCell::new(Client {
        backend: Backend::Idle,
        next_id: 1,
        inflight: HashMap::new(),
        inflight_by_md: HashMap::new(),
        streams: HashMap::new(),
        cache: HtmlCache::default(),
    });
}

fn with_client<R>(f: impl FnOnce(&mut Client) -> (R, Vec<Deliver>)) -> R {
    let (out, deliveries) = CLIENT.with(|c| f(&mut c.borrow_mut()));
    for d in deliveries {
        d.run();
    }
    out
}

impl Client {
    fn alloc_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    fn ensure_started(&mut self) {
        if matches!(self.backend, Backend::Idle) {
            self.backend = spawn_backend();
        }
    }

    /// Send a request to the worker, or handle it now on the local engine.
    fn send(&mut self, request: Request<'_>, out: &mut Vec<Deliver>) {
        self.ensure_started();
        match &mut self.backend {
            Backend::Local(engine) => {
                if let Some(reply) = engine.handle(request) {
                    self.on_reply(reply, out);
                }
            }
            #[cfg(target_arch = "wasm32")]
            Backend::Starting { queue, .. } => queue.push(wire::encode(request)),
            #[cfg(target_arch = "wasm32")]
            Backend::Ready(worker) => {
                if worker.post_message(&wire::encode(request)).is_err() {
                    // Treat an unpostable worker as dead.
                    out.extend(self.fail_over());
                }
            }
            Backend::Idle => unreachable!("ensure_started leaves Idle"),
        }
    }

    fn on_reply(&mut self, reply: Reply, out: &mut Vec<Deliver>) {
        match reply {
            Reply::Rendered { id, html } => {
                let Some(inflight) = self.inflight.remove(&id) else { return };
                self.inflight_by_md.remove(&inflight.markdown);
                let html: Rc<str> = html.into();
                self.cache.insert(inflight.markdown, html.clone());
                out.push(Deliver::Render(inflight.waiters, html));
            }
            Reply::Committed { stream, html, committed } => {
                // A reply for a dropped/reset session is stale; ignore it.
                if let Some(entry) = self.streams.get(&stream) {
                    out.push(Deliver::Chunk(
                        entry.on_chunk.clone(),
                        StreamChunk { replace: false, html, committed },
                    ));
                }
            }
        }
    }

    /// Switch to in-thread rendering and redo everything the worker still owed.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    fn fail_over(&mut self) -> Vec<Deliver> {
        if matches!(self.backend, Backend::Local(_)) {
            return Vec::new();
        }
        #[cfg(target_arch = "wasm32")]
        if let Backend::Starting { worker, .. } | Backend::Ready(worker) = &self.backend {
            worker.set_onmessage(None);
            worker.set_onerror(None);
            worker.terminate();
        }
        let mut engine = Engine::new();
        let mut out = Vec::new();
        let pending: Vec<(u32, Rc<str>)> =
            self.inflight.iter().map(|(id, f)| (*id, f.markdown.clone())).collect();
        for (id, md) in pending {
            if let Some(reply) = engine.handle(Request::Render { id, markdown: &md }) {
                self.on_reply(reply, &mut out);
            }
        }
        for (id, entry) in &self.streams {
            let committed = match engine.handle(Request::Append { stream: *id, delta: &entry.sent }) {
                Some(Reply::Committed { html, committed, .. }) => (html, committed),
                _ => (String::new(), 0),
            };
            out.push(Deliver::Chunk(
                entry.on_chunk.clone(),
                StreamChunk { replace: true, html: committed.0, committed: committed.1 },
            ));
        }
        self.backend = Backend::Local(engine);
        out
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_backend() -> Backend {
    Backend::Local(Engine::new())
}

#[cfg(target_arch = "wasm32")]
fn spawn_backend() -> Backend {
    match wire::spawn_worker() {
        Some(worker) => Backend::Starting { worker, queue: Vec::new() },
        None => Backend::Local(Engine::new()),
    }
}

/// Start the worker now so the first render doesn't wait for it to load.
pub fn warm() {
    with_client(|c| {
        c.ensure_started();
        ((), Vec::new())
    });
}

/// The HTML for `markdown` if it's cached. Otherwise starts a render (or joins
/// an identical one in flight) and returns `None`; `on_ready` runs with the
/// HTML once it arrives. When the client renders in-thread the render finishes
/// inside this call: the HTML is returned *and* `on_ready` runs right after.
pub fn html_or_request(markdown: &str, on_ready: impl FnOnce(Rc<str>) + 'static) -> Option<Rc<str>> {
    with_client(|c| {
        if let Some(html) = c.cache.get(markdown) {
            return (Some(html), Vec::new());
        }
        if let Some(f) = c.inflight_by_md.get(markdown).and_then(|id| c.inflight.get_mut(id)) {
            f.waiters.push(Box::new(on_ready));
            return (None, Vec::new());
        }
        let id = c.alloc_id();
        let md: Rc<str> = markdown.into();
        let waiters: Vec<RenderCallback> = vec![Box::new(on_ready)];
        c.inflight.insert(id, Inflight { markdown: md.clone(), waiters });
        c.inflight_by_md.insert(md, id);
        let mut out = Vec::new();
        c.send(Request::Render { id, markdown }, &mut out);
        (c.cache.get(markdown), out)
    })
}

/// A streaming render session (one per streaming chat reply). Feed it the full
/// text so far with [`update`](Self::update); the callback receives the
/// committed HTML as it grows. Dropping the handle frees the worker session.
pub struct MdStream {
    id: u32,
}

impl MdStream {
    pub fn new(on_chunk: impl Fn(StreamChunk) + 'static) -> Self {
        let id = with_client(|c| {
            let id = c.alloc_id();
            c.streams.insert(id, StreamEntry { sent: String::new(), on_chunk: Rc::new(on_chunk) });
            (id, Vec::new())
        });
        Self { id }
    }

    /// Feed the full text received so far. Sends only the new suffix. If the
    /// text no longer extends what was sent (the reply was reset), the session
    /// restarts and the callback gets a `replace` chunk first.
    pub fn update(&mut self, full: &str) {
        let old = self.id;
        self.id = with_client(|c| {
            let mut out = Vec::new();
            let Some(entry) = c.streams.get_mut(&old) else { return (old, out) };
            let (id, delta) = if full.starts_with(entry.sent.as_str()) {
                let delta = &full[entry.sent.len()..];
                entry.sent.push_str(delta);
                (old, delta)
            } else {
                let mut entry = c.streams.remove(&old).expect("checked above");
                c.send(Request::Drop { stream: old }, &mut out);
                out.push(Deliver::Chunk(
                    entry.on_chunk.clone(),
                    StreamChunk { replace: true, html: String::new(), committed: 0 },
                ));
                entry.sent = full.to_string();
                let id = c.alloc_id();
                c.streams.insert(id, entry);
                (id, full)
            };
            if !delta.is_empty() {
                c.send(Request::Append { stream: id, delta }, &mut out);
            }
            (id, out)
        });
    }
}

impl Drop for MdStream {
    fn drop(&mut self) {
        let id = self.id;
        // `try_with`: the thread-local may already be gone during teardown.
        let _ = CLIENT.try_with(|_| {
            with_client(|c| {
                let mut out = Vec::new();
                if c.streams.remove(&id).is_some() {
                    c.send(Request::Drop { stream: id }, &mut out);
                }
                ((), out)
            })
        });
    }
}

/// JS side: spawning the worker and the `[op, id, text]` array encoding.
#[cfg(target_arch = "wasm32")]
mod wire {
    use catalerum_markdown::worker::{op, Reply, Request};
    use js_sys::Array;
    use wasm_bindgen::prelude::*;
    use web_sys::{Event, MessageEvent, Worker};

    use super::{with_client, Backend, READY_TIMEOUT_MS};

    pub(super) fn encode(request: Request<'_>) -> JsValue {
        match request {
            Request::Render { id, markdown } => {
                Array::of3(&op::RENDER.into(), &id.into(), &markdown.into())
            }
            Request::Append { stream, delta } => {
                Array::of3(&op::APPEND.into(), &stream.into(), &delta.into())
            }
            Request::Drop { stream } => Array::of2(&op::DROP.into(), &stream.into()),
        }
        .into()
    }

    enum Message {
        Ready,
        Reply(Reply),
    }

    fn decode(data: JsValue) -> Option<Message> {
        let msg = data.dyn_into::<Array>().ok()?;
        let num = |i: u32| msg.get(i).as_f64();
        let kind = num(0)? as u32;
        if kind == op::READY {
            return Some(Message::Ready);
        }
        let id = num(1)? as u32;
        let html = msg.get(2).as_string()?;
        Some(Message::Reply(match kind {
            op::RENDER => Reply::Rendered { id, html },
            op::APPEND => Reply::Committed { stream: id, html, committed: num(3)? as usize },
            _ => return None,
        }))
    }

    fn worker_url() -> Option<String> {
        let url = web_sys::window()?
            .document()?
            .query_selector("meta[name=\"catalerum-md-worker\"]")
            .ok()??
            .get_attribute("content")?;
        // The placeholder survives when the post_build hook didn't run.
        (!url.is_empty() && !url.contains("__CATALERUM_MD_WORKER__")).then_some(url)
    }

    fn fail_over() {
        with_client(|c| ((), c.fail_over()));
    }

    pub(super) fn spawn_worker() -> Option<Worker> {
        let Some(url) = worker_url() else {
            leptos::logging::warn!("markdown worker: no loader URL; rendering on the main thread");
            return None;
        };
        let worker = match Worker::new(&url) {
            Ok(w) => w,
            Err(e) => {
                leptos::logging::warn!("markdown worker: spawn failed ({e:?}); rendering on the main thread");
                return None;
            }
        };
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(|ev: MessageEvent| {
            match decode(ev.data()) {
                Some(Message::Ready) => with_client(|c| {
                    if let Backend::Starting { worker, queue } =
                        std::mem::replace(&mut c.backend, Backend::Idle)
                    {
                        let ok = queue.iter().all(|m| worker.post_message(m).is_ok());
                        c.backend = Backend::Ready(worker);
                        if !ok {
                            return ((), c.fail_over());
                        }
                    }
                    ((), Vec::new())
                }),
                Some(Message::Reply(reply)) => with_client(|c| {
                    let mut out = Vec::new();
                    c.on_reply(reply, &mut out);
                    ((), out)
                }),
                None => {}
            }
        });
        let on_error = Closure::<dyn FnMut(Event)>::new(|ev: Event| {
            leptos::logging::warn!("markdown worker failed ({:?}); rendering on the main thread", ev.type_());
            fail_over();
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        // One worker lives for the whole page, so its handlers do too.
        on_message.forget();
        on_error.forget();
        gloo_timers::callback::Timeout::new(READY_TIMEOUT_MS, || {
            let stuck = super::CLIENT.with(|c| matches!(c.borrow().backend, Backend::Starting { .. }));
            if stuck {
                leptos::logging::warn!("markdown worker: no READY in time; rendering on the main thread");
                fail_over();
            }
        })
        .forget();
        Some(worker)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn renders_in_thread_natively_and_caches() {
        let called = Rc::new(Cell::new(false));
        let c = called.clone();
        let html = html_or_request("# Native **render**", move |_| c.set(true));
        assert_eq!(html.as_deref(), Some(catalerum_markdown::to_html("# Native **render**").as_str()));
        assert!(called.get(), "in-thread renders still notify");
        let again = html_or_request("# Native **render**", |_| panic!("cached"));
        assert!(Rc::ptr_eq(&html.unwrap(), &again.unwrap()));
    }

    #[test]
    fn stream_sends_deltas_and_resets_on_rewrite() {
        let got = Rc::new(RefCell::new((String::new(), 0usize)));
        let sink = got.clone();
        let resets = Rc::new(Cell::new(0));
        let r = resets.clone();
        let mut s = MdStream::new(move |c| {
            let mut g = sink.borrow_mut();
            if c.replace {
                g.0.clear();
                r.set(r.get() + 1);
            }
            g.0.push_str(&c.html);
            g.1 = c.committed;
        });
        s.update("# A\n\nhalf");
        s.update("# A\n\nhalf **b");
        s.update("# A\n\nhalf **b**\n\nnext");
        assert_eq!(got.borrow().0, catalerum_markdown::to_html("# A\n\nhalf **b**\n\n"));
        assert_eq!(got.borrow().1, "# A\n\nhalf **b**\n\n".len());
        s.update("Different\n\nx");
        assert_eq!(resets.get(), 1);
        assert_eq!(*got.borrow(), (catalerum_markdown::to_html("Different\n\n"), 11));
        drop(s);
        CLIENT.with(|c| assert!(c.borrow().streams.is_empty()));
    }

    #[test]
    fn cache_evicts_oldest_beyond_budget() {
        let mut cache = HtmlCache::default();
        let big = "x".repeat(CACHE_BUDGET / 3);
        for i in 0..4 {
            cache.insert(format!("{i}{big}").into(), "h".into());
        }
        assert!(cache.get(&format!("0{big}")).is_none());
        assert!(cache.get(&format!("3{big}")).is_some());
        assert!(cache.bytes <= CACHE_BUDGET);
    }
}
