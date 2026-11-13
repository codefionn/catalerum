#![recursion_limit = "256"]

//! catalerum-web — Leptos CSR workbench (SOUL §12). Compiled to wasm via Trunk.
//!
//! The workbench shell ([`components::Workbench`]) — a header, a left nav (Chat,
//! Calendar, and Notes active; Files / Tasks as placeholders), a working
//! streaming [`components::ChatPanel`] that talks to the API's `/ws/chat`
//! WebSocket, the M2 [`components::CalendarPanel`] (day-grouped agenda +
//! connect-calendar form), and the M3 [`components::NotesPanel`] (markdown notes
//! list + editor) over the REST surface.
//!
//! Modules:
//! - [`api`] — the WS/REST JSON contract + dev constants.
//! - [`auth`] — dev magic-link token resolution (`?token=` / `localStorage`).
//! - [`ws`] — the chat WebSocket transport ([`ws::ChatSocket`]).
//! - [`rest`] — the REST client (`/connections`, `/calendars`, `/events`, `/notes`).
//! - [`components`] — the shell and panels.
//! - [`md_worker`] — the Markdown Web Worker client (rendering off the UI thread).

pub mod api;
pub mod auth;
pub mod components;
pub mod md_worker;
pub mod rest;
pub mod strip_ansi;
pub mod ws;

pub use components::{
    AutomationsPanel, CalendarPanel, ChatPanel, ConversationsPanel, EmailPanel, FetchPanel,
    FilesPanel, GrantsPanel, GraphPanel, LoginView, MemoryPanel, NotesPanel, OnboardingPanel,
    ProfilesPanel, SkillsPanel, TasksPanel, Workbench, WorkspaceSwitcher,
};

use leptos::prelude::*;

/// Root component of the catalerum workbench. Injects the workbench stylesheet
/// and mounts either the [`Workbench`] shell (when a session exists) or the
/// [`LoginView`] sign-in surface.
#[component]
pub fn App() -> impl IntoView {
    // Reflect the persisted colour theme onto <html data-theme=…> before paint so
    // the workbench mounts in the chosen palette (default Midnight when unset).
    components::theme::init_theme();
    // Load the Markdown worker now so the first message doesn't wait for it.
    md_worker::warm();
    // A `?code=` one-time handoff (the dev magic-link / SSO browser login
    // redirects here with it, SOUL §18) is exchanged for the real session
    // bearer before any surface mounts — the session token never rides in a
    // URL. The code is read + scrubbed from the address bar first.
    if let Some(code) = auth::take_handoff_code() {
        return view! {
            <style>{STYLE}</style>
            <HandoffExchange code />
        }
        .into_any();
    }
    // Adopt an inbound bearer (`?token=`, e.g. dropped into the URL by the e2e
    // harness) and scrub it from the address bar, *then* decide the surface:
    // the workbench when a session resolves, else the minimal sign-in view (the
    // app otherwise renders panels that would only 401).
    auth::adopt_url_token();
    let authed = auth::resolve_token().is_some();
    view! {
        <style>{STYLE}</style>
        {if authed {
            view! { <Workbench /> }.into_any()
        } else {
            view! { <LoginView /> }.into_any()
        }}
    }
    .into_any()
}

/// The login-handoff surface (SOUL §18): exchanges a one-time `?code=` for the
/// session bearer (`POST /auth/exchange`), caches it, and reloads into the
/// workbench. A failed exchange (unknown/expired/consumed code) bounces to the
/// login view with the generic SSO error banner.
#[component]
fn HandoffExchange(code: String) -> impl IntoView {
    let exchange = LocalResource::new(move || {
        let code = code.clone();
        async move { rest::exchange_handoff_code(&code).await }
    });
    Effect::new(move || {
        let Some(result) = exchange.get() else {
            return;
        };
        let Some(window) = web_sys::window() else {
            return;
        };
        match result {
            Ok(session) => {
                auth::store_token(&session.token);
                // Reload into a clean boot: the token now resolves from storage.
                let _ = window.location().reload();
            }
            Err(_) => {
                // Land on the login view with the generic "sign-in didn't
                // complete" banner (never the raw error — same closed-enum
                // posture as the SSO callback).
                let _ = window.location().set_href("/?sso_error=failed");
            }
        }
    });
    view! {
        <div class="wb-login">
            <div class="wb-login-card">
                <p class="wb-login-hint">"Signing you in…"</p>
            </div>
        </div>
    }
}

/// Minimal embedded stylesheet for the M1 workbench. Inlined (rather than a
/// linked asset) so the CSR bundle is self-contained for dev / e2e.
///
/// The CSS lives in `src/style/*.css`, one file per section. The files are
/// concatenated in this order and cascade order matters, so append new rules
/// to the right section file rather than reordering the list. The leading
/// `"\n"` keeps the string byte-identical to the former inline raw string.
const STYLE: &str = concat!(
    "\n",
    include_str!("style/01-theme-tokens.css"),
    include_str!("style/02-shell.css"),
    include_str!("style/03-chat-sessions.css"),
    include_str!("style/04-panes.css"),
    include_str!("style/05-chat.css"),
    include_str!("style/06-voice.css"),
    include_str!("style/07-calendar.css"),
    include_str!("style/08-notes.css"),
    include_str!("style/09-files.css"),
    include_str!("style/10-skills-mcp-profiles.css"),
    include_str!("style/11-automations.css"),
    include_str!("style/12-grants.css"),
    include_str!("style/13-conversations.css"),
    include_str!("style/14-email.css"),
    include_str!("style/15-fetch.css"),
    include_str!("style/16-tasks.css"),
    include_str!("style/17-dialogs.css"),
    include_str!("style/18-memory.css"),
    include_str!("style/19-graph.css"),
    include_str!("style/20-flow.css"),
    include_str!("style/21-emerged-ui.css"),
    include_str!("style/22-charts.css"),
    include_str!("style/23-onboarding.css"),
    include_str!("style/24-theme-picker.css"),
    include_str!("style/25-mobile.css"),
);

/// wasm entrypoint, invoked by the Trunk-generated bootstrap.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
