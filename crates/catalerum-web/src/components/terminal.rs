//! The read-only terminal pane (SOUL §20). The agent drives terminals via tools;
//! this pane lets the user *watch*: pick an active session and tail its live
//! output (ANSI-stripped) over the [`TerminalSocket`](crate::ws::TerminalSocket)
//! WebSocket, auto-scrolling to the bottom.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use crate::api::TerminalSession;
use crate::auth;
use crate::rest;
use crate::ws::TerminalSocket;

/// Keep at most this much output in the pane (bounds wasm memory on a chatty
/// session); older text scrolls off.
const MAX_PANE_BYTES: usize = 256 * 1024;

/// A read-only terminal output pane with an active-session picker.
#[component]
pub fn TerminalPane() -> impl IntoView {
    let sessions = RwSignal::new(Vec::<TerminalSession>::new());
    let selected = RwSignal::new(Option::<String>::None);
    let term_text = RwSignal::new(String::new());
    let pane: NodeRef<leptos::html::Pre> = NodeRef::new();

    // Load the workspace's active sessions; default-select the first.
    let load = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_terminal_sessions(token.as_deref()).await {
                if selected.get_untracked().is_none() {
                    if let Some(first) = list.first() {
                        selected.set(Some(first.id.clone()));
                    }
                }
                sessions.set(list);
            }
        });
    };
    load();

    // (Re)connect whenever the selected session changes; stream stripped output
    // into `term_text`. A stale task (after a re-select) stops on the id guard,
    // dropping its socket (which closes it).
    Effect::new(move |_| {
        let Some(id) = selected.get() else {
            term_text.set(String::new());
            return;
        };
        term_text.set(String::new());
        spawn_local(async move {
            let token = auth::resolve_token();
            let mut socket = match TerminalSocket::connect(&id, token.as_deref()) {
                Ok(s) => s,
                Err(e) => {
                    term_text.set(format!("[failed to open terminal stream: {e}]"));
                    return;
                }
            };
            while let Some(chunk) = socket.next_chunk().await {
                // Stop if the user switched sessions.
                if selected.get_untracked().as_deref() != Some(id.as_str()) {
                    break;
                }
                term_text.update(|s| {
                    s.push_str(&chunk);
                    if s.len() > MAX_PANE_BYTES {
                        let drop_to = s.len() - MAX_PANE_BYTES;
                        let cut = (drop_to..=s.len())
                            .find(|&i| s.is_char_boundary(i))
                            .unwrap_or(s.len());
                        *s = s[cut..].to_string();
                    }
                });
            }
        });
    });

    // Auto-scroll to the bottom on every output change (recipe: flow.rs NodeRef).
    Effect::new(move |_| {
        term_text.track();
        if let Some(el) = pane.get_untracked() {
            let el: web_sys::Element = el.unchecked_into();
            el.set_scroll_top(el.scroll_height());
        }
    });

    view! {
        <div class="term-pane">
            <div class="term-pane-bar">
                <span class="term-pane-label">"Terminal"</span>
                <select
                    class="term-pane-select"
                    prop:value=move || selected.get().unwrap_or_default()
                    on:change=move |ev| {
                        let v = event_target_value(&ev);
                        selected.set(if v.is_empty() { None } else { Some(v) });
                    }
                >
                    <option value="">"Select a session…"</option>
                    <For
                        each=move || sessions.get()
                        key=|s| s.id.clone()
                        children=move |s| {
                            let short = s.id.get(..8).unwrap_or(&s.id).to_string();
                            let label = format!("{short} · {}", s.backend);
                            view! { <option value=s.id.clone()>{label}</option> }
                        }
                    />
                </select>
                <button
                    class="term-pane-refresh"
                    title="Refresh sessions"
                    on:click=move |_| load()
                >
                    "⟳"
                </button>
            </div>
            <pre node_ref=pane class="term-out">
                {move || term_text.get()}
            </pre>
        </div>
    }
}
