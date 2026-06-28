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

pub mod api;
pub mod auth;
pub mod components;
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
const STYLE: &str = r#"
/* ── Theme tokens ───────────────────────────────────────────────────────────
   Every surface colour is a CSS custom property. The base :root is the default
   "Midnight" theme; alternate themes (below) override the same tokens under a
   [data-theme="…"] attribute the ThemePicker writes onto <html>. To keep a new
   colour theme-able, reach for a token here — never a raw hex in a rule.
   The "custom" theme has no static block here: its tokens are injected at
   runtime as :root[data-theme="custom"] from a saved palette (see theme.rs).   */
:root {
  /* Core surfaces. */
  --bg: #0f1115; --panel: #171a21; --panel-2: #1d212b; --border: #2a2f3a;
  --fg: #e6e8ec; --muted: #8b93a1; --accent: #6ea8fe; --accent-2: #3d6fd1;
  --user: #21304a; --err: #5a2230; --now: #ff5c6c;
  /* Text that sits on an --accent-2 fill (primary buttons, active tabs). */
  --on-accent: #ffffff;
  /* Modal/overlay backdrop scrim. */
  --scrim: rgba(0,0,0,.55);
  /* Semantic status trios (background · border · foreground). */
  --err-bg: #3a1c22; --err-border: #5a2a32; --err-fg: #ff9aa9;
  --ok-bg: #16361f; --ok-border: #1f5e33; --ok-fg: #8ef0b0;
  --warn-bg: #3a2c14; --warn-border: #5a4420; --warn-fg: #ffcf8b;
  /* Categorical chart ramp (charts.rs). Every chart colour resolves to one of
     these tokens, so a theme switch recolours all charts at once; light/AAA
     themes override the ramp below for legibility. */
  --chart-1: #6ea8fe; --chart-2: #44e0a6; --chart-3: #ff9e44; --chart-4: #c98bff;
  --chart-5: #ff6b7a; --chart-6: #ffd166; --chart-7: #4dd0e1; --chart-8: #a3d977;
  font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, sans-serif;
}

/* Aurora — deep teal night, emerald light. */
:root[data-theme="aurora"] {
  --bg: #08130f; --panel: #0d1d17; --panel-2: #12271f; --border: #1f3a2e;
  --fg: #dcefe2; --muted: #74a08c; --accent: #44e0a6; --accent-2: #1b6f54;
  --user: #123329; --err: #4a1f2a; --now: #ff6b7a;
  --on-accent: #ffffff;
  --scrim: rgba(2,12,8,.6);
  --err-bg: #2a1820; --err-border: #5a2f3a; --err-fg: #ff9fb0;
  --ok-bg: #0e3022; --ok-border: #1d6f4f; --ok-fg: #74f0c0;
  --warn-bg: #2e2613; --warn-border: #5a4a1f; --warn-fg: #f7d27a;
}

/* Ember — warm charcoal with amber. */
:root[data-theme="ember"] {
  --bg: #15100b; --panel: #1d1610; --panel-2: #261c13; --border: #39291b;
  --fg: #f1e7d9; --muted: #a8917a; --accent: #ff9e44; --accent-2: #a85a25;
  --user: #36240f; --err: #4c2418; --now: #ff5c5c;
  --on-accent: #ffffff;
  --scrim: rgba(12,7,3,.6);
  --err-bg: #361a16; --err-border: #5e2f24; --err-fg: #ffab93;
  --ok-bg: #2a3014; --ok-border: #566a26; --ok-fg: #c4e07a;
  --warn-bg: #38280f; --warn-border: #6a4a1e; --warn-fg: #ffcf8b;
}

/* Parchment — warm paper, terracotta ink (light). */
:root[data-theme="parchment"] {
  --bg: #f3ede0; --panel: #fbf6ec; --panel-2: #eae0cd; --border: #d9ccb2;
  --fg: #2a261d; --muted: #6b6150; --accent: #b14a32; --accent-2: #8c3a26;
  --user: #e7ddc6; --err: #efd2cb; --now: #c0392b;
  --on-accent: #fdf6ec;
  --scrim: rgba(40,30,15,.4);
  --err-bg: #f4dad4; --err-border: #d8a89c; --err-fg: #9a3322;
  --ok-bg: #e2efd4; --ok-border: #a9cb8c; --ok-fg: #3a6b22;
  --warn-bg: #f6e9c8; --warn-border: #ddc079; --warn-fg: #7c5a16;
  /* Deeper ramp for legibility on the cream surface. */
  --chart-1: #2f6fd0; --chart-2: #1a9e78; --chart-3: #d9791f; --chart-4: #8a4fce;
  --chart-5: #cf3a4c; --chart-6: #b8890f; --chart-7: #1f8aa0; --chart-8: #5a9a2e;
}

/* High contrast — pure black, white delineation, one bright accent (WCAG AAA). */
:root[data-theme="contrast"] {
  --bg: #000000; --panel: #000000; --panel-2: #000000; --border: #ffffff;
  --fg: #ffffff; --muted: #e6e6e6; --accent: #ffe000; --accent-2: #ffe000;
  --user: #1c1c00; --err: #1a0006; --now: #ff5252;
  --on-accent: #000000;
  --scrim: rgba(0,0,0,.9);
  --err-bg: #1a0006; --err-border: #ff4d6a; --err-fg: #ff8aa0;
  --ok-bg: #001a0a; --ok-border: #00e676; --ok-fg: #5cffa0;
  --warn-bg: #1a1400; --warn-border: #ffd000; --warn-fg: #ffe066;
  /* Bright, maximally-distinct ramp on pure black (AAA). */
  --chart-1: #59a5ff; --chart-2: #38ffa8; --chart-3: #ffab3d; --chart-4: #d59bff;
  --chart-5: #ff6b83; --chart-6: #ffe000; --chart-7: #4be0ff; --chart-8: #b6ff4d;
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--fg); }
/* svh (small viewport, with the vh fallback) = the height with the mobile
   browser's toolbars *shown*. All scrolling lives in nested overflow:auto panes,
   so the document body never scrolls and those toolbars never retract — meaning
   the visible area stays at svh. dvh (dynamic) can resolve to the taller
   toolbar-hidden viewport here, which overflows the root and hides the bottom of
   a scroller (e.g. the last agenda events) behind the toolbar. svh never exceeds
   the visible area, so nothing is clipped. */
.workbench { display: flex; flex-direction: column; height: 100vh; height: 100svh; }
.wb-header {
  display: flex; align-items: baseline; gap: .75rem;
  padding: .7rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.wb-title { font-weight: 700; font-size: 1.1rem; letter-spacing: .2px; }
.wb-subtitle { color: var(--muted); font-size: .85rem; }
.wb-header-spacer { flex: 1; }
.wb-workspace {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .35rem .5rem; font: inherit; font-size: .85rem; cursor: pointer;
}
.wb-workspace:hover:not(:disabled) { border-color: var(--accent); }
.wb-workspace:focus { outline: none; border-color: var(--accent); }
.wb-workspace:disabled { color: var(--muted); cursor: progress; }
.md-icon {
  display: inline-flex; width: 1em; height: 1em; flex: none;
  align-items: center; justify-content: center; line-height: 1;
}
.md-icon > svg { display: block; width: 100%; height: 100%; fill: currentColor; }

.wb-settings-btn {
  align-self: center; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; width: 2rem; height: 2rem;
  padding: 0; font-size: 1rem; line-height: 1; cursor: pointer; flex: none;
}
.wb-settings-btn:hover { border-color: var(--accent); color: var(--accent); }
.wb-settings-btn:focus { outline: none; border-color: var(--accent); }
/* Hamburger toggle for the nav drawer — only shown on narrow viewports (see the
   mobile media block at the end of this sheet). */
.wb-menu-btn {
  display: none; align-self: center; align-items: center; justify-content: center;
  background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; width: 2rem; height: 2rem;
  padding: 0; font-size: 1rem; line-height: 1; cursor: pointer; flex: none;
}
.wb-menu-btn:hover { border-color: var(--accent); color: var(--accent); }
.wb-menu-btn:focus { outline: none; border-color: var(--accent); }
/* The switcher wrapper participates in the header flex directly (contents), so
   its grouped <select> + org button sit alongside the gear as sibling items. */
.wb-switcher { display: contents; }
.wb-org-btn {
  align-self: center; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; height: 2rem;
  padding: 0 .6rem; font: inherit; font-size: .8rem; font-weight: 600;
  line-height: 1; cursor: pointer; flex: none;
}
.wb-org-btn:hover { border-color: var(--accent); color: var(--accent); }
.wb-org-btn:focus { outline: none; border-color: var(--accent); }
.wb-body { display: flex; flex: 1; min-height: 0; }

/* Unauthenticated sign-in surface (shell::LoginView). Centred card over the app
   background; the SSO button uses the primary --accent-2 fill, the hint --muted. */
.wb-login {
  display: flex; align-items: center; justify-content: center;
  min-height: 100vh; min-height: 100dvh; padding: 2rem 1rem; background: var(--bg);
}
.wb-login-card {
  display: flex; flex-direction: column; gap: 1.25rem; align-items: stretch;
  width: 100%; max-width: 22rem; padding: 2rem;
  background: var(--panel); border: 1px solid var(--border); border-radius: 12px;
  box-shadow: 0 12px 40px rgba(0,0,0,.35); text-align: center;
}
.wb-login-brand { display: flex; flex-direction: column; gap: .35rem; }
.wb-login-brand .wb-title { font-size: 1.4rem; }
.wb-login-sso {
  display: block; padding: .7rem 1rem; border-radius: 8px;
  background: var(--accent-2); color: var(--on-accent);
  border: 1px solid var(--accent-2); font: inherit; font-weight: 600;
  text-decoration: none; cursor: pointer;
}
.wb-login-sso:hover { background: var(--accent); border-color: var(--accent); }
.wb-login-sso:focus { outline: none; border-color: var(--accent); }
.wb-login-hint { margin: 0; color: var(--muted); font-size: .85rem; line-height: 1.4; }
.wb-login-form { display: grid; gap: .65rem; }
.wb-login-form h2 { margin: .35rem 0; font-size: 1rem; }
.wb-login-form label { color: var(--muted); font-size: .78rem; }
.wb-login-form input { background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: .45rem; padding: .7rem; }
.wb-login-error {
  margin: 0; padding: .6rem .75rem; border-radius: 8px; text-align: left;
  background: var(--err-bg); color: var(--err-fg); border: 1px solid var(--err-border);
  font-size: .85rem; line-height: 1.4;
}

/* Settings modal (tabbed: About · Email · LLM gateway · Status · API keys, SOUL §12). */
.settings-overlay {
  position: fixed; inset: 0; z-index: 50; background: var(--scrim);
  display: flex; align-items: flex-start; justify-content: center; padding: 4rem 1rem;
  overflow-y: auto;
}
.settings-modal {
  background: var(--panel); border: 1px solid var(--border); border-radius: 12px;
  width: 100%; max-width: 720px; max-height: 80vh; box-shadow: 0 12px 40px rgba(0,0,0,.5);
  display: flex; flex-direction: column; overflow: hidden;
}
.settings-layout { display: flex; min-height: 0; flex: 1; }
.settings-tabs {
  display: flex; flex-direction: column; gap: .15rem; padding: .8rem .6rem;
  border-right: 1px solid var(--border); background: var(--panel-2); min-width: 9rem;
}
.settings-tab {
  text-align: left; background: transparent; color: var(--fg); border: 0;
  border-radius: 8px; padding: .5rem .65rem; font: inherit; font-size: .88rem; cursor: pointer;
}
.settings-tab:hover { background: var(--panel); }
.settings-tab-active { background: var(--accent-2); color: var(--on-accent); }
.settings-content { flex: 1; min-width: 0; overflow-y: auto; padding: 1.1rem 1.2rem 1.3rem; }
.settings-header {
  display: flex; align-items: center; gap: .75rem;
  padding: .9rem 1.1rem; border-bottom: 1px solid var(--border);
}
.settings-header-titles { display: flex; flex-direction: column; gap: .1rem; flex: 1; }
.settings-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.settings-subtitle { color: var(--muted); font-size: .8rem; }
.settings-close {
  background: transparent; color: var(--muted); border: 0; border-radius: 6px;
  width: 1.8rem; height: 1.8rem; font-size: 1rem; cursor: pointer; flex: none;
}
.settings-close:hover { color: var(--fg); background: var(--panel-2); }
.settings-section { gap: .55rem; }
.settings-blurb { margin: 0 0 .3rem; color: var(--muted); font-size: .85rem; line-height: 1.45; }
.settings-hint { display: block; margin-top: .3rem; color: var(--muted); font-size: .78rem; }
.settings-blurb code, .settings-mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.settings-section { display: flex; flex-direction: column; gap: .55rem; }
.settings-section-title {
  margin: 0; font-size: .72rem; font-weight: 700; color: var(--muted);
  text-transform: uppercase; letter-spacing: .6px;
}
.settings-status { color: var(--muted); font-size: .85rem; }
.settings-error { color: var(--err-fg); }
.settings-empty { color: var(--muted); font-size: .85rem; font-style: italic; }
.settings-check { display: flex; align-items: center; gap: .4rem; font-size: .85rem; cursor: pointer; }
.settings-conn-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.settings-conn {
  display: flex; align-items: center; justify-content: space-between; gap: .6rem;
  padding: .5rem .65rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.settings-conn-name { font-size: .9rem; font-weight: 600; flex: 1; min-width: 0; }
.settings-conn-state {
  font-size: .65rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px;
  background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 0 .35rem;
}
.settings-conn-synced { color: var(--accent); border-color: var(--accent-2); }
.settings-conn-del {
  flex: none; background: transparent; color: var(--muted); border: 1px solid transparent;
  border-radius: 6px; padding: .2rem .45rem; font-size: .85rem; line-height: 1; cursor: pointer;
}
.settings-conn-del:hover { color: var(--err-fg); background: var(--err-bg); border-color: var(--err-border); }
.settings-form { display: flex; flex-direction: column; gap: .55rem; }
.settings-field { display: flex; flex-direction: column; gap: .25rem; }
.settings-label { font-size: .72rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; }
.settings-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .5rem .6rem; font: inherit; font-size: .9rem;
}
.settings-input:focus { outline: none; border-color: var(--accent); }
/* Removable model chips — the "force image input" list editor (SOUL §7/§9). */
.settings-chips { display: flex; flex-wrap: wrap; gap: .3rem; margin-bottom: .1rem; }
.settings-chip {
  display: inline-flex; align-items: center; gap: .3rem; padding: .12rem .2rem .12rem .5rem;
  border: 1px solid var(--border); border-radius: 999px; background: var(--panel-2);
  color: var(--fg); font-size: .78rem; line-height: 1.5;
}
.settings-chip-x {
  border: none; background: none; color: var(--muted); cursor: pointer; font-size: 1rem;
  line-height: 1; padding: 0 .2rem; border-radius: 50%;
}
.settings-chip-x:hover { color: var(--err-fg); }
.settings-form-error { color: var(--err-fg); font-size: .85rem; }
.settings-form-notice {
  font-size: .85rem; padding: .55rem .7rem;
  background: var(--user); border: 1px solid var(--accent-2); border-radius: 8px;
}
.settings-actions {
  display: flex; align-items: center; flex-wrap: wrap; gap: .4rem; margin-top: .2rem;
}
.settings-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .45rem .8rem; font: inherit; font-size: .85rem; font-weight: 600; cursor: pointer;
}
.settings-btn:disabled { color: var(--muted); cursor: not-allowed; }
.settings-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.settings-btn-primary:hover:not(:disabled) { background: var(--accent); }
.settings-btn-danger { color: var(--err-fg); border-color: var(--err-border); }
.settings-btn-danger:hover:not(:disabled) { background: var(--err-bg); color: var(--err-fg); }
.settings-section-head { display: flex; align-items: center; justify-content: space-between; gap: 1rem; }
.settings-form-row { flex-direction: row; align-items: flex-end; gap: .6rem; }
.settings-input-narrow { max-width: 8rem; }
/* Terminals */
.settings-conn-path { margin-left: .5rem; font-size: .72rem; color: var(--muted); word-break: break-all; }
.settings-conn-off { opacity: .6; }
.settings-conn-off-badge { color: var(--warn-fg); border-color: var(--warn-border); background: var(--warn-bg); }
.settings-conn-actions { display: flex; align-items: center; gap: .35rem; flex: none; }
.settings-btn-mini { padding: .22rem .5rem; font-size: .72rem; }

/* Users — a compact account ledger. Creation and credential maintenance are
   intentionally separate cards; directory rows keep identity and access
   scannable instead of collapsing them into an unlabelled text run. */
.settings-users { gap: .85rem; }
.settings-users-heading {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem;
  padding-bottom: .8rem; border-bottom: 1px solid var(--border);
}
.settings-users-title {
  margin: 0; font-size: 1.05rem; line-height: 1.2; letter-spacing: -.01em;
}
.settings-users-heading .settings-hint { margin: .28rem 0 0; max-width: 31rem; line-height: 1.45; }
.settings-users-count {
  display: inline-flex; align-items: baseline; gap: .3rem; flex: none;
  padding: .28rem .5rem; border: 1px solid var(--border); border-radius: 999px;
  background: var(--panel-2); color: var(--muted); font-size: .68rem;
  text-transform: uppercase; letter-spacing: .06em;
}
.settings-users-count strong { color: var(--fg); font-size: .8rem; }
.settings-users-message { margin: 0; }
.settings-form-error.settings-users-message {
  padding: .55rem .7rem; background: var(--err-bg); border: 1px solid var(--err-border); border-radius: 8px;
}
.settings-user-card {
  position: relative; overflow: hidden; padding: .85rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 10px;
}
.settings-user-card::before {
  content: ""; position: absolute; inset: 0 auto 0 0; width: 3px; background: var(--accent-2);
}
.settings-user-card-head {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; margin-bottom: .75rem;
}
.settings-user-card-head h4, .settings-user-directory-head h4 {
  margin: 0; font-size: .86rem; line-height: 1.25;
}
.settings-user-card-head p {
  margin: .18rem 0 0; color: var(--muted); font-size: .74rem; line-height: 1.4;
}
.settings-user-step {
  color: var(--muted); font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .68rem; letter-spacing: .08em;
}
.settings-user-create-form {
  display: grid; grid-template-columns: repeat(auto-fit, minmax(13rem, 1fr)); gap: .65rem .75rem;
}
.settings-user-create-form .settings-input,
.settings-user-reset-form .settings-input { width: 100%; min-width: 0; }
.settings-user-submit { grid-column: 1 / -1; justify-self: end; min-width: 8.5rem; }
.settings-user-reset-card::before { background: var(--muted); }
.settings-user-reset-form {
  display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: end; gap: .65rem .75rem;
}
.settings-user-reset-account { grid-column: 1 / -1; }
.settings-user-reset-submit { min-height: 2.2rem; }
.settings-user-directory-head {
  display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; margin-top: .1rem;
}
.settings-user-directory-head span {
  color: var(--muted); font-size: .62rem; text-transform: uppercase; letter-spacing: .08em;
}
.settings-user-list {
  list-style: none; display: flex; flex-direction: column; gap: .4rem; margin: 0; padding: 0;
}
.settings-user-row {
  display: grid; grid-template-columns: 2.1rem minmax(0, 1fr) auto; align-items: center; gap: .65rem;
  padding: .58rem .65rem; border: 1px solid var(--border); border-radius: 9px; background: var(--bg);
}
.settings-user-avatar {
  display: inline-flex; align-items: center; justify-content: center; width: 2.1rem; height: 2.1rem;
  border: 1px solid var(--accent-2); border-radius: 7px; background: var(--user); color: var(--fg);
  font-size: .68rem; font-weight: 750; letter-spacing: .04em;
}
.settings-user-meta { display: flex; flex-direction: column; min-width: 0; gap: .08rem; }
.settings-user-meta strong { overflow: hidden; text-overflow: ellipsis; font-size: .83rem; white-space: nowrap; }
.settings-user-meta span { overflow: hidden; color: var(--muted); font-size: .72rem; text-overflow: ellipsis; white-space: nowrap; }
.settings-user-role {
  padding: .16rem .42rem; border: 1px solid var(--border); border-radius: 999px;
  color: var(--muted); background: var(--panel-2); font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .62rem; font-weight: 650; text-transform: uppercase; letter-spacing: .06em;
}
.settings-user-role[data-role="owner"], .settings-user-role[data-role="admin"] {
  color: var(--accent); border-color: var(--accent-2); background: var(--user);
}
.settings-user-empty {
  display: flex; flex-direction: column; align-items: center; gap: .18rem; padding: 1.2rem;
  border: 1px dashed var(--border); border-radius: 9px; color: var(--muted); font-size: .76rem; text-align: center;
}
.settings-user-empty strong { color: var(--fg); font-size: .82rem; }

@media (max-width: 420px) {
  .settings-users-heading { flex-direction: column; gap: .55rem; }
  .settings-user-create-form, .settings-user-reset-form { grid-template-columns: 1fr; }
  .settings-user-submit { justify-self: stretch; }
  .settings-user-reset-submit { width: 100%; }
  .settings-user-row { grid-template-columns: 2.1rem minmax(0, 1fr); }
  .settings-user-role { grid-column: 2; justify-self: start; }
}

/* llmleaf topology editor — compact control-room styling. The visual hierarchy
   mirrors the actual flow: provider inventory first, then ordered route chains. */
.llmleaf-section { gap: .85rem; }
.llmleaf-heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
.llmleaf-heading .settings-hint { max-width: 36rem; line-height: 1.5; }
.llmleaf-live {
  display: inline-flex; align-items: center; gap: .35rem; flex: none;
  color: var(--muted); font-size: .65rem; text-transform: uppercase; letter-spacing: .08em;
}
.llmleaf-live i {
  width: .45rem; height: .45rem; border-radius: 50%; background: var(--accent);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 18%, transparent);
}
.llmleaf-kind-switch {
  display: grid; grid-template-columns: 1fr 1fr; padding: .25rem; gap: .25rem;
  border: 1px solid var(--border); border-radius: 10px; background: var(--panel-2);
}
.llmleaf-kind-switch button {
  display: flex; align-items: center; justify-content: center; gap: .45rem;
  min-height: 2.35rem; border: 1px solid transparent; border-radius: 7px;
  color: var(--muted); background: transparent; font: inherit; font-size: .82rem;
  font-weight: 650; cursor: pointer;
}
.llmleaf-kind-switch button span { font: 600 .62rem/1 ui-monospace, monospace; opacity: .62; }
.llmleaf-kind-switch button:hover { color: var(--fg); }
.llmleaf-kind-switch .llmleaf-kind-active {
  color: var(--fg); background: var(--bg); border-color: var(--border);
  box-shadow: 0 2px 8px color-mix(in srgb, var(--fg) 7%, transparent);
}
.llmleaf-form-card {
  gap: .75rem; padding: .85rem; border: 1px solid var(--border); border-radius: 12px;
  background: linear-gradient(145deg, var(--panel), var(--bg));
}
.llmleaf-card-heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
.llmleaf-card-heading > div { display: flex; flex-direction: column; gap: .16rem; }
.llmleaf-card-heading strong { font-size: .92rem; letter-spacing: -.01em; }
.llmleaf-card-heading > div > span { color: var(--muted); font-size: .75rem; line-height: 1.35; }
.llmleaf-resource-mark, .llmleaf-entry-icon {
  display: grid; place-items: center; width: 1.65rem; height: 1.65rem; flex: none;
  border: 1px solid var(--accent-2); border-radius: 5px; color: var(--accent);
  background: color-mix(in srgb, var(--accent) 8%, var(--bg));
  font: 700 .68rem/1 ui-monospace, monospace;
}
.llmleaf-form-grid { display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: .65rem; }
.llmleaf-field-help { color: var(--muted); font-size: .7rem; line-height: 1.35; }
.llmleaf-env-input { display: flex; align-items: stretch; }
.llmleaf-env-input > span {
  display: flex; align-items: center; padding: 0 .55rem; border: 1px solid var(--border);
  border-right: 0; border-radius: 8px 0 0 8px; color: var(--accent);
  background: var(--panel-2); font: 600 .75rem/1 ui-monospace, monospace;
}
.llmleaf-env-input .settings-input { min-width: 0; flex: 1; border-radius: 0 8px 8px 0; }
.llmleaf-advanced { border-top: 1px dashed var(--border); padding-top: .55rem; }
.llmleaf-advanced summary { color: var(--muted); font-size: .74rem; cursor: pointer; }
.llmleaf-advanced[open] summary { margin-bottom: .6rem; color: var(--fg); }
.llmleaf-target-head { display: flex; align-items: center; justify-content: space-between; }
.llmleaf-target-head > span { color: var(--muted); font-size: .68rem; }
.llmleaf-targets { display: flex; flex-direction: column; gap: .4rem; }
.llmleaf-target-row {
  display: grid; grid-template-columns: 1.7rem minmax(0, 1fr) minmax(0, 1fr) 1.7rem;
  align-items: end; gap: .45rem; padding: .5rem;
  border: 1px solid var(--border); border-radius: 9px; background: var(--bg);
}
.llmleaf-target-order {
  display: grid; place-items: center; align-self: center; width: 1.35rem; height: 1.35rem;
  border-radius: 50%; background: var(--accent-2); color: var(--on-accent);
  font: 700 .65rem/1 ui-monospace, monospace;
}
.llmleaf-target-remove {
  align-self: center; width: 1.6rem; height: 1.6rem; border: 0; border-radius: 5px;
  color: var(--muted); background: transparent; font-size: 1.15rem; cursor: pointer;
}
.llmleaf-target-remove:hover:not(:disabled) { color: var(--err-fg); background: var(--err-bg); }
.llmleaf-target-remove:disabled { opacity: .25; cursor: default; }
.llmleaf-add-target {
  align-self: flex-start; border: 0; color: var(--accent); background: transparent;
  font: inherit; font-size: .74rem; font-weight: 600; line-height: 1.2;
  cursor: pointer; padding: .15rem 0;
}
.llmleaf-add-target span { margin-right: .3rem; font-size: .9rem; }
.llmleaf-form-footer {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem;
  padding-top: .65rem; border-top: 1px solid var(--border);
}
.llmleaf-enabled { align-items: flex-start; }
.llmleaf-enabled > span { display: flex; flex-direction: column; gap: .08rem; color: var(--muted); font-size: .7rem; }
.llmleaf-enabled strong { color: var(--fg); font-size: .8rem; }
.llmleaf-save { min-width: 8rem; }
.llmleaf-message { margin-top: -.15rem; }
.llmleaf-list-heading { display: flex; align-items: center; justify-content: space-between; margin-top: .15rem; }
.llmleaf-list-heading > div { display: flex; align-items: center; gap: .45rem; }
.llmleaf-list-heading > div > span { font-size: .74rem; font-weight: 700; text-transform: uppercase; letter-spacing: .06em; }
.llmleaf-list-heading strong {
  min-width: 1.2rem; padding: .05rem .3rem; border-radius: 999px; text-align: center;
  color: var(--muted); background: var(--panel-2); font-size: .65rem;
}
.llmleaf-list-heading > span { color: var(--muted); font-size: .68rem; }
.llmleaf-entry-list { gap: .45rem; }
.llmleaf-entry {
  display: flex; align-items: center; gap: .65rem; padding: .65rem;
  border: 1px solid var(--border); border-radius: 10px; background: var(--panel-2);
}
.llmleaf-entry-off { opacity: .62; }
.llmleaf-entry-copy { display: flex; flex: 1; min-width: 0; flex-direction: column; gap: .18rem; }
.llmleaf-entry-copy > div { display: flex; align-items: center; gap: .4rem; }
.llmleaf-entry-copy strong { overflow: hidden; text-overflow: ellipsis; font-size: .86rem; }
.llmleaf-entry-copy > span { overflow: hidden; color: var(--muted); font-size: .72rem; text-overflow: ellipsis; white-space: nowrap; }
.llmleaf-entry-actions { display: flex; align-items: center; gap: .15rem; flex: none; }
.llmleaf-empty {
  padding: 1rem; border: 1px dashed var(--border); border-radius: 10px;
  color: var(--muted); text-align: center; font-size: .78rem;
}

/* Organisations manager (workspace.rs) — a two-pane modal: an organisation rail
   on the left, the selected org's detail on the right. Reuses the settings-*
   modal chrome + form primitives; everything below is the org-specific layout. */
.org-modal { max-width: 880px; height: min(80vh, 40rem); }
.org-layout { display: flex; min-height: 0; flex: 1; }
.org-rail {
  width: 15rem; flex: none; display: flex; flex-direction: column; gap: .25rem;
  padding: .8rem .6rem; border-right: 1px solid var(--border);
  background: var(--panel-2); overflow-y: auto;
}
.org-rail-label {
  font-size: .68rem; color: var(--muted); text-transform: uppercase;
  letter-spacing: .5px; padding: 0 .35rem .25rem;
}
.org-rail-item {
  display: flex; align-items: center; gap: .45rem; text-align: left;
  background: transparent; color: var(--fg); border: 1px solid transparent;
  border-radius: 8px; padding: .45rem .6rem; font: inherit; font-size: .88rem;
  cursor: pointer;
}
.org-rail-item:hover { background: var(--panel); }
.org-rail-item-active { background: var(--panel); border-color: var(--accent-2); }
.org-rail-name {
  flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis;
  white-space: nowrap; font-weight: 600;
}
.org-rail-count { color: var(--muted); font-size: .72rem; flex: none; }
.org-rail-new {
  margin-top: .5rem; text-align: left; background: transparent;
  color: var(--accent); border: 1px dashed var(--border); border-radius: 8px;
  padding: .45rem .6rem; font: inherit; font-size: .85rem; font-weight: 600;
  cursor: pointer;
}
.org-rail-new:hover { border-color: var(--accent); background: var(--panel); }
.org-rail-new-active { border-style: solid; border-color: var(--accent-2); background: var(--panel); }
.org-detail {
  flex: 1; min-width: 0; overflow-y: auto;
  padding: 1.1rem 1.2rem 1.3rem; display: flex; flex-direction: column; gap: 1.1rem;
}
.org-head { display: flex; align-items: center; gap: .6rem; flex-wrap: wrap; }
.org-head-title { margin: 0; font-size: 1.1rem; font-weight: 700; }
.org-chip {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .72rem;
  color: var(--muted); border: 1px solid var(--border); border-radius: 999px;
  padding: .1rem .55rem; background: var(--panel-2);
}
.org-badge {
  font-size: .66rem; font-weight: 700; text-transform: uppercase;
  letter-spacing: .5px; border-radius: 999px; padding: .12rem .5rem;
  border: 1px solid var(--border); color: var(--muted); background: var(--panel-2);
  flex: none;
}
.org-badge-owner { color: var(--warn-fg); border-color: var(--warn-border); background: var(--warn-bg); }
.org-badge-admin { color: var(--accent); border-color: var(--accent-2); }
.org-badge-current { color: var(--ok-fg); border-color: var(--ok-border); background: var(--ok-bg); }
.org-badge-archived { color: var(--warn-fg); border-color: var(--warn-border); background: var(--warn-bg); }
.org-count { color: var(--muted); font-weight: 400; }
.org-ws-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .4rem; }
.org-ws-row {
  display: flex; align-items: center; gap: .6rem; padding: .5rem .7rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 10px;
}
.org-ws-row-active { border-color: var(--accent-2); }
.org-ws-dot { width: .55rem; height: .55rem; border-radius: 50%; background: var(--accent); flex: none; }
.org-ws-dot-archived { background: var(--muted); }
.org-ws-meta { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: .05rem; }
.org-ws-name {
  font-size: .9rem; font-weight: 600; overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
.org-ws-slug {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .7rem; color: var(--muted);
}
.org-ws-actions { display: flex; align-items: center; gap: .4rem; flex: none; }
.org-member-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .4rem; }
.org-member-row {
  display: flex; align-items: center; gap: .6rem; padding: .45rem .7rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 10px;
}
.org-avatar {
  width: 1.8rem; height: 1.8rem; border-radius: 50%; background: var(--accent-2);
  color: var(--on-accent); display: flex; align-items: center; justify-content: center;
  font-size: .8rem; font-weight: 700; flex: none;
}
.org-member-meta { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: .05rem; }
.org-member-name {
  font-size: .88rem; font-weight: 600; overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
.org-member-mail { font-size: .72rem; color: var(--muted); }
.org-danger {
  display: flex; flex-direction: column; gap: .5rem;
  border: 1px solid var(--err-border); border-radius: 10px; padding: .8rem .9rem;
}
.org-danger-title {
  font-size: .7rem; font-weight: 700; color: var(--err-fg);
  text-transform: uppercase; letter-spacing: .5px;
}
.settings-term-sessions {
  margin-top: .5rem; padding-top: .6rem; border-top: 1px solid var(--border);
  display: flex; flex-direction: column; gap: .4rem;
}
/* About */
.about-hero {
  display: flex; flex-direction: column; gap: .15rem; padding: .4rem 0 1rem;
  border-bottom: 1px solid var(--border); margin-bottom: .6rem;
}
.about-mark { font-size: 1.6rem; font-weight: 800; letter-spacing: .3px; }
.about-tagline { color: var(--muted); font-size: .9rem; font-style: italic; }
.about-version {
  margin-top: .35rem; font-size: .7rem; color: var(--accent);
  text-transform: uppercase; letter-spacing: .6px;
}
.about-facts { list-style: none; margin: .4rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .4rem; }
.about-facts li { display: flex; flex-direction: column; gap: .1rem; }
.about-fact-k { font-size: .68rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; }
.about-fact-v { font-size: .88rem; }
.about-licenses { margin-top: .7rem; }
.about-licenses > summary {
  cursor: pointer; user-select: none; font-size: .76rem; color: var(--muted);
}
.about-licenses > summary:hover { color: var(--fg); }
.about-licenses > .settings-blurb { margin-top: .4rem; }
.license-list { list-style: none; margin: .4rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .3rem; }
.license-list li { display: flex; justify-content: space-between; gap: .8rem; font-size: .8rem; }
.license-k { color: var(--fg); }
.license-v {
  color: var(--muted); white-space: nowrap;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .74rem;
}
.license-note { margin-top: .5rem; }
/* Key/value (LLM gateway) */
.settings-kv { margin: 0; display: flex; flex-direction: column; gap: .1rem; }
.settings-kv-row {
  display: flex; gap: .8rem; align-items: baseline; padding: .45rem .1rem;
  border-bottom: 1px solid var(--border);
}
.settings-kv-row dt { min-width: 9.5rem; color: var(--muted); font-size: .78rem; }
.settings-kv-row dd { margin: 0; font-size: .88rem; word-break: break-all; }
.settings-mono { font-size: .85rem; }
/* Status services */
.settings-version { font-size: .72rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; }
.settings-svc-list { list-style: none; margin: .2rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.settings-svc {
  display: flex; align-items: center; gap: .6rem;
  padding: .5rem .65rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.settings-svc-name { font-size: .9rem; font-weight: 600; min-width: 9rem; }
.settings-svc-detail { flex: 1; min-width: 0; color: var(--muted); font-size: .8rem; word-break: break-all; }
.settings-svc-state {
  font-size: .65rem; font-weight: 700; text-transform: uppercase; letter-spacing: .5px;
  border-radius: 4px; padding: .1rem .4rem; flex: none;
}
.settings-svc-up { color: var(--ok-fg); background: var(--ok-bg); border: 1px solid var(--ok-border); }
.settings-svc-down { color: var(--err-fg); background: var(--err-bg); border: 1px solid var(--err-border); }
.settings-svc-disabled { color: var(--muted); background: var(--bg); border: 1px solid var(--border); }
/* Overall health rollup badge (reuses the up/down palette). */
.settings-health {
  align-self: flex-start; font-size: .74rem; font-weight: 600; padding: .2rem .6rem;
  border-radius: 999px; margin-bottom: .15rem;
}
.settings-health-ok { color: var(--ok-fg); background: var(--ok-bg); border: 1px solid var(--ok-border); }
.settings-health-bad { color: var(--err-fg); background: var(--err-bg); border: 1px solid var(--err-border); }
/* API keys */
.settings-token-reveal {
  display: flex; flex-direction: column; gap: .5rem; padding: .7rem .8rem;
  background: var(--user); border: 1px solid var(--accent-2); border-radius: 8px;
}
.settings-token-warn { font-size: .82rem; color: var(--fg); }
.settings-token-value {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .82rem;
  word-break: break-all; background: var(--bg); border: 1px solid var(--border);
  border-radius: 6px; padding: .5rem .6rem; user-select: all;
}
.settings-token-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.settings-token {
  display: flex; align-items: center; gap: .6rem;
  padding: .45rem .65rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.settings-token-id { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .82rem; }
.settings-token-grant:not(:empty) { font-size: .72rem; color: var(--accent); border: 1px solid var(--border); border-radius: .5rem; padding: .05rem .4rem; }
.settings-token-exp { flex: 1; color: var(--muted); font-size: .78rem; }
/* External MCP servers (mcp_servers section) — catalerum as an MCP client. */
.mcp-srv-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .4rem; }
.mcp-srv {
  display: flex; flex-direction: column; gap: .3rem;
  padding: .5rem .7rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.mcp-srv-head { display: flex; align-items: center; gap: .5rem; flex-wrap: wrap; }
.mcp-srv-name { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; font-weight: 600; }
.mcp-srv-badge {
  font-size: .68rem; text-transform: uppercase; letter-spacing: .03em; color: var(--muted);
  border: 1px solid var(--border); border-radius: .5rem; padding: .05rem .4rem;
}
.mcp-srv-status { font-size: .72rem; display: inline-flex; align-items: center; gap: .3rem; }
.mcp-srv-status::before { content: ""; width: .5rem; height: .5rem; border-radius: 50%; background: var(--muted); }
.mcp-srv-status.is-on { color: var(--ok-fg, var(--accent)); }
.mcp-srv-status.is-on::before { background: var(--ok-fg, var(--accent)); }
.mcp-srv-status.is-off { color: var(--err-fg); }
.mcp-srv-status.is-off::before { background: var(--err-fg); }
.mcp-srv-status.is-disabled { color: var(--muted); }
.mcp-srv-actions { margin-left: auto; display: flex; gap: .35rem; }
.mcp-srv-target { color: var(--muted); font-size: .78rem; word-break: break-all; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.mcp-srv-err { color: var(--err-fg); font-size: .74rem; }
/* MCP clients (mcp_connect.rs) — copy-paste config for external MCP products. */
.mcp-url-row { display: flex; align-items: center; gap: .5rem; }
.mcp-url {
  flex: 1; min-width: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .8rem; background: var(--bg); border: 1px solid var(--border);
  border-radius: 6px; padding: .45rem .55rem; word-break: break-all;
}
.mcp-token-input { flex: 1; min-width: 0; }
.mcp-clients { display: flex; flex-wrap: wrap; gap: .35rem; }
.mcp-client-chip {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 999px; padding: .3rem .7rem; font: inherit; font-size: .78rem;
  font-weight: 600; cursor: pointer;
}
.mcp-client-chip:hover { border-color: var(--accent); }
.mcp-client-chip-active { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.mcp-client-chip-active:hover { border-color: var(--accent-2); }
.mcp-snippet { display: flex; flex-direction: column; gap: .25rem; margin-top: .35rem; }
.mcp-snippet-head { display: flex; align-items: center; justify-content: space-between; gap: .6rem; }
.mcp-snippet-code {
  margin: 0; background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
  padding: .55rem .65rem; overflow-x: auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .76rem; line-height: 1.5; white-space: pre;
}
.mcp-share-note { font-size: .76rem; color: var(--muted); line-height: 1.4; }
.mcp-share-back { display: block; margin-top: .45rem; }
.mcp-footnote { margin-top: .4rem; }
.wb-nav {
  width: 180px; border-right: 1px solid var(--border); background: var(--panel);
  padding: .5rem;
}
/* Backdrop behind the mobile nav drawer; inert on desktop where the nav is a
   static column. Fades via opacity so the drawer's slide transition reads. */
.wb-nav-scrim {
  display: none; position: fixed; inset: 0; z-index: 59; background: var(--scrim);
  border: 0; padding: 0; opacity: 0; pointer-events: none; transition: opacity .18s ease;
}
.wb-nav ul { list-style: none; margin: 0; padding: 0; }
.nav-item {
  width: 100%; text-align: left; background: transparent; color: var(--fg);
  border: 0; border-radius: 7px; padding: .5rem .6rem; margin: .1rem 0;
  font-size: .92rem; cursor: pointer; display: flex; justify-content: space-between;
  align-items: center; text-decoration: none;
}
.nav-item-label { display: inline-flex; align-items: center; gap: .55rem; min-width: 0; }
.nav-item-label .md-icon { width: 1.05rem; height: 1.05rem; color: var(--muted); }
.nav-item-active .nav-item-label .md-icon { color: currentColor; }
.nav-item:hover:not(.nav-item-disabled) { background: var(--panel-2); }
.nav-item-active { background: var(--accent-2); color: var(--on-accent); }
.nav-item-disabled, .nav-item[aria-disabled="true"] { color: var(--muted); cursor: not-allowed; }
.nav-soon {
  font-size: .65rem; color: var(--muted); background: var(--panel-2);
  border-radius: 4px; padding: 0 .3rem;
}
.nav-section-toggle {
  width: 100%; text-align: left; background: transparent; color: var(--muted);
  border: 0; border-top: 1px solid var(--border); border-radius: 0;
  margin: .4rem 0 .1rem; padding: .5rem .6rem; font: inherit;
  font-size: .72rem; text-transform: uppercase; letter-spacing: .5px;
  cursor: pointer; display: flex; justify-content: space-between; align-items: center;
}
.nav-section-toggle:hover { color: var(--fg); }
.nav-chevron { transition: transform .15s ease; display: inline-flex; font-size: .9rem; }
.nav-chevron-open { transform: rotate(90deg); }
.nav-collapsed { list-style: none; margin: 0; padding: 0; }
/* Apps nav entry: the pinned-apps quick menu. The flyout opens on row hover
   (desktop) or via the ▸ toggle (touch/keyboard); it only renders when any
   apps are pinned. */
.nav-apps { position: relative; }
.nav-apps-row { display: flex; align-items: center; }
.nav-apps-row .nav-item { flex: 1 1 auto; min-width: 0; }
.nav-apps-toggle {
  flex: 0 0 auto; background: transparent; border: 0; color: var(--muted);
  cursor: pointer; padding: .35rem .45rem; border-radius: 6px; line-height: 1;
}
.nav-apps-toggle:hover { background: var(--panel-2); color: var(--fg); }
.nav-apps-open .nav-apps-toggle .nav-chevron { transform: rotate(90deg); }
.nav-apps-flyout {
  display: none; position: absolute; left: 100%; top: 0; z-index: 70;
  min-width: 160px; max-width: 240px; margin: 0; padding: .3rem;
  list-style: none; background: var(--panel); border: 1px solid var(--border);
  border-radius: 8px; box-shadow: 0 8px 28px rgba(0,0,0,.35);
}
.nav-apps:hover .nav-apps-flyout, .nav-apps-open .nav-apps-flyout { display: block; }
.nav-apps-pin {
  width: 100%; text-align: left; background: transparent; border: 0;
  color: var(--fg); font-size: .85rem; padding: .4rem .55rem; border-radius: 6px;
  cursor: pointer; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.nav-apps-pin:hover { background: var(--panel-2); }
.wb-main { flex: 1; min-width: 0; display: flex; }
.panel-placeholder { margin: auto; text-align: center; color: var(--muted); }

/* --- Chat: sessions sidebar + streaming pane (SOUL §12) --- The sessions
   sidebar is a shared `.pane-list`; `.chat-sidebar` keeps only the mobile
   drawer mechanics (its toggle lives in the chat toolbar, not the shared ☰). */
/* `min-width: 0` breaks the min-content chain: without it a wide unbreakable
   child (a code block's longest line, a table, a mermaid SVG) becomes the
   layout's minimum width and widens the whole page on a phone — the content
   must instead scroll inside its own block (see `.msg-markdown pre` etc.). */
.chat-layout { display: flex; flex: 1; min-height: 0; min-width: 0; }
/* Backdrop behind the mobile sessions drawer; inert on desktop. */
.chat-sidebar-scrim {
  display: none; position: fixed; inset: 0; z-index: 59; background: var(--scrim);
  border: 0; padding: 0; opacity: 0; pointer-events: none; transition: opacity .18s ease;
}
.chat-sidebar-body { padding: .3rem; }
.chat-search {
  width: 100%; margin: .15rem 0 .35rem; padding: .35rem .5rem; font: inherit; font-size: .82rem;
  background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px;
}
.chat-search:focus { outline: none; border-color: var(--accent); }
.chat-session-group { display: flex; flex-direction: column; }
.chat-group-label {
  font-size: .66rem; color: var(--muted); text-transform: uppercase; letter-spacing: .6px;
  font-weight: 700; padding: .6rem .6rem .25rem;
  position: sticky; top: 0; background: var(--panel); z-index: 1;
}
.chat-session-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .1rem; }
.chat-session {
  display: block; width: 100%; text-align: left; background: transparent; color: var(--fg);
  border: 1px solid transparent; border-radius: 8px; padding: .45rem .6rem;
  font-size: .88rem; cursor: pointer; text-decoration: none;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.chat-session:hover { background: var(--panel-2); }
/* Topic-tag pills under a sidebar session title (auto-tag): generated by the
   backend's background metadata pass; three max, single-line, muted. */
.chat-session-tags { display: flex; gap: .25rem; flex-wrap: wrap; padding: .1rem .6rem .25rem; }
.chat-session-tag {
  font-size: .62rem; color: var(--muted); text-transform: lowercase;
  border: 1px solid var(--border); border-radius: 999px; padding: .05rem .4rem;
  line-height: 1.35; white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  max-width: 9rem;
}
.chat-session-active { background: var(--panel-2); border-color: var(--accent-2); }
/* Shared list-row action buttons (SOUL §12): the edit ✎ / delete ✕ icon
   controls on the chat, calendar and notes rows, via `widgets::row_action`.
   The look lives here once; each surface keeps only its own positioning and
   (optional) hover-reveal wrapper. */
.row-acts { display: flex; align-items: center; gap: .1rem; flex-shrink: 0; }
.row-acts-reveal { opacity: 0; transition: opacity .12s ease; }
.chat-session-row:hover .row-acts-reveal,
.cal-cal-item:hover .row-acts-reveal,
.apps-row:hover .row-acts-reveal,
.row-acts-reveal:focus-within { opacity: 1; }
.row-act {
  flex: none; background: transparent; color: var(--muted);
  border: 1px solid transparent; border-radius: 6px;
  padding: .3rem .4rem; font-size: .8rem; line-height: 1; cursor: pointer;
}
.row-act:hover { color: var(--fg); background: var(--panel-2); border-color: var(--border); }
.row-act-danger:hover { color: var(--err-fg); background: var(--err-bg); border-color: var(--err-border); }

/* Session row: the title button fills the full row width; the ✎/✕ actions
   overlay its right edge on hover — the panel-2 bg masks the title behind. */
.chat-session-row { position: relative; display: flex; align-items: center; flex-wrap: wrap; }
.chat-session-row > .chat-session { flex: 1 1 auto; }
.chat-session-tags { flex-basis: 100%; }
.chat-session-row .chat-session { flex: 1 1 auto; min-width: 0; width: 100%; }
.chat-session-acts {
  position: absolute; right: .2rem; top: 50%; transform: translateY(-50%);
  background: var(--panel-2); border-radius: 6px;
}
/* Inline rename form, shown above the session list while renaming. */
.chat-rename-form { display: flex; gap: .3rem; padding: .2rem .4rem .5rem; }
.chat-rename-input {
  flex: 1; min-width: 0; background: var(--bg); color: var(--fg); border: 1px solid var(--accent);
  border-radius: 6px; padding: .3rem .45rem; font: inherit; font-size: .85rem;
}
.chat-rename-input:focus { outline: none; }
.chat-rename-btn {
  flex: none; background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 6px; padding: .3rem .5rem; font-size: .8rem; cursor: pointer;
}

.chat-panel { display: flex; flex-direction: column; flex: 1; min-width: 0; min-height: 0; }
/* Chat panel toolbar: the profile picker stays at the top-left when profiles
   exist; the panel toggle stays right-aligned. On narrow viewports the chat
   drawer toggle joins the left-hand controls. */
.chat-toolbar { display: flex; align-items: center; justify-content: flex-end; gap: .5rem; padding: .35rem .8rem; border-bottom: 1px solid var(--border); }
.chat-toolbar-profile {
  min-width: 0; margin-right: auto; display: inline-flex; align-items: center; gap: .4rem;
  color: var(--muted); font-size: .72rem; font-weight: 700; text-transform: uppercase;
  letter-spacing: .45px;
}
.chat-toolbar-profile-select {
  min-width: 8.5rem; max-width: min(16rem, 38vw); padding: .3rem 1.8rem .3rem .55rem;
  border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--fg);
  font: inherit; font-size: .8rem; font-weight: 600; text-transform: none; letter-spacing: normal;
  cursor: pointer;
}
.chat-toolbar-profile-select:hover:not(:disabled) { border-color: var(--accent); }
.chat-toolbar-profile-select:focus { outline: none; border-color: var(--accent); }
.chat-toolbar-profile-select:disabled { color: var(--muted); cursor: wait; }
/* Opens the sessions sidebar as a drawer — only shown on narrow viewports. */
.chat-sessions-toggle { display: none; }
.chat-panel-toggle { padding: .3rem .7rem; border: 1px solid var(--border); border-radius: 8px; background: var(--bg); color: var(--fg); font: inherit; font-size: .8rem; font-weight: 600; cursor: pointer; }
.chat-panel-toggle:hover { background: var(--panel-2); }
.chat-panel-toggle-on { background: var(--panel-2); border-color: var(--accent-2); }

/* --- Shared master-detail second sidebar (SOUL §12) --- Every catalogue
   panel (Chat sessions, Notes, Skills, Profiles, Automations, Grants, History,
   Memory, Endpoints, Apps, Calendars, Email) is the same shape: a `.pane-split`
   row holding a `.pane-list` aside (header + scrollable body + rows) beside a
   `.pane-detail` editor. The look lives here once; a panel keeps its own class
   only for real deviations (width, extra children). */
.pane-split { display: flex; flex: 1; min-height: 0; }
.pane-list {
  width: 280px; flex-shrink: 0; display: flex; flex-direction: column;
  border-right: 1px solid var(--border); background: var(--panel); min-height: 0;
}
.pane-list-header {
  display: flex; align-items: center; justify-content: space-between; gap: .5rem;
  padding: .7rem .8rem; border-bottom: 1px solid var(--border);
}
.pane-list-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.pane-list-body { flex: 1; min-height: 0; overflow-y: auto; }
.pane-list-status { color: var(--muted); padding: 1rem .8rem; font-size: .88rem; margin: 0; }
.pane-list-error { color: var(--err-fg); }
.pane-search {
  width: calc(100% - 1rem); margin: .4rem .5rem .1rem; padding: .35rem .5rem;
  font: inherit; font-size: .82rem; background: var(--bg); color: var(--fg);
  border: 1px solid var(--border); border-radius: 6px;
}
.pane-search:focus { outline: none; border-color: var(--accent); }
.pane-items { list-style: none; margin: 0; padding: .4rem; display: flex; flex-direction: column; gap: .2rem; }
.pane-item {
  width: 100%; text-align: left; display: flex; flex-direction: column; gap: .15rem;
  background: transparent; color: var(--fg); border: 1px solid transparent;
  border-radius: 8px; padding: .5rem .6rem; cursor: pointer;
}
.pane-item:hover { background: var(--panel-2); }
.pane-item-active { background: var(--panel-2); border-color: var(--accent-2); }
.pane-item-title {
  font-size: .92rem; font-weight: 600; word-break: break-word;
  display: flex; align-items: center; gap: .4rem;
}
.pane-item-preview {
  font-size: .78rem; color: var(--muted); white-space: nowrap; overflow: hidden;
  text-overflow: ellipsis;
}
.pane-item-meta { font-size: .76rem; color: var(--muted); }
.pane-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .8rem; font: inherit; font-size: .85rem;
  font-weight: 600; cursor: pointer;
}
.pane-btn:hover:not(:disabled) { border-color: var(--accent); }
.pane-btn:disabled { color: var(--muted); cursor: not-allowed; }
.pane-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.pane-btn-primary:hover:not(:disabled) { background: var(--accent); }
.pane-btn-danger { background: var(--err); color: var(--err-fg); border-color: var(--err-border); }
.pane-btn-danger:hover:not(:disabled) { border-color: var(--err-fg); }
.pane-detail { flex: 1; min-width: 0; display: flex; }

/* The list pane collapses to an off-canvas drawer on narrow viewports, toggled
   by the ☰ `.list-drawer-toggle` in the detail pane — the same affordance as
   the chat sessions sidebar (`widgets::list_drawer_scrim`/`list_drawer_toggle`).
   All three pieces are inert on desktop; the mobile media query below turns the
   `.list-drawer`-classed aside off-canvas and reveals the toggle + scrim. */
.list-drawer-scrim {
  display: none; position: fixed; inset: 0; z-index: 59; background: var(--scrim);
  border: 0; padding: 0; opacity: 0; pointer-events: none; transition: opacity .18s ease;
}
.list-drawer-toggle {
  display: none; align-items: center; gap: .3rem; align-self: flex-start;
  margin: .5rem .8rem 0; padding: .3rem .7rem; border: 1px solid var(--border);
  border-radius: 8px; background: var(--bg); color: var(--fg); font: inherit;
  font-size: .8rem; font-weight: 600; cursor: pointer;
}
.list-drawer-toggle:hover { background: var(--panel-2); }

/* Right workbench sidebar: tabbed Output (terminal) + Settings (pickers), SOUL §12/§20. */
.chat-side {
  width: 320px; flex-shrink: 0; display: flex; flex-direction: column;
  border-left: 1px solid var(--border); background: var(--panel); min-height: 0;
}
/* Backdrop behind the mobile right-panel overlay; inert on desktop (split pane). */
.chat-side-scrim {
  display: none; position: fixed; inset: 0; z-index: 59; background: var(--scrim); border: 0; padding: 0;
}
.chat-side-tabs { display: flex; align-items: center; gap: .15rem; padding: .35rem .4rem; border-bottom: 1px solid var(--border); }
.chat-side-tab {
  flex: none; background: transparent; color: var(--muted); border: 1px solid transparent;
  border-radius: 7px; padding: .3rem .65rem; font: inherit; font-size: .82rem; font-weight: 600; cursor: pointer;
}
.chat-side-tab:hover { background: var(--panel-2); color: var(--fg); }
.chat-side-tab-active { background: var(--panel-2); color: var(--fg); border-color: var(--accent-2); }
.chat-side-close {
  margin-left: auto; flex: none; background: transparent; color: var(--muted);
  border: 1px solid transparent; border-radius: 6px; padding: .25rem .45rem; font-size: .8rem; line-height: 1; cursor: pointer;
}
.chat-side-close:hover { color: var(--fg); background: var(--panel-2); }
.chat-side-body { flex: 1; min-height: 0; display: flex; flex-direction: column; overflow: hidden; }
.chat-side-hint { color: var(--muted); font-size: .85rem; padding: .9rem .8rem; margin: 0; }
.chat-settings { display: flex; flex-direction: column; gap: .8rem; padding: .8rem; overflow-y: auto; }
.chat-set-field { display: flex; flex-direction: column; gap: .25rem; }
.chat-set-label { font-size: .7rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; font-weight: 700; }
.chat-set-select {
  width: 100%; padding: .4rem .5rem; border: 1px solid var(--border); border-radius: 7px;
  background: var(--bg); color: var(--fg); font: inherit; font-size: .85rem;
}
.chat-set-select:focus { outline: none; border-color: var(--accent); }
.chat-set-error { color: var(--danger, #c0392b); font-size: .78rem; }
/* Current model's capability chips under the chat Model picker (SOUL §7/§9): a
   muted pill per capability; the ones the model supports light up in accent. */
.chat-set-caps { display: flex; flex-wrap: wrap; gap: .3rem; }
.chat-cap {
  display: inline-flex; align-items: center; gap: .25rem; padding: .12rem .45rem;
  border: 1px solid var(--border); border-radius: 999px; background: var(--panel-2);
  color: var(--muted); font-size: .72rem; line-height: 1.5; white-space: nowrap;
}
.chat-cap-on { border-color: var(--accent); color: var(--accent); background: var(--bg); }
.chat-cap-force {
  align-self: flex-start; margin-top: .1rem; padding: .2rem .5rem; border-radius: 7px;
  border: 1px solid var(--border); background: var(--bg); color: var(--muted);
  font-size: .72rem; cursor: pointer;
}
.chat-cap-force:hover { color: var(--fg); border-color: var(--accent); }
/* The Settings tab's Debug section (SOUL §12): the "Copy chat as JSON" export. */
.chat-debug-btn {
  align-self: flex-start; padding: .35rem .6rem; border-radius: 7px;
  border: 1px solid var(--border); background: var(--bg); color: var(--fg);
  font: inherit; font-size: .82rem; cursor: pointer;
}
.chat-debug-btn:hover:enabled { border-color: var(--accent); }
.chat-debug-btn:disabled { opacity: .6; cursor: default; }
.chat-debug-btn-done { border-color: var(--accent); color: var(--accent); }
/* Inside a settings field the panel-level hint padding would double up. */
.chat-debug-hint { padding: 0; }
/* In the sidebar's Output tab the terminal pane fills the height (no inline cap). */
.term-pane { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.term-pane-bar { display: flex; align-items: center; gap: .5rem; padding: .35rem 1rem; background: var(--bg-alt, var(--bg)); }
.term-pane-label { font-size: .8rem; color: var(--muted, #888); }
.term-pane-select { padding: .2rem .5rem; border: 1px solid var(--border); border-radius: 6px; background: var(--bg); color: var(--fg); font-size: .8rem; }
.term-pane-refresh { border: 1px solid var(--border); border-radius: 6px; background: var(--bg); color: var(--fg); cursor: pointer; padding: .15rem .45rem; }
.term-out { flex: 1; overflow: auto; margin: 0; padding: .6rem 1rem; background: #11151a; color: #d6deeb; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .8rem; line-height: 1.3; white-space: pre-wrap; word-break: break-word; min-height: 6rem; }
/* `overflow-x: hidden` is the backstop against horizontal panning: every wide
   block child (pre/table/mermaid/math) scrolls inside its own box, so the log
   itself must never scroll sideways — panning here is what read as "the chat
   gets wider" on phones. */
.chat-log { flex: 1; overflow-y: auto; overflow-x: hidden; padding: 1rem; display: flex; flex-direction: column; gap: .6rem; }
.chat-empty { margin: auto; color: var(--muted); text-align: center; }
.chat-empty-disclaimer { margin-top: .5rem; font-size: .8rem; opacity: .8; }
.msg { display: flex; flex-direction: column; gap: .15rem; max-width: 70ch; }
.msg-user { align-self: flex-end; }
.msg-role { font-size: .7rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; }
.msg-text {
  white-space: pre-wrap; word-break: break-word; padding: .55rem .7rem;
  border-radius: 10px; background: var(--panel-2); border: 1px solid var(--border);
}
.msg-user .msg-text { background: var(--user); }
.msg-error .msg-text { background: var(--err); border-color: var(--err-border); }
.msg-cursor { animation: blink 1s step-start infinite; color: var(--accent); }
@keyframes blink { 50% { opacity: 0; } }
.msg-thinking { font-size: .8rem; color: var(--muted); margin-bottom: .3rem; }
.msg-thinking > summary { cursor: pointer; user-select: none; }
.msg-thinking-text { display: block; white-space: pre-wrap; word-break: break-word; margin-top: .3rem; padding-left: .6rem; border-left: 2px solid var(--border); }
.msg-cost {
  align-self: flex-start; margin-top: .25rem; font-size: .68rem; color: var(--muted);
  font-variant-numeric: tabular-nums; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
/* Per-message action row: subtle controls under a bubble — copy the message
   text, and (under a user bubble) regenerate the conversation from it. Right-
   aligned to sit under the right-aligned user bubble; revealed on row hover to
   stay unobtrusive, like the sidebar act buttons. */
.msg-acts {
  align-self: flex-end; display: flex; align-items: center; gap: .3rem;
  margin-top: .2rem; opacity: 0; transition: opacity .12s ease;
}
.msg:hover .msg-acts, .msg-acts:focus-within { opacity: .8; }
.msg-act {
  padding: .12rem .45rem; font-size: .68rem; color: var(--muted);
  background: transparent; border: 1px solid var(--border); border-radius: 6px;
  cursor: pointer; transition: color .12s, border-color .12s, opacity .12s;
}
.msg-act:hover { color: var(--fg); border-color: var(--accent-2); opacity: 1; }
.msg-regen:disabled, .msg:hover .msg-regen:disabled { cursor: default; opacity: .35; }

/* Shared copy-to-clipboard button (`widgets::copy_button`): the `.copy-btn-done`
   flash tints it success-green for ~1.2s after a copy. The resting look comes
   from the caller's extra class (`.msg-act`, `.pane-btn`, …). */
.copy-btn { cursor: pointer; }
/* Two-class selector so the flash outranks the callers' own `.msg-act` /
   `.pane-btn` colour rules regardless of stylesheet order. */
.copy-btn.copy-btn-done, .copy-btn.copy-btn-done:hover {
  color: var(--ok-fg); border-color: var(--ok-border); opacity: 1;
}
/* Per-turn token info-icon: a muted ⓘ whose native title tooltip spells out the
   turn's token + cache usage and the running conversation total. */
.msg-tokens {
  align-self: flex-start; margin-top: .25rem; font-size: .72rem; line-height: 1;
  color: var(--muted); cursor: help; user-select: none;
}
.msg-tokens:hover { color: var(--accent); }
/* Tool-call cards: one collapsible <details> per tool the assistant ran this
   turn, modelled on the .msg-thinking block. Live cards show a spinner that
   flips to ✓/✗ in place when the result arrives. */
.msg-tools { display: flex; flex-direction: column; gap: .3rem; margin: .1rem 0 .3rem; }
.msg-tool {
  font-size: .8rem; background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 8px; overflow: hidden;
}
.msg-tool-summary {
  display: flex; align-items: center; gap: .4rem; cursor: pointer; user-select: none;
  padding: .35rem .55rem; color: var(--fg);
}
.msg-tool-summary::-webkit-details-marker { display: none; }
.msg-tool-glyph { font-size: .8rem; color: var(--accent); width: 1rem; text-align: center; }
.msg-tool-failed .msg-tool-glyph { color: var(--err-fg); }
.msg-tool-name {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .74rem;
  color: var(--fg); background: var(--panel); border: 1px solid var(--border);
  border-radius: 5px; padding: .05rem .35rem;
}
.msg-tool-src {
  flex: none; font-size: .58rem; font-weight: 700; text-transform: uppercase; letter-spacing: .3px;
  color: var(--accent); background: var(--panel); border: 1px solid var(--accent-2);
  border-radius: 4px; padding: .02rem .3rem;
}
.msg-tool-detail {
  color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0;
}
.msg-tool-dur {
  margin-left: auto; flex: none; font-size: .68rem; color: var(--muted);
  font-variant-numeric: tabular-nums; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.msg-tool-body {
  padding: .1rem .55rem .5rem; border-top: 1px solid var(--border);
  display: flex; flex-direction: column; gap: .35rem;
}
.msg-tool-sub {
  font-size: .66rem; text-transform: uppercase; letter-spacing: .3px; color: var(--muted); margin-top: .2rem;
}
.msg-tool-kv { margin: 0; display: flex; flex-direction: column; gap: .15rem; }
.msg-tool-row { display: flex; gap: .5rem; }
.msg-tool-key { color: var(--muted); min-width: 4rem; }
.msg-tool-val { color: var(--fg); word-break: break-word; }
.msg-tool-list { margin: 0; padding-left: 1.1rem; display: flex; flex-direction: column; gap: .2rem; }
/* Labelled result groups (batch web search, ask_user Q&A rows). */
.msg-tool-groups { display: flex; flex-direction: column; gap: .4rem; }
/* ask_user Q&A card rows: the question, then the answer the user gave (or the
   offered options while unanswered). */
.msg-tool-qa { display: flex; flex-direction: column; gap: .1rem; }
.msg-tool-question { color: var(--fg); font-weight: 600; }
.msg-tool-answer { color: var(--fg); padding-left: .9rem; word-break: break-word; }
.msg-tool-answer::before { content: "↳ "; color: var(--muted); }
.msg-tool-hit { word-break: break-word; }
.msg-tool-link { color: var(--accent); text-decoration: underline; }
.msg-tool-snip { color: var(--muted); font-size: .74rem; }
.msg-tool-md { word-break: break-word; }
.msg-tool-empty { color: var(--muted); }
.msg-tool-args, .msg-tool-out, .msg-tool-cmd, .msg-tool-err {
  margin: 0; white-space: pre-wrap; word-break: break-word; max-height: 14rem; overflow: auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: .74rem;
  background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: .4rem .5rem;
}
.msg-tool-cmd { color: #d6deeb; }
.msg-tool-err { color: var(--err-fg); background: var(--err); border-color: var(--err-border); }
.msg-tool-trunc {
  flex: none; font-size: .62rem; text-transform: uppercase; letter-spacing: .3px;
  color: var(--warn-fg); background: var(--warn-bg); border: 1px solid var(--warn-border); border-radius: 4px; padding: .02rem .3rem;
}
.msg-tool-spinner {
  display: inline-block; width: .7rem; height: .7rem; border-radius: 50%;
  border: 2px solid var(--border); border-top-color: var(--accent);
  animation: msg-tool-spin .7s linear infinite;
}
@keyframes msg-tool-spin { to { transform: rotate(360deg); } }
/* Rendered Markdown inside a finalized assistant bubble. The container keeps the
   `.msg-text` bubble; these tame the block children so they sit flush in it. */
.msg-markdown > :first-child { margin-top: 0; }
.msg-markdown > :last-child { margin-bottom: 0; }
.msg-markdown p { margin: 0 0 .5rem; }
.msg-markdown h1, .msg-markdown h2, .msg-markdown h3,
.msg-markdown h4, .msg-markdown h5, .msg-markdown h6 { margin: .4rem 0 .3rem; line-height: 1.25; }
.msg-markdown h1 { font-size: 1.2rem; }
.msg-markdown h2 { font-size: 1.08rem; }
.msg-markdown h3 { font-size: 1rem; }
.msg-markdown h4 { font-size: .92rem; }
.msg-markdown h5 { font-size: .86rem; }
.msg-markdown h6 { font-size: .86rem; color: var(--muted); }
.msg-markdown ul, .msg-markdown ol { margin: 0 0 .5rem; padding-left: 1.25rem; }
.msg-markdown li { margin: .12rem 0; }
.msg-markdown li > ul, .msg-markdown li > ol { margin: .12rem 0 0; }
.msg-markdown hr { border: 0; border-top: 1px solid var(--border); margin: .6rem 0; }
.msg-markdown a { color: var(--accent); text-decoration: underline; }
.msg-markdown code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .86em;
  background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 0 .25rem;
}
.msg-markdown pre {
  margin: 0 0 .5rem; background: var(--bg); border: 1px solid var(--border);
  border-radius: 6px; padding: .5rem .6rem; overflow-x: auto;
}
.msg-markdown pre code { background: none; border: 0; padding: 0; font-size: .82rem; }
.msg-markdown blockquote {
  margin: 0 0 .5rem; padding-left: .6rem; border-left: 3px solid var(--border); color: var(--muted);
}
/* Markdown tables/images stay inside the bubble: a table scrolls horizontally
   rather than widening the chat (critical on phones); an image never overflows. */
.msg-markdown table {
  display: block; width: max-content; max-width: 100%; overflow-x: auto;
  border-collapse: collapse; margin: 0 0 .5rem; font-size: .86rem;
}
.msg-markdown th, .msg-markdown td { border: 1px solid var(--border); padding: .3rem .5rem; text-align: left; }
.msg-markdown th { background: var(--panel-2); }
.msg-markdown img { max-width: 100%; height: auto; }
/* Mermaid diagrams (`<figure>` around the engine's inline SVG; the raw-source
   `<pre class="mermaid">` fallback is covered by the pre rule above) and block
   math scroll horizontally like tables — the engine emits these classes as
   styling hooks and relies on the host sheet for the overflow behaviour. */
.msg-markdown figure.catalerum-mermaid { margin: 0 0 .5rem; overflow-x: auto; }
.msg-markdown figure.catalerum-mermaid svg { display: block; }
.msg-markdown .catalerum-math-block { overflow-x: auto; text-align: center; margin: 0 0 .5rem; }
.chat-input {
  position: relative; /* anchors the slash-command menu */
  display: flex; flex-direction: column; gap: .5rem; padding: .6rem 1rem;
  border-top: 1px solid var(--border); background: var(--panel);
}
.chat-input-row { display: flex; gap: .5rem; align-items: stretch; }
/* Slash-command menu (SOUL §12/§23): floats above the whole composer block while
   the draft spells a command, so attachment chips never shift it. */
.chat-slash-menu {
  position: absolute; left: 1rem; right: 1rem; bottom: calc(100% + .25rem); z-index: 30;
  display: flex; flex-direction: column; padding: .25rem; max-height: 14rem; overflow-y: auto;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
  box-shadow: 0 10px 28px rgba(0,0,0,.5);
}
.chat-slash-item {
  display: flex; gap: .6rem; align-items: baseline; min-width: 0;
  border: 0; border-radius: 6px; background: transparent; text-align: left;
  padding: .4rem .55rem; font: inherit; font-size: .85rem; color: var(--fg); cursor: pointer;
}
.chat-slash-item-active { background: var(--user); }
.chat-slash-name {
  flex: none; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  color: var(--accent);
}
.chat-slash-desc {
  color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
/* Tool-guard approval prompt (SOUL §19) — shown above the composer while a
   guarded tool call is paused awaiting the user's decision. */
.chat-approval {
  margin: 0 1rem .5rem; padding: .6rem .8rem; border-radius: 10px;
  border: 1px solid var(--accent); background: var(--panel-2);
  display: flex; flex-direction: column; gap: .4rem;
}
.chat-approval-head { display: flex; align-items: baseline; gap: .5rem; }
.chat-approval-title { font-weight: 600; color: var(--fg); }
.chat-approval-tool {
  font-family: var(--mono, monospace); color: var(--accent);
  background: var(--panel); padding: .05rem .35rem; border-radius: 6px;
}
.chat-approval-reason { color: var(--muted); font-size: .88rem; }
.chat-approval-args {
  margin: 0; padding: .4rem .5rem; border-radius: 6px; overflow-x: auto;
  background: var(--panel); border: 1px solid var(--border);
  font-family: var(--mono, monospace); font-size: .8rem; color: var(--fg);
  white-space: pre-wrap; word-break: break-word;
}
.chat-approval-actions { display: flex; gap: .5rem; }
.chat-approval-approve {
  border: 1px solid var(--accent-2); background: var(--accent-2);
  color: var(--on-accent); border-radius: 8px; padding: .35rem .9rem; cursor: pointer;
}
.chat-approval-approve:hover { filter: brightness(1.08); }
.chat-approval-reject {
  border: 1px solid var(--border); background: var(--panel);
  color: var(--fg); border-radius: 8px; padding: .35rem .9rem; cursor: pointer;
}
.chat-approval-reject:hover { border-color: var(--accent); color: var(--accent); }

/* `ask_user` question form (SOUL §7/§12) — shown above the composer while the
   assistant is waiting on the user's answers. */
.chat-questions-wrap {
  margin: 0 1rem .5rem; padding: .6rem .8rem; border-radius: 10px;
  border: 1px solid var(--accent); background: var(--panel-2);
  display: flex; flex-direction: column; gap: .5rem;
}
.chat-questions-head { font-weight: 600; color: var(--fg); }
.chat-questions { display: flex; flex-direction: column; gap: .5rem; }
.chat-questions-body { display: flex; flex-direction: column; gap: .7rem; }
.chat-question { display: flex; flex-direction: column; gap: .35rem; }
.chat-question-text-label { color: var(--fg); font-size: .92rem; }
.chat-question-opts { display: flex; flex-direction: column; gap: .25rem; }
.chat-question-opt {
  display: flex; align-items: center; gap: .5rem; cursor: pointer;
  color: var(--fg); font-size: .9rem;
}
.chat-question-opt input { accent-color: var(--accent); }
.chat-question-text {
  resize: vertical; background: var(--panel); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .4rem .5rem;
  font: inherit; font-size: .9rem;
}
.chat-questions-actions { display: flex; justify-content: flex-end; }
.chat-questions-submit {
  border: 1px solid var(--accent-2); background: var(--accent-2);
  color: var(--on-accent); border-radius: 8px; padding: .35rem .9rem; cursor: pointer;
}
.chat-questions-submit:hover { filter: brightness(1.08); }
.chat-textarea {
  flex: 1; resize: none; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .5rem .6rem;
  font: inherit; font-size: .92rem;
}
.chat-textarea:focus { outline: none; border-color: var(--accent); }
.chat-send {
  align-self: stretch; min-width: 64px; background: var(--accent-2); color: var(--on-accent);
  border: 0; border-radius: 8px; padding: 0 1rem; font: inherit; font-weight: 600;
  cursor: pointer;
}
.chat-send:disabled { background: var(--panel-2); color: var(--muted); cursor: not-allowed; }
/* Stop-generating button (SOUL §12): shown beside Send while a turn streams. */
.chat-stop {
  align-self: stretch; min-width: 64px; background: var(--panel-2); color: var(--err-fg);
  border: 1px solid var(--err-border); border-radius: 8px; padding: 0 .8rem;
  font: inherit; font-weight: 600; cursor: pointer; white-space: nowrap;
}
.chat-stop:hover { background: var(--err); }
.chat-stop:disabled { color: var(--muted); border-color: var(--border); cursor: default; background: var(--panel-2); }
/* A user message sent while a turn was streaming, not yet placed into the
   conversation: dimmed + dashed until the server's ack lands. */
.msg-queued .msg-text { opacity: .6; border-style: dashed; }
.msg-queued-tag {
  margin-left: .4rem; font-size: .58rem; font-weight: 700; letter-spacing: .3px;
  color: var(--muted); background: var(--panel-2); border: 1px dashed var(--border);
  border-radius: 4px; padding: .02rem .3rem; cursor: help;
}
/* Uploaded files shown on top of a sent message bubble: image thumbnails and
   labelled download chips, above the message text. */
.msg-attachments { display: flex; flex-wrap: wrap; gap: .4rem; margin: .1rem 0 .15rem; }
.msg-attachment { display: inline-flex; align-items: center; gap: .35rem; text-decoration: none; color: var(--fg); }
.msg-attachment-img {
  max-width: 12rem; max-height: 9rem; border-radius: 8px;
  border: 1px solid var(--border); object-fit: cover; display: block;
}
.msg-attachment-link, .msg-attachment-unsafe {
  max-width: 18rem; background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 999px; padding: .15rem .7rem; font-size: .82rem;
}
.msg-attachment-link:hover { border-color: var(--accent); }
.msg-attachment-icon { opacity: .8; }
.msg-attachment-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* Staged chat attachments (uploaded; sent as references). */
.chat-attachments { display: flex; flex-wrap: wrap; gap: .4rem; }
.chat-attachment-chip {
  display: inline-flex; align-items: center; gap: .35rem; max-width: 18rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 999px;
  padding: .15rem .25rem .15rem .65rem; font-size: .82rem; color: var(--fg);
}
.chat-attachment-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.chat-attachment-remove {
  display: inline-flex; align-items: center; justify-content: center;
  width: 1.15rem; height: 1.15rem; border: 0; border-radius: 999px;
  background: transparent; color: var(--muted); cursor: pointer; font-size: .95rem; line-height: 1;
}
.chat-attachment-remove:hover { background: var(--border); color: var(--fg); }
.chat-attach-status { font-size: .82rem; color: var(--muted); }
.chat-attach-error { font-size: .82rem; color: var(--err-fg); }
.chat-attach-btn {
  align-self: stretch; display: inline-flex; align-items: center; justify-content: center;
  min-width: 42px; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; cursor: pointer; font-size: 1.05rem;
}
.chat-attach-btn:hover { border-color: var(--accent); }
/* Microphone dictation button (SOUL §7): shown only when the gateway offers STT
   models. Turns red and pulses while recording; disabled while transcribing. */
.chat-mic {
  align-self: stretch; display: inline-flex; align-items: center; justify-content: center;
  min-width: 42px; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; cursor: pointer; font-size: 1.05rem;
}
.chat-mic:hover:not(:disabled) { border-color: var(--accent); }
.chat-mic:disabled { color: var(--muted); cursor: default; }
.chat-capability-checking:disabled { cursor: progress; }
.chat-mic-recording {
  color: var(--on-accent); background: var(--err); border-color: var(--err-border);
  animation: chat-mic-pulse 1.2s ease-in-out infinite;
}
@keyframes chat-mic-pulse {
  0%, 100% { box-shadow: 0 0 0 0 var(--err-border); }
  50% { box-shadow: 0 0 0 4px transparent; }
}

/* --- Voice conversation overlay (SOUL §7/§12) --- */
/* The 🎧 opener sits beside the mic and shares its chrome. */
.chat-voice {
  align-self: stretch; display: inline-flex; align-items: center; justify-content: center;
  min-width: 42px; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; cursor: pointer; font-size: 1.05rem;
}
.chat-voice:hover:not(:disabled) { border-color: var(--accent); }
.chat-voice:disabled { color: var(--muted); cursor: default; }
/* Full-screen opaque takeover, above the context menus (70). */
.voice-overlay {
  position: fixed; inset: 0; z-index: 80; background: var(--bg);
  display: flex; flex-direction: column; align-items: center; justify-content: center;
  gap: 1.2rem; padding: 1.5rem; text-align: center;
}
/* The sound-reactive orb: `--voice-level` (0..1, set from the live mic or
   playback analyser) drives scale + glow; the rings ripple harder. Short
   ease-out transitions smooth the 50–120 ms meter ticks into fluid motion. */
.voice-orb-wrap { position: relative; width: min(42vmin, 260px); aspect-ratio: 1; cursor: pointer; }
.voice-orb {
  position: absolute; inset: 0; border-radius: 50%;
  background: radial-gradient(circle at 35% 30%,
    color-mix(in srgb, var(--accent) 80%, var(--fg)), var(--accent));
  transform: scale(calc(1 + var(--voice-level, 0) * .3));
  box-shadow: 0 0 calc(1.5rem + var(--voice-level, 0) * 4rem)
    color-mix(in srgb, var(--accent) 55%, transparent);
  transition: transform .15s ease-out, box-shadow .15s ease-out, filter .3s ease;
}
.voice-ring {
  position: absolute; inset: -8%; border-radius: 50%;
  border: 2px solid color-mix(in srgb, var(--accent) 40%, transparent);
  transform: scale(calc(1 + var(--voice-level, 0) * .5));
  transition: transform .18s ease-out, opacity .18s ease-out;
  pointer-events: none;
}
.voice-ring-b {
  inset: -16%; opacity: .6;
  border-color: color-mix(in srgb, var(--accent) 25%, transparent);
  transform: scale(calc(1 + var(--voice-level, 0) * .8));
}
/* Speaking swaps the dominant tone so the reply reads as "the other party". */
.voice-speaking .voice-orb {
  background: radial-gradient(circle at 35% 30%,
    color-mix(in srgb, var(--accent-2) 80%, var(--fg)), var(--accent-2));
  box-shadow: 0 0 calc(1.5rem + var(--voice-level, 0) * 4rem)
    color-mix(in srgb, var(--accent-2) 55%, transparent);
}
.voice-speaking .voice-ring { border-color: color-mix(in srgb, var(--accent-2) 40%, transparent); }
.voice-speaking .voice-ring-b { border-color: color-mix(in srgb, var(--accent-2) 25%, transparent); }
/* Thinking: a slow breathing pulse + one spinning ring while the turn runs. */
.voice-thinking .voice-orb, .voice-transcribing .voice-orb {
  animation: voice-think 1.6s ease-in-out infinite;
}
.voice-thinking .voice-ring-a, .voice-transcribing .voice-ring-a {
  border-top-color: var(--accent);
  animation: voice-ring-spin 2.4s linear infinite;
}
@keyframes voice-think {
  0%, 100% { transform: scale(1); }
  50% { transform: scale(1.07); }
}
@keyframes voice-ring-spin {
  from { transform: rotate(0deg); }
  to { transform: rotate(360deg); }
}
.voice-paused .voice-orb { filter: grayscale(.65); animation: none; }
.voice-paused .voice-ring { opacity: .3; }
.voice-status { color: var(--fg); font-size: 1.05rem; min-height: 1.4em; }
.voice-heard { color: var(--muted); max-width: 40rem; }
.voice-error { color: var(--err-fg); max-width: 40rem; }
.voice-close {
  position: fixed; top: 1rem; right: 1rem; min-width: 42px; min-height: 42px;
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; cursor: pointer; font-size: 1.05rem;
}
.voice-close:hover { border-color: var(--accent); }
.voice-pause {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; cursor: pointer; padding: .45rem 1.1rem; font-size: .95rem;
}
.voice-pause:hover { border-color: var(--accent); }

/* --- Calendar panel (M2) --- */
.cal-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.cal-header {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem;
  padding: .8rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.cal-header-titles { display: flex; flex-direction: column; gap: .15rem; }
.cal-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.cal-subtitle { color: var(--muted); font-size: .82rem; }
.cal-header-actions { display: flex; gap: .5rem; flex-shrink: 0; }
.cal-filter {
  display: flex; align-items: center; gap: .4rem; flex-wrap: wrap;
  padding: .5rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.cal-filter-label { font-size: .78rem; color: var(--muted); }
.cal-filter-date {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .3rem .5rem; font: inherit; font-size: .82rem;
}
.cal-filter-date:focus { outline: none; border-color: var(--accent); }
.cal-filter-clear {
  background: transparent; color: var(--muted); border: 1px solid var(--border);
  border-radius: 8px; padding: .3rem .6rem; font: inherit; font-size: .78rem; cursor: pointer;
}
.cal-filter-clear:hover { color: var(--fg); border-color: var(--accent); }
.cal-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .45rem .8rem; font: inherit; font-size: .85rem;
  font-weight: 600; cursor: pointer;
}
.cal-btn:hover:not(:disabled) { border-color: var(--accent); }
.cal-btn:disabled { color: var(--muted); cursor: not-allowed; }
.cal-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.cal-btn-primary:hover:not(:disabled) { background: var(--accent); }
.cal-connect {
  display: flex; flex-direction: column; gap: .6rem;
  padding: .9rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel-2);
}
.cal-field { display: flex; flex-direction: column; gap: .25rem; max-width: 520px; }
.cal-label { font-size: .72rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; }
.cal-calendar-picker { display: flex; align-items: stretch; gap: .4rem; }
.cal-calendar-picker .cal-input { flex: 1; min-width: 0; }
.cal-newcal-inline { white-space: nowrap; }
.cal-empty-editor { display: flex; align-items: center; gap: .65rem; flex-wrap: wrap; }
.cal-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .5rem .6rem; font: inherit; font-size: .9rem;
}
.cal-input:focus { outline: none; border-color: var(--accent); }
.cal-form-actions { margin-top: .2rem; }
.cal-form-error, .cal-error { color: var(--err-fg); }
.cal-form-error { font-size: .85rem; }
.cal-notice {
  margin: .7rem 1rem 0; padding: .55rem .7rem; font-size: .85rem;
  background: var(--user); border: 1px solid var(--accent-2); border-radius: 8px;
}
.cal-body { flex: 1; min-height: 0; overflow-y: auto; padding: 1rem; }
.cal-status { color: var(--muted); margin: 1.5rem auto; text-align: center; max-width: 40ch; }
.cal-status p { margin: .2rem 0; }
.cal-muted { font-size: .85rem; }
.cal-agenda { display: flex; flex-direction: column; gap: 1.2rem; max-width: 760px; }
.cal-day { display: flex; flex-direction: column; gap: .35rem; }
.cal-day-heading {
  margin: 0; font-size: .8rem; font-weight: 700; color: var(--accent);
  text-transform: uppercase; letter-spacing: .6px;
  position: sticky; top: -1rem; background: var(--bg); padding: .25rem 0;
}
.cal-event-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.cal-event {
  display: flex; gap: .8rem; align-items: baseline;
  padding: .55rem .7rem; background: var(--panel); border: 1px solid var(--border);
  border-radius: 9px; border-left: 3px solid var(--accent-2);
}
.cal-event-time {
  flex-shrink: 0; min-width: 6.5rem; font-variant-numeric: tabular-nums;
  font-size: .82rem; color: var(--muted);
}
.cal-event-main { display: flex; flex-direction: column; gap: .2rem; min-width: 0; flex: 1; }
.cal-event-summary {
  display: flex; align-items: center; gap: .4rem; width: 100%;
  background: transparent; border: 0; color: inherit; font: inherit; text-align: left;
  padding: 0; font-size: .95rem; font-weight: 600; cursor: pointer;
}
.cal-event-summary:disabled { cursor: default; }
.cal-event-summary:not(:disabled):hover .cal-event-summary-text { color: var(--accent); }
.cal-event-summary-text { word-break: break-word; }
.cal-caret { color: var(--muted); font-size: .7rem; flex-shrink: 0; }
.cal-recur { color: var(--accent); font-size: .85rem; flex-shrink: 0; }
.cal-event-detail {
  margin-top: .35rem; display: flex; flex-direction: column; gap: .35rem;
  padding: .5rem .6rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.cal-event-body { margin: 0; white-space: pre-wrap; word-break: break-word; font-size: .85rem; color: var(--fg); line-height: 1.5; }
.cal-event-kv { display: flex; gap: .6rem; font-size: .82rem; }
.cal-event-detail-k {
  flex-shrink: 0; min-width: 5rem; color: var(--muted);
  text-transform: uppercase; font-size: .64rem; letter-spacing: .5px; padding-top: .15rem;
}
.cal-event-rrule { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; word-break: break-all; }
.cal-event-meta { display: flex; flex-wrap: wrap; gap: .4rem; align-items: center; }
.cal-event-loc { font-size: .8rem; color: var(--muted); }
.cal-event-cal {
  font-size: .7rem; color: var(--fg); background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 5px; padding: .05rem .4rem;
}
/* The edit/delete pair (shared `.row-act` buttons) renders together under one
   writable gate; the auto margin pushes the pair to the meta line's right. */
.cal-event-acts { margin-left: auto; }

/* --- Calendar: description textarea, labels, attachments --- */
.cal-textarea { resize: vertical; min-height: 2.4rem; font: inherit; line-height: 1.5; }
.cal-event-label {
  font-size: .68rem; color: var(--accent); background: color-mix(in srgb, var(--accent) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--accent) 35%, transparent); border-radius: 999px; padding: .03rem .4rem;
}
.cal-event-attach-icon { font-size: .72rem; opacity: .75; }
/* Agenda label filter bar */
.cal-labelbar { display: flex; flex-wrap: wrap; align-items: center; gap: .35rem; padding: 0 1rem .6rem; }
.cal-label-chip {
  display: inline-flex; align-items: center; gap: .3rem;
  font-size: .74rem; color: var(--muted); background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 999px; padding: .14rem .6rem; cursor: pointer;
  transition: color .12s ease, border-color .12s ease, background-color .12s ease;
}
.cal-label-chip:hover { color: var(--fg); border-color: var(--accent); }
.cal-label-chip:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
.cal-label-chip-on { color: var(--fg); background: color-mix(in srgb, var(--accent) 18%, transparent); border-color: var(--accent); }
.cal-label-hash { opacity: .55; }
.cal-label-count {
  font-size: .62rem; line-height: 1; color: var(--muted);
  background: color-mix(in srgb, var(--fg) 9%, transparent);
  border-radius: 999px; padding: .12rem .32rem;
}
.cal-label-chip-on .cal-label-count { color: var(--fg); background: color-mix(in srgb, var(--accent) 22%, transparent); }
/* Add-event form: staged attachments + add controls */
.cal-attach-staged { list-style: none; margin: 0 0 .4rem; padding: 0; display: flex; flex-direction: column; gap: .25rem; }
.cal-attach-staged-item { display: flex; align-items: center; gap: .4rem; font-size: .82rem; }
.cal-attach-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; max-width: 22rem; }
.cal-attach-del {
  font-size: .72rem; line-height: 1; color: var(--muted); background: transparent;
  border: 1px solid transparent; border-radius: 5px; padding: .05rem .3rem; cursor: pointer;
}
.cal-attach-del:hover { color: var(--fg); border-color: var(--border); }
.cal-attach-add { display: flex; gap: .4rem; margin-bottom: .4rem; }
.cal-attach-url { flex: 1; }
.cal-attach-upload { display: flex; align-items: center; gap: .5rem; font-size: .82rem; }
/* Event-detail attachment thumbnails / links */
.cal-attach-list { display: flex; flex-wrap: wrap; gap: .5rem; }
.cal-attach-item { display: inline-flex; align-items: center; gap: .35rem; text-decoration: none; color: var(--fg); }
.cal-attach-img {
  max-width: 140px; max-height: 110px; border-radius: 6px; border: 1px solid var(--border); object-fit: cover; display: block;
}
.cal-attach-link {
  font-size: .82rem; background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 6px; padding: .2rem .5rem;
}
.cal-attach-link:hover { border-color: var(--accent); }
.cal-attach-file-icon { opacity: .8; }
.cal-attach-unsafe {
  font-size: .82rem; color: var(--muted); background: var(--panel-2);
  border: 1px dashed var(--border); border-radius: 6px; padding: .2rem .5rem;
}

/* --- Calendar: activate/deactivate sidebar (a narrow shared `.pane-list`) --- */
.cal-sidebar { width: 240px; }
.cal-sidebar-count {
  font-size: .72rem; color: var(--muted); font-variant-numeric: tabular-nums;
}
.cal-sidebar-body { padding: .3rem; }
.cal-cal-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .1rem; }
.cal-cal-item { display: flex; align-items: center; gap: .15rem; }
.cal-toggle {
  display: flex; align-items: center; gap: .5rem; flex: 1; min-width: 0;
  padding: .45rem .6rem; border-radius: 8px; cursor: pointer;
}
.cal-calendar-dot {
  width: .55rem; height: .55rem; border-radius: 50%; flex: 0 0 auto;
  box-shadow: 0 0 0 2px color-mix(in srgb, currentColor 10%, transparent);
}
.cal-toggle:hover { background: var(--panel-2); }
/* The per-calendar delete (a shared `.row-act` inside a `.row-acts-reveal`
   wrapper) stays hidden until the row is hovered, so the sidebar reads calm —
   the reveal itself is the shared `.cal-cal-item:hover .row-acts-reveal` rule. */
.cal-toggle-box { flex-shrink: 0; accent-color: var(--accent-2); cursor: pointer; }
.cal-cal-name {
  font-size: .88rem; word-break: break-word;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.cal-cal-off { color: var(--muted); text-decoration: line-through; }
.cal-sidebar-actions {
  display: flex; gap: .5rem; padding: .55rem .8rem;
  border-top: 1px solid var(--border);
}
.cal-sidebar-link {
  background: transparent; color: var(--muted); border: 0; padding: 0;
  font: inherit; font-size: .78rem; cursor: pointer;
}
.cal-sidebar-link:hover { color: var(--accent); text-decoration: underline; }
.cal-sources { border-top: 1px solid var(--border); padding: .55rem .5rem .7rem; }
.cal-sources-header {
  display: flex; align-items: center; gap: .35rem; width: 100%;
  background: transparent; border: none; border-radius: 6px;
  padding: .2rem .3rem .35rem; font: inherit; cursor: pointer; text-align: left;
}
.cal-sources-header:hover { background: var(--panel-2); }
.cal-sources-arrow { flex-shrink: 0; width: .9rem; color: var(--muted); font-size: .7rem; }
.cal-sources-title { font-size: .82rem; text-transform: uppercase; letter-spacing: .5px; color: var(--muted); }
.cal-sources-count { color: var(--muted); font-size: .72rem; }
.cal-sources-warn { margin-left: auto; font-size: .78rem; }
.cal-source-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .15rem; }
.cal-source {
  display: flex; flex-direction: column; gap: .25rem;
  padding: .35rem .4rem; border-radius: 8px;
}
.cal-source-row { display: flex; align-items: center; gap: .4rem; }
.cal-source:hover { background: var(--panel-2); }
/* Dormant source (SOUL §29): configured but no Collect automation ingests it. */
.cal-source-idle {
  margin: 0; font-size: .68rem; line-height: 1.35;
  color: var(--warn-fg); background: var(--warn-bg);
  border: 1px solid var(--warn-border); border-radius: 6px;
  padding: .25rem .4rem;
}
.cal-source-name {
  flex: 1; min-width: 0; font-size: .82rem;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.cal-source-state { flex-shrink: 0; font-size: .68rem; color: var(--muted); }
.cal-source-synced { color: var(--accent); }

/* --- Calendar: view switcher + grid navigation --- */
.cal-viewbar {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem; flex-wrap: wrap;
  padding: .5rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.cal-viewtabs {
  display: inline-flex; gap: .15rem; background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 9px; padding: .15rem;
}
.cal-viewtab {
  background: transparent; color: var(--muted); border: 0; border-radius: 7px;
  padding: .3rem .75rem; font: inherit; font-size: .82rem; font-weight: 600; cursor: pointer;
}
.cal-viewtab:hover { color: var(--fg); }
.cal-viewtab-on { background: var(--accent-2); color: var(--on-accent); }
.cal-nav { display: flex; align-items: center; gap: .4rem; }
.cal-nav-btn, .cal-nav-today {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; font: inherit; cursor: pointer;
}
.cal-nav-btn { width: 30px; height: 30px; padding: 0; font-size: 1.1rem; line-height: 1; }
.cal-nav-today { padding: .35rem .7rem; font-size: .8rem; font-weight: 600; }
.cal-nav-btn:hover, .cal-nav-today:hover { border-color: var(--accent); }
.cal-nav-title { margin-right: .5rem; font-size: .95rem; font-weight: 700; font-variant-numeric: tabular-nums; }

/* --- Calendar: month grid --- */
.cal-month { display: flex; flex-direction: column; }
.cal-month-hdr { display: grid; grid-template-columns: repeat(7, minmax(0, 1fr)); }
.cal-month-hcell {
  padding: .25rem .5rem; font-size: .7rem; font-weight: 700; color: var(--muted);
  text-transform: uppercase; letter-spacing: .5px;
}
.cal-month-grid {
  display: grid; grid-template-columns: repeat(7, minmax(0, 1fr));
  grid-auto-rows: minmax(96px, 1fr); gap: 1px;
  background: var(--border); border: 1px solid var(--border); border-radius: 10px; overflow: hidden;
}
.cal-mcell {
  background: var(--panel); display: flex; flex-direction: column; gap: .2rem;
  padding: .25rem; min-width: 0; min-height: 0;
}
.cal-mcell-out { background: var(--bg); }
.cal-mcell-out .cal-mcell-num { color: var(--muted); }
.cal-mcell-today { background: var(--user); }
.cal-mcell-num {
  align-self: flex-start; width: 1.6rem; height: 1.6rem; border-radius: 50%;
  background: transparent; border: 0; color: var(--fg); font: inherit; font-size: .82rem;
  font-weight: 600; cursor: pointer; font-variant-numeric: tabular-nums;
}
.cal-mcell-num:hover { background: var(--panel-2); }
.cal-mcell-today .cal-mcell-num { background: var(--accent-2); color: var(--on-accent); }
.cal-mcell-evs { display: flex; flex-direction: column; gap: .12rem; min-width: 0; overflow: hidden; }
.cal-chip {
  display: flex; gap: .3rem; align-items: baseline; width: 100%; text-align: left;
  background: var(--panel-2); border: 1px solid var(--border); border-left: 3px solid var(--accent-2);
  border-radius: 5px; padding: .1rem .3rem; font: inherit; font-size: .72rem; cursor: pointer;
  white-space: nowrap; overflow: hidden;
}
.cal-chip:hover { border-color: var(--accent); }
.cal-chip-time { color: var(--muted); font-variant-numeric: tabular-nums; flex-shrink: 0; }
.cal-chip-text { overflow: hidden; text-overflow: ellipsis; }
.cal-chip-more {
  background: transparent; border: 0; color: var(--muted); font: inherit; font-size: .7rem;
  text-align: left; padding: .05rem .3rem; cursor: pointer;
}
.cal-chip-more:hover { color: var(--accent); text-decoration: underline; }

/* --- Calendar: week / day time grid --- */
.cal-tg { --tg-h: 1152px; --tg-gutter: 52px; --tg-head-h: 54px; display: flex; flex-direction: column; min-width: 0; }
.cal-tg-single { max-width: 760px; }
/* The header keeps a fixed height (`--tg-head-h`) regardless of whether today is
   in view, so the all-day row's sticky offset below it stays exact — the day
   number always occupies the same 1.7rem box, today only tints it. */
.cal-tg-head {
  display: grid; grid-template-columns: var(--tg-gutter) 1fr; height: var(--tg-head-h);
  position: sticky; top: -1rem; z-index: 2; background: var(--bg); border-bottom: 1px solid var(--border);
}
.cal-tg-headcells { display: grid; }
.cal-tg-dayhead {
  display: flex; flex-direction: column; align-items: center; justify-content: center; gap: .1rem;
  border: 0; border-left: 1px solid var(--border); padding: 0;
  background: transparent; color: var(--fg); font: inherit; cursor: pointer;
}
.cal-tg-dayhead:hover { background: var(--panel-2); }
.cal-tg-dayhead:focus-visible { outline: 2px solid var(--accent); outline-offset: -2px; }
.cal-tg-dow { font-size: .66rem; font-weight: 700; text-transform: uppercase; letter-spacing: .5px; color: var(--muted); }
.cal-tg-dnum {
  font-size: 1rem; font-weight: 700; font-variant-numeric: tabular-nums; border-radius: 50%;
  width: 1.7rem; height: 1.7rem; display: inline-flex; align-items: center; justify-content: center;
}
.cal-tg-dayhead.cal-tg-today .cal-tg-dnum { background: var(--accent-2); color: var(--on-accent); }
.cal-tg-allday {
  display: grid; grid-template-columns: var(--tg-gutter) 1fr;
  position: sticky; top: calc(-1rem + var(--tg-head-h)); z-index: 1; background: var(--bg);
  border-bottom: 1px solid var(--border);
}
.cal-tg-allgutter {
  font-size: .6rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px;
  padding: .3rem .35rem; text-align: right;
}
.cal-tg-allbody { display: flex; flex-direction: column; gap: 2px; padding: .15rem 0; min-width: 0; }
.cal-tg-allrow { display: grid; gap: 1px; }
.cal-tg-allbar {
  min-width: 0; background: var(--panel-2); border: 1px solid var(--border);
  border-left: 3px solid var(--accent-2); border-radius: 5px; padding: .1rem .4rem;
  color: var(--fg); font: inherit; font-size: .74rem; text-align: left;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis; cursor: pointer;
}
.cal-tg-allbar:hover:not(:disabled) { border-color: var(--accent); }
.cal-tg-allbar:disabled { cursor: default; }
.cal-tg-allmore { font-size: .7rem; color: var(--muted); padding: .1rem .4rem; }
.cal-tg-grid { display: grid; grid-template-columns: var(--tg-gutter) 1fr; }
.cal-tg-hours { display: flex; flex-direction: column; }
.cal-tg-hour { height: 48px; position: relative; }
.cal-tg-hour span {
  position: absolute; top: -.55rem; right: .4rem; font-size: .64rem;
  color: var(--muted); font-variant-numeric: tabular-nums;
}
.cal-tg-cols { display: grid; height: var(--tg-h); min-width: 0; }
.cal-tg-col {
  position: relative; min-width: 0; border-left: 1px solid var(--border);
  cursor: crosshair;
  background: repeating-linear-gradient(
    to bottom, transparent 0, transparent 47px, var(--border) 47px, var(--border) 48px);
}
.cal-tg-col.cal-tg-today { background-color: var(--user); }
/* Current-time line: a thin accent rule across today's column with a dot on the
   left edge and a time pill riding on it, so the active / next event reads at a
   glance. Sits above the event blocks and ignores pointer events so it never
   swallows a click on an event underneath. */
.cal-tg-now {
  position: absolute; left: 0; right: 0; height: 0; z-index: 3;
  pointer-events: none; border-top: 2px solid var(--now);
}
.cal-tg-now::before {
  content: ""; position: absolute; left: -4px; top: -4px;
  width: 8px; height: 8px; border-radius: 50%; background: var(--now);
}
.cal-tg-now-label {
  position: absolute; left: 8px; top: 0; transform: translateY(-50%);
  font-size: .58rem; font-weight: 700; font-variant-numeric: tabular-nums;
  line-height: 1; color: var(--on-accent); background: var(--now);
  padding: 1px 4px; border-radius: 5px;
}
.cal-tg-block {
  position: absolute; box-sizing: border-box; overflow: hidden;
  display: flex; flex-direction: column; gap: .05rem; min-height: 1.4rem; line-height: 1.2;
  background: var(--panel); border: 1px solid var(--border); border-left: 3px solid var(--accent-2);
  border-radius: 6px; padding: .1rem .3rem; color: var(--fg); font: inherit;
  text-align: left; font-size: .72rem; cursor: pointer;
}
.cal-tg-block:hover:not(:disabled) {
  z-index: 2; border-color: var(--accent); box-shadow: 0 3px 12px color-mix(in srgb, var(--fg) 12%, transparent);
}
.cal-tg-block:disabled { cursor: default; }
/* Short blocks (≲45 min) are too shallow for two stacked lines, so the time and
   title share one line — time first, title ellipsised — instead of clipping the
   title mid-glyph. */
.cal-tg-block-short {
  flex-direction: row; align-items: baseline; gap: .3rem;
  padding-top: 0; padding-bottom: 0;
}
.cal-tg-btime {
  color: var(--muted); font-size: .64rem; font-variant-numeric: tabular-nums;
  white-space: nowrap; flex-shrink: 0;
}
.cal-tg-bsum {
  font-weight: 600; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}

/* --- Notes panel (M3) — shared `.pane-*` chrome + notes-only row badges --- */
.notes-item-agent {
  font-size: .6rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px;
  color: var(--ok-fg); background: var(--panel); border: 1px solid var(--ok-border);
  border-radius: 4px; padding: 0 .3rem; flex-shrink: 0;
}
.notes-item-tags { display: flex; flex-wrap: wrap; gap: .25rem; margin-top: .1rem; }
.notes-item-tag {
  font-size: .64rem; color: var(--muted); background: var(--panel);
  border: 1px solid var(--border); border-radius: 4px; padding: 0 .3rem;
}
/* Tag filter bar above the notes list. */
.notes-tagbar {
  display: flex; flex-wrap: wrap; gap: .3rem; padding: .4rem .5rem;
  border-bottom: 1px solid var(--border);
}
.notes-tag-chip {
  font-size: .7rem; color: var(--muted); background: transparent;
  border: 1px solid var(--border); border-radius: 999px; padding: .1rem .55rem; cursor: pointer;
}
.notes-tag-chip:hover { color: var(--fg); background: var(--panel-2); }
.notes-tag-chip-active { color: var(--on-accent); background: var(--accent-2); border-color: var(--accent-2); }
.notes-form { flex: 1; display: flex; flex-direction: column; gap: .6rem; padding: 1rem; min-height: 0; }
.notes-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .55rem .7rem; font: inherit;
}
.notes-input:focus { outline: none; border-color: var(--accent); }
.notes-input-title { font-size: 1.05rem; font-weight: 700; }
.notes-input-tags { font-size: .85rem; }
.notes-toolbar {
  display: flex; flex-wrap: wrap; gap: .35rem; align-items: center;
  padding: .15rem 0 .05rem;
}
.notes-tool {
  width: 2rem; height: 1.9rem; display: inline-flex; align-items: center; justify-content: center;
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; font: inherit; font-size: .78rem; font-weight: 700; cursor: pointer;
}
.notes-tool:hover:not(:disabled) { border-color: var(--accent); color: var(--accent); }
.notes-tool:disabled { color: var(--muted); cursor: not-allowed; }
.notes-tool-italic { font-style: italic; }
.notes-wysiwyg { flex: 1; min-height: 0; display: grid; grid-template-columns: minmax(0, 1fr) minmax(0, 1fr); gap: .8rem; }
.notes-pane { min-height: 0; min-width: 0; display: flex; flex-direction: column; gap: .35rem; }
.notes-pane-label {
  color: var(--muted); font-size: .68rem; font-weight: 700; text-transform: uppercase;
  letter-spacing: .6px;
}
.notes-textarea {
  flex: 1; min-height: 12rem; resize: none; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .6rem .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .9rem; line-height: 1.5;
}
.notes-textarea:focus { outline: none; border-color: var(--accent); }
.notes-preview {
  flex: 1; min-height: 12rem; overflow: auto; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .75rem .85rem;
  line-height: 1.55; overflow-wrap: anywhere;
}
.notes-preview h1, .notes-preview h2, .notes-preview h3,
.notes-preview h4, .notes-preview h5, .notes-preview h6 {
  margin: 0 0 .7rem; line-height: 1.2;
}
.notes-preview h1 { font-size: 1.35rem; }
.notes-preview h2 { font-size: 1.14rem; }
.notes-preview h3 { font-size: 1rem; }
.notes-preview h4 { font-size: .92rem; }
.notes-preview h5 { font-size: .85rem; }
.notes-preview h6 { font-size: .85rem; color: var(--muted); }
.notes-preview p, .notes-preview ul, .notes-preview ol, .notes-preview blockquote, .notes-preview pre {
  margin: 0 0 .75rem;
}
.notes-preview ul, .notes-preview ol { padding-left: 1.25rem; }
.notes-preview li { margin: .18rem 0; }
.notes-preview li > ul, .notes-preview li > ol { margin: .18rem 0 0; }
.notes-preview hr { border: 0; border-top: 1px solid var(--border); margin: .9rem 0; }
.notes-preview blockquote {
  border-left: 3px solid var(--accent-2); padding-left: .7rem; color: var(--fg);
}
.notes-preview code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 5px; padding: .05rem .3rem; font-size: .85em;
}
.notes-preview pre {
  background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
  padding: .7rem; overflow-x: auto;
}
.notes-preview pre code { background: transparent; border: 0; padding: 0; }
.notes-preview a { color: var(--accent); }
.notes-preview-empty { color: var(--muted); }
.notes-form-error { color: var(--err-fg); font-size: .85rem; }
.notes-form-actions { display: flex; gap: .5rem; }

/* --- Files panel (M3 object-storage browser) --- */
.files-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.files-header {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem;
  padding: .8rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.files-header-titles { display: flex; flex-direction: column; gap: .15rem; }
.files-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.files-subtitle { color: var(--muted); font-size: .82rem; }
.files-actions { display: flex; gap: .5rem; flex-shrink: 0; align-items: center; }
.files-filter { display: flex; gap: .5rem; }
.files-upload { cursor: pointer; }
.files-upload-input { display: none; }
.files-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .45rem .6rem; font: inherit; font-size: .85rem; min-width: 16rem;
}
.files-input:focus { outline: none; border-color: var(--accent); }
.files-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .8rem; font: inherit; font-size: .82rem;
  font-weight: 600; cursor: pointer; text-decoration: none; display: inline-block;
}
.files-btn:hover:not(:disabled) { border-color: var(--accent); }
.files-btn:disabled { color: var(--muted); cursor: not-allowed; }
.files-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.files-btn-primary:hover:not(:disabled) { background: var(--accent); }
.files-btn-link { color: var(--accent); }
.files-btn-danger { background: var(--err); color: var(--err-fg); border-color: var(--err-border); }
.files-btn-danger:hover:not(:disabled) { border-color: var(--err-fg); }
.files-banner { margin: .7rem 1rem 0; padding: .5rem .7rem; font-size: .85rem; border-radius: 8px; }
.files-error { color: var(--err-fg); }
.files-banner.files-error { background: var(--err); border: 1px solid var(--err-border); color: var(--err-fg); }
.files-banner.files-notice {
  background: var(--panel-2); border: 1px solid var(--border); color: var(--fg);
  display: flex; align-items: center; gap: .6rem; justify-content: space-between;
}
.files-store-select { max-width: 16ch; }
.files-manager { margin: .7rem 1rem 0; padding: .8rem; border: 1px solid var(--border); border-radius: 10px; background: var(--panel-2, rgba(255,255,255,.02)); }
.files-store-list { list-style: none; margin: 0 0 .8rem; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.files-store-row { display: flex; gap: .6rem; align-items: center; font-size: .85rem; }
.files-store-name { font-weight: 600; }
.files-store-row .files-type { color: var(--muted); }
.files-store-row .files-btn-danger { margin-left: auto; }
.files-store-form { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.files-store-check { display: inline-flex; gap: .35rem; align-items: center; color: var(--muted); font-size: .82rem; }
.files-body { flex: 1; min-height: 0; overflow-y: auto; padding: 1rem; }
.files-status { color: var(--muted); margin: 1.2rem auto; text-align: center; max-width: 48ch; }
.files-type { color: var(--muted); font-size: .82rem; word-break: break-word; }
/* Filesystem tree — the Files panel browses a store's backend as an expandable
   directory tree (indent is an inline padding-left from each row's depth). */
.files-tree { display: flex; flex-direction: column; }
.files-tree-row {
  display: flex; align-items: center; gap: .5rem; padding: .3rem .6rem;
  border-bottom: 1px solid var(--border); font-size: .88rem; min-width: 0;
}
.files-tree-row:hover { background: var(--panel); }
.files-tree-dir { cursor: pointer; user-select: none; font-weight: 600; }
.files-tree-caret { width: .9rem; flex: none; color: var(--muted); text-align: center; }
.files-tree-icon { flex: none; }
.files-tree-name { flex: 1 1 auto; min-width: 0; word-break: break-word; }
.files-tree-type {
  flex: none; color: var(--muted); font-size: .8rem; max-width: 14ch;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.files-tree-size {
  flex: none; color: var(--muted); font-variant-numeric: tabular-nums;
  white-space: nowrap; min-width: 5.5ch; text-align: right;
}
.files-tree-modified {
  flex: none; color: var(--muted); font-variant-numeric: tabular-nums; white-space: nowrap;
}
.files-tree-cell { flex: none; text-align: center; min-width: 1.5rem; }
.files-tree-actions { flex: none; display: flex; gap: .4rem; white-space: nowrap; }
.files-tree-truncated { text-align: left; margin: .6rem; font-size: .8rem; }
/* File/dir labels — chips shown inline on a tree row, each with a remove ("×"),
   plus an inline add ("+"). The strip stops click propagation so tagging a
   directory row never toggles its expand. */
.files-labels { flex: 0 1 auto; display: inline-flex; align-items: center; gap: .3rem; flex-wrap: wrap; min-width: 0; }
.files-label-chip {
  display: inline-flex; align-items: center; gap: .15rem; padding: .02rem .45rem;
  border-radius: 999px; background: var(--panel-2); color: var(--muted);
  border: 1px solid var(--border); font-size: .72rem; line-height: 1.5;
}
.files-label-text { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; max-width: 16ch; }
.files-label-x {
  background: transparent; border: 0; cursor: pointer; color: var(--muted);
  font: inherit; line-height: 1; padding: 0 .05rem;
}
.files-label-x:hover { color: var(--err); }
.files-label-add {
  background: transparent; border: 1px dashed var(--border); cursor: pointer;
  color: var(--muted); border-radius: 999px; font-size: .72rem; line-height: 1.3;
  padding: .02rem .4rem;
}
.files-label-add:hover { background: var(--panel-2); color: var(--fg); }
/* Content-search results list (object excerpts). */
.files-hits { list-style: none; margin: 0; padding: .4rem; display: flex; flex-direction: column; gap: .4rem; }
.files-hit {
  display: flex; flex-direction: column; gap: .3rem; padding: .55rem .65rem;
  border: 1px solid var(--border); border-radius: 8px; background: var(--panel);
}
.files-hit-head { display: flex; align-items: baseline; gap: .5rem; justify-content: space-between; }
.files-hit-name { font-weight: 600; font-size: .9rem; word-break: break-word; }
.files-hit-excerpt {
  font-size: .8rem; color: var(--muted); white-space: pre-wrap; word-break: break-word;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  display: -webkit-box; -webkit-line-clamp: 3; -webkit-box-orient: vertical; overflow: hidden;
}
.files-hit-actions { display: flex; gap: .5rem; }
.files-modified { color: var(--muted); font-variant-numeric: tabular-nums; white-space: nowrap; }
.files-badge { color: var(--muted); }
.files-badge-on { color: var(--accent); font-weight: 700; }
/* The "Indexed ✓" badge doubles as a button opening the extracted-text viewer. */
.files-badge-btn {
  background: transparent; border: 0; cursor: pointer; font: inherit; font-weight: 700;
  padding: .05rem .35rem; border-radius: 6px; line-height: 1;
}
.files-badge-btn:hover { background: var(--panel-2); }

/* --- Files: extracted-text viewer modal (§10) --- */
.files-modal-overlay {
  position: fixed; inset: 0; z-index: 50; background: var(--scrim);
  display: flex; align-items: flex-start; justify-content: center; padding: 4rem 1rem;
  overflow-y: auto;
}
.files-modal {
  background: var(--panel); border: 1px solid var(--border); border-radius: 12px;
  width: 100%; max-width: 820px; max-height: 80vh; box-shadow: 0 12px 40px rgba(0,0,0,.5);
  display: flex; flex-direction: column; overflow: hidden;
}
.files-modal-header {
  display: flex; align-items: center; gap: .75rem;
  padding: .9rem 1.1rem; border-bottom: 1px solid var(--border);
}
.files-modal-title {
  margin: 0; font-size: .98rem; font-weight: 700; flex: 1; min-width: 0;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.files-modal-close {
  background: transparent; color: var(--muted); border: 0; border-radius: 6px;
  width: 1.8rem; height: 1.8rem; font-size: 1rem; cursor: pointer; flex: none;
}
.files-modal-close:hover { color: var(--fg); background: var(--panel-2); }
.files-modal-body { overflow-y: auto; padding: 1rem 1.1rem 1.2rem; min-height: 0; }
.files-modal-summary {
  margin: 0 0 .8rem; padding: .55rem .7rem; border-left: 3px solid var(--accent-2);
  background: var(--panel-2); border-radius: 0 8px 8px 0; color: var(--fg);
  font-size: .86rem; line-height: 1.45;
}
.files-modal-text {
  margin: 0; white-space: pre-wrap; word-break: break-word;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .82rem;
  line-height: 1.5; color: var(--fg);
}
.files-modal-note { margin: .8rem 0 0; color: var(--muted); font-size: .78rem; font-style: italic; }

/* --- Skills panel (SOUL §23 manager; also hosts Profiles) — shared `.pane-*`
   chrome + the skills-only origin tag --- */
.skills-tag {
  font-size: .65rem; color: var(--accent); background: var(--panel);
  border: 1px solid var(--accent-2); border-radius: 4px; padding: 0 .3rem;
  text-transform: uppercase; letter-spacing: .4px; font-weight: 700;
}
.skills-form { flex: 1; display: flex; flex-direction: column; gap: .6rem; padding: 1rem; min-height: 0; overflow-y: auto; }
.skills-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .55rem .7rem; font: inherit;
}
.skills-input:focus { outline: none; border-color: var(--accent); }
.skills-input:disabled { color: var(--muted); }
.skills-input-name { font-size: 1.05rem; font-weight: 700; }
.skills-textarea {
  min-height: 9rem; resize: vertical; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .6rem .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .9rem; line-height: 1.5;
}
.skills-textarea:focus { outline: none; border-color: var(--accent); }
.skills-textarea-code { min-height: 7rem; }
.skills-code {
  display: flex; flex-direction: column; gap: .5rem;
  padding: .7rem; border: 1px dashed var(--border); border-radius: 8px;
}
.skills-check { display: flex; align-items: center; gap: .5rem; font-size: .88rem; }
.skills-check input { width: 1rem; height: 1rem; flex: none; }
.skills-form-error { color: var(--err-fg); font-size: .85rem; }
.skills-form-actions { display: flex; gap: .5rem; }
/* Skills read view — the rendered skill card shown before editing. */
.skills-view {
  flex: 1; min-width: 0; display: flex; flex-direction: column; gap: .9rem;
  padding: 1rem; min-height: 0; overflow-y: auto;
}
.skills-view-header { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
.skills-view-name { margin: 0; font-size: 1.2rem; font-weight: 700; word-break: break-word; }
.skills-view-desc { margin: 0; color: var(--muted); font-size: .9rem; line-height: 1.45; }
.skills-section { display: flex; flex-direction: column; gap: .4rem; }
.skills-section-label {
  display: flex; align-items: center; gap: .5rem; font-size: .72rem; font-weight: 700;
  color: var(--muted); text-transform: uppercase; letter-spacing: .6px;
}
.skills-view-muted { margin: 0; color: var(--muted); font-size: .85rem; font-style: italic; }
.skills-view-chips { display: flex; flex-wrap: wrap; gap: .35rem; }
.skills-chip {
  display: inline-flex; align-items: center; padding: .15rem .55rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 999px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .78rem;
}
.skills-view-md { min-height: 0; }
.skills-code-badge {
  padding: .05rem .4rem; background: var(--accent-2); color: var(--on-accent); border-radius: 5px;
  font-size: .68rem; font-weight: 700; text-transform: none; letter-spacing: .3px;
}
.skills-code-entry {
  color: var(--muted); font-size: .72rem; text-transform: none; letter-spacing: 0;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.skills-codeblock {
  margin: 0; background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
  padding: .7rem .8rem; overflow-x: auto; white-space: pre;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; line-height: 1.5;
}
.skills-codeblock code { font-family: inherit; }

/* --- MCP Endpoints panel (SOUL §30) — a two-pane manager mirroring Skills: a
   searchable endpoint list + a read-then-edit detail pane that also surfaces the
   connect URL and a mint-a-share-link action. Reuses the shared `.pane-*`
   chrome, `.pf-*` form fields, `.mcp-url*` URL row, and `.list-drawer*` mobile
   drawer; endpoint names run long, so the list pane is wider. --- */
.mcpe-list { width: 300px; }
.mcpe-tag {
  font-size: .65rem; color: var(--muted); background: var(--panel);
  border: 1px solid var(--border); border-radius: 4px; padding: 0 .3rem;
  text-transform: uppercase; letter-spacing: .4px; font-weight: 700;
}
.mcpe-view, .mcpe-form {
  flex: 1; min-width: 0; display: flex; flex-direction: column;
  padding: 1rem; min-height: 0; overflow-y: auto;
}
.mcpe-view { gap: .9rem; }
.mcpe-form { gap: .6rem; }
.mcpe-view-header { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
.mcpe-view-name {
  margin: 0; font-size: 1.2rem; font-weight: 700; word-break: break-word;
  display: flex; align-items: center; gap: .5rem;
}
.mcpe-view-desc { margin: 0; color: var(--muted); font-size: .9rem; line-height: 1.45; }
.mcpe-status-badge {
  font-size: .62rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px;
  border-radius: 999px; padding: .1rem .5rem; border: 1px solid var(--border);
}
.mcpe-status-on { color: var(--accent); border-color: var(--accent-2); }
.mcpe-status-off { color: var(--muted); }
.mcpe-section { display: flex; flex-direction: column; gap: .4rem; }
.mcpe-section-label {
  font-size: .72rem; font-weight: 700; color: var(--muted);
  text-transform: uppercase; letter-spacing: .6px;
}
.mcpe-muted { margin: 0; color: var(--muted); font-size: .8rem; line-height: 1.4; }
.mcpe-chips { display: flex; flex-wrap: wrap; gap: .35rem; }
.mcpe-chip {
  display: inline-flex; align-items: center; padding: .15rem .55rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 999px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .78rem;
}
.mcpe-notice {
  font-size: .8rem; line-height: 1.45; color: var(--fg); display: flex; flex-direction: column; gap: .4rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px; padding: .55rem .7rem;
}
.mcpe-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .55rem .7rem; font: inherit;
}
.mcpe-input:focus { outline: none; border-color: var(--accent); }
.mcpe-input:disabled { color: var(--muted); }
.mcpe-input-name { font-size: 1.05rem; font-weight: 700; }
.mcpe-check { display: flex; align-items: center; gap: .5rem; font-size: .88rem; }
.mcpe-check input { width: 1rem; height: 1rem; flex: none; }
.mcpe-textarea {
  min-height: 14rem; resize: vertical; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .6rem .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; line-height: 1.5;
}
.mcpe-textarea:focus { outline: none; border-color: var(--accent); }
.mcpe-codeblock {
  margin: 0; background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
  padding: .7rem .8rem; overflow-x: auto; white-space: pre;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .82rem; line-height: 1.5;
}
.mcpe-codeblock code { font-family: inherit; }
.mcpe-form-error { color: var(--err-fg); font-size: .85rem; }
.mcpe-form-actions { display: flex; gap: .5rem; flex-wrap: wrap; }

/* --- Profiles panel: labeled pickers (model / tools / skills / grant) --- */
.pf-group-title {
  font-size: .72rem; font-weight: 700; color: var(--accent); text-transform: uppercase;
  letter-spacing: .6px; margin: .7rem 0 -.1rem; padding-bottom: .25rem;
  border-bottom: 1px solid var(--border);
}
.pf-group-title:first-child { margin-top: 0; }
.pf-field { display: flex; flex-direction: column; gap: .25rem; }
.pf-label { font-size: .82rem; font-weight: 600; color: var(--fg); }
.pf-help { font-size: .72rem; color: var(--muted); line-height: 1.35; }
.pf-select { cursor: pointer; }
.pf-empty {
  font-size: .8rem; color: var(--muted); font-style: italic;
  padding: .4rem .1rem;
}
/* Checklist: a scroll-bounded grid of checkboxes over a catalog. */
.pf-checklist {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(13rem, 1fr)); gap: .15rem .6rem;
  max-height: 12rem; overflow-y: auto; padding: .5rem .6rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.pf-check { display: flex; align-items: flex-start; gap: .45rem; padding: .2rem .1rem; cursor: pointer; }
.pf-check:hover .pf-check-name { color: var(--accent); }
.pf-check-box { margin-top: .15rem; accent-color: var(--accent-2); cursor: pointer; flex: none; }
.pf-check-text { display: flex; flex-direction: column; gap: 0; min-width: 0; }
.pf-check-name { font-size: .85rem; word-break: break-word; }
.pf-check-hint {
  font-size: .7rem; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
/* Chips: removable tags + an inline add input (channels, tools fallback). */
.pf-chips {
  display: flex; flex-wrap: wrap; align-items: center; gap: .35rem;
  padding: .4rem .5rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.pf-chip {
  display: inline-flex; align-items: center; gap: .3rem; font-size: .8rem;
  background: var(--user); border: 1px solid var(--accent-2); border-radius: 999px; padding: .1rem .25rem .1rem .6rem;
}
.pf-chip-x {
  display: inline-flex; align-items: center; justify-content: center; width: 1.1rem; height: 1.1rem;
  background: transparent; color: var(--muted); border: 0; border-radius: 50%;
  font-size: .7rem; line-height: 1; cursor: pointer;
}
.pf-chip-x:hover:not(:disabled) { color: var(--err-fg); background: rgba(0,0,0,.25); }
.pf-chip-input {
  flex: 1; min-width: 9rem; background: transparent; color: var(--fg); border: 0;
  padding: .2rem .1rem; font: inherit; font-size: .85rem;
}
.pf-chip-input:focus { outline: none; }

/* Autocomplete combobox: the shared model/voice picker (SOUL §12) — a text input
   that filters a catalog as you type and floats a clickable suggestion list. The
   list is teleported to <body> with `position: fixed` (coords measured from the
   input, supplied inline) so it escapes the `overflow` clip of whichever scroll
   panel hosts the picker; z-index sits above the settings overlay (50). */
.ac-wrap { position: relative; }
.ac-wrap > input { width: 100%; }
.ac-list {
  position: fixed; z-index: 60;
  margin: 0; padding: .25rem; list-style: none; max-height: 14rem; overflow-y: auto;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
  box-shadow: 0 10px 28px rgba(0,0,0,.5);
}
.ac-item {
  padding: .4rem .55rem; border-radius: 6px; font-size: .85rem; cursor: pointer;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.ac-item:hover, .ac-item-active { background: var(--user); }
.ac-item-current { color: var(--accent); }

/* --- Automations panel (SOUL §11 builder) — shared `.pane-*` chrome + the
   automations-only enabled pill --- */
.auto-pill {
  font-size: .62rem; border-radius: 4px; padding: 0 .3rem; text-transform: uppercase;
  letter-spacing: .4px; font-weight: 700;
}
.auto-pill-on { color: #b9f6ca; background: #14361f; border: 1px solid #1f5c33; }
.auto-pill-off { color: var(--muted); background: var(--panel); border: 1px solid var(--border); }
.auto-form { flex: 1; display: flex; flex-direction: column; gap: .5rem; padding: 1rem; min-height: 0; overflow-y: auto; }
.auto-form-row { display: flex; gap: .6rem; align-items: center; flex-wrap: wrap; }
.auto-mode { display: flex; gap: .35rem; align-items: center; flex-wrap: wrap; }
.auto-mode-btn {
  background: var(--panel-2); color: var(--muted); border: 1px solid var(--border);
  border-radius: 7px; padding: .3rem .75rem; font: inherit; font-size: .82rem;
  font-weight: 600; cursor: pointer;
}
.auto-mode-btn:hover:not(:disabled) { border-color: var(--accent); }
.auto-mode-btn:disabled { cursor: not-allowed; }
.auto-mode-active { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.auto-mode-hint { font-size: .76rem; color: var(--muted); margin-left: .3rem; }
.auto-flow-wrap {
  display: flex; min-height: 26rem; height: calc(100vh - 16rem); border: 1px solid var(--border);
  border-radius: 8px; overflow: hidden;
}
.auto-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .55rem .7rem; font: inherit;
}
.auto-input:focus { outline: none; border-color: var(--accent); }
.auto-input:disabled { color: var(--muted); }
.auto-input-name { flex: 1; min-width: 12rem; font-size: 1.05rem; font-weight: 700; }
.auto-check { display: flex; align-items: center; gap: .35rem; font-size: .85rem; color: var(--muted); }
.auto-field-label { font-size: .72rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; margin-top: .3rem; }
.auto-textarea {
  min-height: 7rem; resize: vertical; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .6rem .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; line-height: 1.5;
}
.auto-textarea:focus { outline: none; border-color: var(--accent); }
.auto-textarea-sm { min-height: 3.5rem; }
.auto-textarea-json { min-height: 18rem; white-space: pre; overflow-wrap: normal; overflow-x: auto; }
.auto-form-error { color: var(--err-fg); font-size: .85rem; }
.auto-form-actions { display: flex; gap: .5rem; margin-top: .2rem; }
.auto-notice { font-size: .82rem; }
.auto-notice-ok { color: var(--ok-fg); }
.auto-notice-err { color: var(--err-fg); }
.auto-fire {
  border: 1px solid var(--border); border-radius: 8px; background: var(--panel);
  padding: .55rem .65rem; display: flex; flex-direction: column; gap: .4rem; margin-top: .3rem;
}
.auto-fire-head { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; }
.auto-fire-title { font-size: .8rem; font-weight: 700; color: var(--accent); }
.auto-tb {
  border: 1px solid var(--border); border-radius: 8px; background: var(--panel);
  padding: .55rem .65rem; display: flex; flex-direction: column; gap: .5rem; margin-top: .3rem;
}
.auto-tb-head { display: flex; gap: .5rem; align-items: center; flex-wrap: wrap; }
.auto-tb-title { font-size: .8rem; font-weight: 700; color: var(--accent); }
.auto-tb-kind { flex: 0 0 auto; min-width: 11rem; }
.auto-tb-fields { display: grid; grid-template-columns: repeat(auto-fit, minmax(13rem, 1fr)); gap: .5rem; }
.auto-tb-field { display: flex; flex-direction: column; gap: .2rem; }
.auto-tb-flabel { font-size: .72rem; color: var(--muted); }
.auto-runs { margin-top: .8rem; border-top: 1px solid var(--border); padding-top: .6rem; }
.auto-runs-title { margin: 0 0 .4rem; font-size: .78rem; font-weight: 700; color: var(--accent); text-transform: uppercase; letter-spacing: .5px; }
.auto-run-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .3rem; }
.auto-run {
  display: flex; gap: .6rem; align-items: baseline; flex-wrap: wrap;
  padding: .4rem .55rem; background: var(--panel); border: 1px solid var(--border); border-radius: 8px;
}
.auto-run-badge {
  font-size: .65rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px;
  border-radius: 4px; padding: 0 .35rem; flex-shrink: 0;
}
.auto-run-ok { color: #b9f6ca; background: #14361f; }
.auto-run-fail { color: var(--err-fg); background: #4a1622; }
.auto-run-running { color: #cfe0ff; background: #1c2c4a; }
.auto-run-other { color: var(--muted); background: var(--panel-2); }
.auto-run-when { font-size: .8rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.auto-run-kind {
  font-size: .7rem; color: var(--fg); background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 5px; padding: .05rem .4rem;
}
.auto-run-err { font-size: .78rem; color: var(--err-fg); word-break: break-word; }
.auto-run {
  width: 100%; text-align: left; background: var(--panel); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; cursor: pointer; font: inherit;
}
.auto-run:hover { border-color: var(--accent); }
.auto-run-selected { border-color: var(--accent-2); background: var(--panel-2); }
.auto-steps {
  margin-top: .4rem; padding: .5rem; background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 8px;
}
.auto-step-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.auto-step {
  display: flex; flex-direction: column; gap: .25rem;
  padding: .4rem .5rem; background: var(--panel); border: 1px solid var(--border); border-radius: 7px;
}
.auto-step-head { display: flex; flex-wrap: wrap; gap: .5rem; align-items: baseline; }
.auto-step-ord { font-size: .72rem; font-weight: 700; color: var(--muted); font-variant-numeric: tabular-nums; }
.auto-step-badge {
  font-size: .62rem; font-weight: 700; text-transform: uppercase; letter-spacing: .3px;
  border-radius: 4px; padding: .05rem .35rem;
}
.auto-step-kind {
  font-size: .7rem; color: var(--fg); background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 5px; padding: .05rem .4rem;
}
/* Per-agent-run cost chip + truncation badge in the run-step header (§19). */
.auto-step-cost {
  font-size: .7rem; color: var(--muted); font-variant-numeric: tabular-nums;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.auto-step-cap {
  font-size: .68rem; color: var(--warn-fg); background: var(--warn-bg); border: 1px solid var(--warn-border);
  border-radius: 5px; padding: .05rem .4rem; text-transform: uppercase; letter-spacing: .3px;
}
.auto-step-out {
  margin: 0; white-space: pre-wrap; word-break: break-word;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .76rem;
  color: var(--muted); background: var(--bg); border: 1px solid var(--border);
  border-radius: 6px; padding: .35rem .5rem; max-height: 12rem; overflow: auto;
}
.auto-step-err { font-size: .78rem; color: var(--err-fg); word-break: break-word; }

/* --- Grants panel (SOUL §19 capability-grant builder) — shared `.pane-*`
   chrome; only the editor form below is grants-specific --- */
.grant-form { flex: 1; display: flex; flex-direction: column; gap: .5rem; padding: 1rem; min-height: 0; overflow-y: auto; }
.grant-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .55rem .7rem; font: inherit;
}
.grant-input:focus { outline: none; border-color: var(--accent); }
.grant-input:disabled { color: var(--muted); }
.grant-input-name { font-size: 1.05rem; font-weight: 700; }
.grant-field-label { font-size: .72rem; color: var(--muted); text-transform: uppercase; letter-spacing: .5px; margin-top: .3rem; }
.grant-textarea {
  min-height: 7rem; resize: vertical; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .6rem .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; line-height: 1.5;
}
.grant-textarea:focus { outline: none; border-color: var(--accent); }
.grant-textarea-sm { min-height: 4rem; }
.grant-form-error { color: var(--err-fg); font-size: .85rem; }
.grant-form-actions { display: flex; gap: .5rem; margin-top: .2rem; }

/* --- Grants: visual capability builder + constraints form --- */
.cap-rows { display: flex; flex-direction: column; gap: .55rem; }
.cap-row-wrap {
  display: flex; flex-direction: column; gap: .2rem;
  padding: .5rem .55rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.cap-row { display: flex; gap: .4rem; align-items: center; flex-wrap: wrap; }
.cap-action { flex: 0 0 auto; min-width: 6.5rem; cursor: pointer; }
.cap-domain { flex: 1 1 9rem; min-width: 8rem; }
.cap-sel { flex: 1 1 9rem; min-width: 8rem; }
.cap-cons { margin-top: .1rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .82rem; }
.cap-icon {
  flex: none; background: var(--panel); color: var(--muted); border: 1px solid var(--border);
  border-radius: 6px; padding: .35rem .5rem; font-size: .85rem; line-height: 1; cursor: pointer;
}
.cap-icon:hover:not(:disabled) { color: var(--fg); border-color: var(--accent); }
.cap-icon:disabled { opacity: .5; cursor: not-allowed; }
.cap-icon-del:hover:not(:disabled) { color: var(--err-fg); border-color: var(--err-border); background: var(--err-bg); }
.cap-preview { font-size: .74rem; color: var(--accent); padding-left: .1rem; }
.cap-add {
  align-self: flex-start; background: transparent; color: var(--muted);
  border: 1px dashed var(--border); border-radius: 8px; padding: .4rem .8rem;
  font: inherit; font-size: .82rem; cursor: pointer; margin-top: -.1rem;
}
.cap-add:hover:not(:disabled) { border-color: var(--accent); color: var(--fg); }
.cap-add:disabled { opacity: .5; cursor: not-allowed; }
.cap-constraints {
  display: flex; flex-direction: column; gap: .5rem;
  padding: .6rem .7rem; background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
}
.cap-check { display: flex; align-items: center; gap: .45rem; font-size: .85rem; cursor: pointer; }
.cap-check input { accent-color: var(--accent-2); cursor: pointer; }
.cap-cap-row { display: flex; gap: .8rem; flex-wrap: wrap; }
.cap-num { display: flex; flex-direction: column; gap: .2rem; flex: 1 1 9rem; min-width: 8rem; }
.cap-num-label { font-size: .72rem; color: var(--muted); }
.cap-num .grant-input { width: 100%; }
.cap-other { margin-top: .1rem; }
.cap-other > summary {
  cursor: pointer; user-select: none; font-size: .76rem; color: var(--muted);
}
.cap-other > summary:hover { color: var(--fg); }
.cap-other > textarea { margin-top: .4rem; }

/* --- Conversations panel (history browser) — shared `.pane-*` chrome +
   transcript search --- */
.conv-search { display: flex; gap: .3rem; padding: .4rem .5rem; border-bottom: 1px solid var(--border); }
.conv-search-input {
  flex: 1; min-width: 0; padding: .35rem .5rem; font: inherit; font-size: .82rem;
  background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px;
}
.conv-search-input:focus { outline: none; border-color: var(--accent); }
.conv-search-clear {
  flex-shrink: 0; background: transparent; color: var(--muted); border: 1px solid var(--border);
  border-radius: 6px; padding: 0 .5rem; cursor: pointer; font-size: .8rem;
}
.conv-search-clear:hover { color: var(--fg); background: var(--panel-2); }
.conv-hit-head { display: flex; align-items: baseline; gap: .4rem; justify-content: space-between; }
.conv-hit-role {
  flex-shrink: 0; font-size: .6rem; color: var(--muted); text-transform: uppercase; letter-spacing: .4px;
}
.conv-hit-snippet {
  font-size: .78rem; color: var(--muted); word-break: break-word;
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical; overflow: hidden;
}
.conv-item-origin {
  font-size: .65rem; color: var(--muted); text-transform: uppercase; letter-spacing: .4px;
}
.conv-transcript { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.conv-transcript-head {
  display: flex; justify-content: flex-end; padding: .5rem .8rem;
  border-bottom: 1px solid var(--border);
}
.conv-resume { color: var(--accent); border-color: var(--accent); }
.conv-messages { flex: 1; min-height: 0; overflow-y: auto; padding: 1rem; display: flex; flex-direction: column; gap: .6rem; }
.conv-msg {
  display: flex; flex-direction: column; gap: .25rem; max-width: 72ch;
  padding: .55rem .7rem; border-radius: 10px; border: 1px solid var(--border);
  background: var(--panel); border-left: 3px solid var(--border);
}
.conv-msg-user { align-self: flex-end; background: var(--user); border-left-color: var(--accent-2); }
.conv-msg-assistant { border-left-color: var(--accent); }
.conv-msg-system { border-left-color: var(--muted); opacity: .85; }
.conv-msg-tool { border-left-color: #8a7; background: var(--panel-2); }
.conv-msg-head { display: flex; gap: .5rem; align-items: baseline; }
.conv-role { font-size: .65rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px; }
.conv-role-user { color: var(--accent); }
.conv-role-assistant { color: #b9f6ca; }
.conv-role-system { color: var(--muted); }
.conv-role-tool { color: var(--ok-fg); }
.conv-role-other { color: var(--muted); }
.conv-msg-when { font-size: .7rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.conv-msg-text { white-space: pre-wrap; word-break: break-word; font-size: .92rem; }
.conv-tools { list-style: none; margin: .1rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .15rem; }
.conv-tool {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .78rem;
  color: var(--muted); word-break: break-word;
}

/* --- Email panel (SOUL §28 read-only inbox) — three shared-chrome panes --- */
/* Mailboxes rail (pane 1 of 3): a narrow `.pane-list`; on mobile it becomes a
   left drawer toggled from the list header (same pattern as the chat sessions
   drawer). */
.email-mailboxes { width: 230px; }
/* Backdrop behind the mobile mailboxes drawer; inert on desktop. */
.email-mbx-scrim {
  display: none; position: fixed; inset: 0; z-index: 59; background: var(--scrim);
  border: 0; padding: 0; opacity: 0; pointer-events: none; transition: opacity .18s ease;
}
/* Message list (pane 2 of 3): a wide `.pane-list`. */
.email-list { width: 320px; }
.email-list-header { padding: .6rem .8rem; border-bottom: 1px solid var(--border); display: flex; flex-direction: column; gap: .5rem; }
.email-list-titlebar { display: flex; align-items: center; gap: .5rem; min-width: 0; }
.email-list-title {
  margin: 0; flex: 1; min-width: 0; font-size: .95rem; font-weight: 700;
  overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
/* Opens the mailboxes drawer; hidden on desktop where the rail is always shown. */
.email-mbx-toggle {
  display: none; flex-shrink: 0; font: inherit; font-size: .82rem; color: var(--fg);
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px;
  padding: .25rem .55rem; cursor: pointer;
}
.email-mbx-toggle:hover { border-color: var(--accent); }
/* Account tree (SOUL §28): mailboxes grouped per synced account (connection),
   each section expandable, every row carrying its unread-count pill. */
.email-accounts {
  flex: 1; min-height: 0; overflow-y: auto; padding: .5rem .5rem .7rem;
  display: flex; flex-direction: column; gap: .1rem;
}
.email-account { display: flex; flex-direction: column; }
.email-account-header {
  display: flex; align-items: center; gap: .35rem; width: 100%;
  background: transparent; color: var(--fg); border: none; border-radius: 6px;
  padding: .3rem .4rem; font: inherit; font-size: .8rem; font-weight: 700; cursor: pointer;
  text-align: left;
}
.email-account-header:hover { background: var(--panel-2); }
.email-account-arrow { flex-shrink: 0; width: .9rem; color: var(--muted); font-size: .7rem; }
.email-account-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.email-account-mailboxes { list-style: none; margin: 0; padding: 0 0 .15rem; display: flex; flex-direction: column; gap: .05rem; }
.email-mbx {
  display: flex; align-items: center; gap: .4rem; width: 100%;
  background: transparent; color: var(--fg); border: 1px solid transparent; border-radius: 6px;
  padding: .25rem .4rem .25rem 1.6rem; font: inherit; font-size: .8rem; cursor: pointer;
  text-align: left;
}
.email-mbx:hover { background: var(--panel-2); }
.email-mbx-active { background: var(--panel-2); border-color: var(--accent-2); }
.email-mbx-all { padding-left: .4rem; font-weight: 600; }
.email-mbx-name { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.email-badge {
  flex-shrink: 0; margin-left: auto; background: var(--accent-2); color: var(--on-accent);
  border-radius: 999px; padding: 0 .45rem; font-size: .68rem; font-weight: 700;
  line-height: 1.35; font-variant-numeric: tabular-nums;
}
.email-filters { display: flex; flex-wrap: wrap; gap: .35rem; align-items: center; }
.email-select, .email-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .35rem .5rem; font: inherit; font-size: .82rem;
}
.email-select:focus, .email-input:focus { outline: none; border-color: var(--accent); }
.email-input { flex: 1; min-width: 7rem; }
.email-btn {
  background: var(--accent-2); color: var(--on-accent); border: 1px solid var(--accent-2);
  border-radius: 7px; padding: .35rem .7rem; font: inherit; font-size: .82rem; font-weight: 600; cursor: pointer;
}
.email-btn:hover:not(:disabled) { background: var(--accent); }
.email-btn:disabled { background: var(--panel-2); color: var(--muted); cursor: not-allowed; border-color: var(--border); }
.email-status { color: var(--muted); padding: 1rem .8rem; font-size: .88rem; }
.email-error { color: var(--err-fg); }
.email-items { list-style: none; margin: 0; padding: .3rem; display: flex; flex-direction: column; gap: .15rem; }
.email-item {
  width: 100%; text-align: left; display: flex; flex-direction: column; gap: .2rem;
  background: transparent; color: var(--fg); border: 1px solid transparent;
  border-radius: 8px; padding: .5rem .6rem; cursor: pointer;
}
.email-item:hover { background: var(--panel-2); }
.email-item-active { background: var(--panel-2); border-color: var(--accent-2); }
.email-item-unread .email-item-subject { font-weight: 700; }
.email-item-unread { border-left: 3px solid var(--accent); }
.email-item-row1 { display: flex; align-items: baseline; justify-content: space-between; gap: .4rem; }
.email-item-subject { flex: 1; min-width: 0; font-size: .9rem; word-break: break-word; }
/* Cross-folder dedup badge (SOUL §29): a small pill on a message filed in more than
   one folder ("also in Archive" / "+2 folders"), tooltip carries the full list. */
.email-item-folders {
  flex-shrink: 0; font-size: .62rem; color: var(--muted); background: var(--panel);
  border: 1px solid var(--accent-2); border-radius: 999px; padding: 0 .4rem;
  white-space: nowrap; cursor: default;
}
.email-meta-folders { margin-left: .5rem; }
.email-attach { flex-shrink: 0; font-size: .78rem; }
.email-item-row2 { display: flex; align-items: baseline; justify-content: space-between; gap: .4rem; }
.email-item-from { font-size: .78rem; color: var(--muted); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.email-item-when { font-size: .72rem; color: var(--muted); flex-shrink: 0; font-variant-numeric: tabular-nums; }
.email-detail { flex: 1; min-width: 0; display: flex; flex-direction: column; min-height: 0; }
.email-detail-empty { margin: auto; color: var(--muted); text-align: center; }
/* Mobile-only Back bar atop the detail pane. Hidden on desktop, where the list
   and detail sit side by side; shown when the mobile layout swaps the list out
   for a full-screen message (see the narrow-viewport rules below). */
.email-detail-topbar {
  display: none; align-items: center; flex-shrink: 0;
  padding: .5rem .8rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.email-back-btn {
  font: inherit; font-size: .85rem; font-weight: 600; color: var(--fg); cursor: pointer;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 7px;
  padding: .35rem .7rem; min-height: 36px;
}
.email-back-btn:hover { border-color: var(--accent); }
.email-message { flex: 1; min-height: 0; overflow-y: auto; padding: 1rem 1.2rem; }
.email-subject-row { display: flex; align-items: flex-start; justify-content: space-between; gap: .8rem; margin: 0 0 .6rem; }
.email-subject { margin: 0; font-size: 1.15rem; font-weight: 700; word-break: break-word; }
.email-subject-row .email-subject { flex: 1; min-width: 0; }
/* Read/unread toggle — mutates catalerum's local copy only, never the provider. */
.email-mark-btn {
  flex-shrink: 0; font: inherit; font-size: .76rem; color: var(--fg); cursor: pointer;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px;
  padding: .25rem .55rem;
}
.email-mark-btn:hover:not(:disabled) { border-color: var(--accent); }
.email-mark-btn:disabled { color: var(--muted); cursor: not-allowed; }
.email-meta {
  display: flex; flex-direction: column; gap: .2rem; margin-bottom: .9rem;
  padding-bottom: .7rem; border-bottom: 1px solid var(--border);
}
.email-meta-row { display: flex; gap: .6rem; font-size: .85rem; }
.email-meta-k { flex-shrink: 0; width: 4rem; color: var(--muted); text-transform: uppercase; font-size: .68rem; letter-spacing: .5px; padding-top: .15rem; }
.email-meta-v { color: var(--fg); word-break: break-word; }
.email-body {
  white-space: pre-wrap; word-break: break-word; margin: 0; font: inherit;
  font-size: .92rem; line-height: 1.55; color: var(--fg);
}
.email-empty { color: var(--muted); font-style: italic; }
/* Sanitized HTML mail renders inside a fully sandboxed iframe (see
   email.rs::render_body). The white canvas is intentional and not themed —
   HTML mail is authored against a light background. User-resizable. */
.email-html-frame {
  width: 100%; height: 60vh; min-height: 20rem; resize: vertical; overflow: auto;
  border: 1px solid var(--border); border-radius: 8px; background: #fff;
}
.email-remote-bar {
  display: flex; align-items: center; gap: .6rem; margin-bottom: .5rem;
  font-size: .78rem; color: var(--muted);
}
.email-remote-btn {
  font: inherit; font-size: .74rem; color: var(--fg); cursor: pointer;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px;
  padding: .15rem .5rem;
}
.email-remote-btn:hover { border-color: var(--accent); }
.email-attachments { padding: 0 1.2rem .7rem; display: flex; flex-direction: column; gap: .5rem; }
.email-attachments-head { font-size: .68rem; text-transform: uppercase; letter-spacing: .5px; color: var(--muted); }
.email-attach-list { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: .5rem; }
.email-attach-item { display: inline-flex; }
.email-attach-btn {
  display: inline-flex; align-items: center; gap: .4rem; max-width: 22rem;
  font: inherit; font-size: .82rem; color: var(--fg); cursor: pointer;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px; padding: .25rem .55rem;
}
.email-attach-btn:hover { border-color: var(--accent); }
.email-attach-eml { align-self: flex-start; }
.email-attach-icon { opacity: .8; flex-shrink: 0; }
.email-attach-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.email-attach-size { color: var(--muted); flex-shrink: 0; font-variant-numeric: tabular-nums; }

/* --- Fetch panel (SOUL §27 web-fetch utility) --- */
.fetch-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.fetch-header {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem;
  padding: .8rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.fetch-header-titles { display: flex; flex-direction: column; gap: .15rem; flex-shrink: 0; }
.fetch-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.fetch-subtitle { color: var(--muted); font-size: .82rem; }
.fetch-form { display: flex; flex-direction: column; gap: .5rem; flex: 1; max-width: 720px; }
.fetch-url {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .5rem .65rem; font: inherit; font-size: .9rem; width: 100%;
}
.fetch-url:focus { outline: none; border-color: var(--accent); }
.fetch-opts { display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; }
.fetch-select {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .35rem .5rem; font: inherit; font-size: .82rem;
}
.fetch-select:focus { outline: none; border-color: var(--accent); }
.fetch-check { display: flex; align-items: center; gap: .35rem; font-size: .82rem; color: var(--muted); }
.fetch-btn {
  background: var(--accent-2); color: var(--on-accent); border: 1px solid var(--accent-2);
  border-radius: 7px; padding: .4rem .9rem; font: inherit; font-size: .85rem; font-weight: 600; cursor: pointer;
}
.fetch-btn:hover:not(:disabled) { background: var(--accent); }
.fetch-btn:disabled { background: var(--panel-2); color: var(--muted); cursor: not-allowed; border-color: var(--border); }
.fetch-body { flex: 1; min-height: 0; overflow-y: auto; padding: 1rem; }
.fetch-status { color: var(--muted); padding: 1rem 0; font-size: .9rem; }
.fetch-error { color: var(--err-fg); }
.fetch-result { max-width: 820px; }
.fetch-result-title { margin: 0 0 .5rem; font-size: 1.1rem; font-weight: 700; word-break: break-word; }
.fetch-result-meta {
  display: flex; flex-wrap: wrap; gap: .5rem; align-items: center; margin-bottom: .8rem;
  padding-bottom: .7rem; border-bottom: 1px solid var(--border); font-size: .82rem;
}
.fetch-status-pill {
  font-size: .68rem; font-weight: 700; border-radius: 4px; padding: .05rem .4rem;
  text-transform: uppercase; letter-spacing: .4px;
}
.fetch-status-ok { color: #b9f6ca; background: #14361f; }
.fetch-status-redir { color: #cfe0ff; background: #1c2c4a; }
.fetch-status-bad { color: var(--err-fg); background: #4a1622; }
.fetch-result-url { color: var(--accent); word-break: break-all; text-decoration: none; }
.fetch-result-url:hover { text-decoration: underline; }
.fetch-result-ctype { color: var(--muted); }
.fetch-result-savings {
  color: var(--fg); background: var(--panel-2); border: 1px solid var(--border);
  border-radius: 5px; padding: .05rem .4rem; font-variant-numeric: tabular-nums;
}
.fetch-content {
  white-space: pre-wrap; word-break: break-word; margin: 0; font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  font-size: .85rem; line-height: 1.55; color: var(--fg);
  background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: .8rem;
}

/* --- Tasks panel (SOUL §24 Kanban board) --- */
.task-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.task-header {
  display: flex; align-items: center; justify-content: space-between; gap: 1rem; flex-wrap: wrap;
  padding: .8rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.task-header-left { display: flex; align-items: center; gap: .8rem; }
.task-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.task-board-select, .task-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .6rem; font: inherit; font-size: .85rem;
}
.task-board-select:focus, .task-input:focus { outline: none; border-color: var(--accent); }
.task-board-del {
  background: transparent; color: var(--muted); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .55rem; font-size: .85rem; line-height: 1; cursor: pointer;
}
.task-board-del:hover:not(:disabled) { color: var(--err-fg); border-color: var(--err-border); background: var(--err-bg); }
.task-board-del:disabled { opacity: .5; cursor: not-allowed; }
.task-board-edit {
  background: transparent; color: var(--muted); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .55rem; font-size: .85rem; line-height: 1; cursor: pointer;
}
.task-board-edit:hover:not(:disabled) { color: var(--fg); border-color: var(--accent); }
.task-board-edit:disabled { opacity: .5; cursor: not-allowed; }
.task-rename-form {
  display: flex; gap: .5rem; align-items: center; margin: .7rem 1rem 0;
  padding: .5rem .7rem; background: var(--panel); border: 1px solid var(--accent); border-radius: 8px;
}
.task-newboard { display: flex; gap: .5rem; align-items: center; }
.task-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .7rem; font: inherit; font-size: .82rem; font-weight: 600; cursor: pointer;
}
.task-btn:hover:not(:disabled) { border-color: var(--accent); }
.task-btn:disabled { color: var(--muted); cursor: not-allowed; }
.task-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.task-btn-primary:hover:not(:disabled) { background: var(--accent); }
.task-banner { margin: .7rem 1rem 0; padding: .5rem .7rem; font-size: .85rem; border-radius: 8px; }
.task-error { color: var(--err-fg); background: var(--err); border: 1px solid var(--err-border); }
.task-status { color: var(--muted); padding: 1.2rem 1rem; font-size: .9rem; }
.task-body { flex: 1; min-height: 0; overflow: auto; padding: 1rem; }
.task-board { display: flex; gap: .8rem; align-items: flex-start; min-height: 0; }
.task-col {
  flex: 0 0 260px; display: flex; flex-direction: column; gap: .5rem;
  background: var(--panel); border: 1px solid var(--border); border-radius: 10px; padding: .6rem;
  max-height: 100%;
}
.task-filter { min-width: 170px; }
.task-col-head { display: flex; align-items: center; gap: .35rem; padding: 0 .2rem; min-height: 1.6rem; }
.task-col-name {
  font-size: .8rem; font-weight: 700; text-transform: uppercase; letter-spacing: .5px; color: var(--accent);
  min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.task-col-count {
  margin-left: auto; font-size: .7rem; color: var(--muted); background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 10px; padding: 0 .45rem; min-width: 1.4rem; text-align: center;
}
/* Column rename/delete tools: revealed on hover or keyboard focus. */
.task-col-tools { display: flex; gap: .25rem; visibility: hidden; }
.task-col:hover .task-col-tools, .task-col:focus-within .task-col-tools { visibility: visible; }
.task-col-rename { flex: 1; min-width: 0; padding: .2rem .4rem; font-size: .8rem; }
.task-col-ghost { background: transparent; border-style: dashed; }
.task-col-cards { display: flex; flex-direction: column; gap: .45rem; overflow-y: auto; }
.task-card {
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 8px;
  padding: .55rem .6rem; display: flex; flex-direction: column; gap: .35rem;
}
.task-card:hover { border-color: var(--accent-2); }
.task-card:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
.task-card-top { display: flex; align-items: flex-start; justify-content: space-between; gap: .4rem; }
.task-card-title { font-size: .88rem; font-weight: 600; word-break: break-word; }
.task-card-preview { font-size: .76rem; color: var(--muted); word-break: break-word; }
.task-badge {
  flex-shrink: 0; font-size: .6rem; font-weight: 700; text-transform: uppercase; letter-spacing: .3px;
  border-radius: 4px; padding: .05rem .35rem;
}
.task-badge-open { color: var(--muted); background: var(--panel); border: 1px solid var(--border); }
.task-badge-progress { color: var(--accent); background: var(--user); border: 1px solid var(--accent-2); }
.task-badge-blocked { color: var(--err-fg); background: var(--err-bg); border: 1px solid var(--err-border); }
.task-badge-done { color: var(--ok-fg); background: var(--ok-bg); border: 1px solid var(--ok-border); }
.task-status-select {
  flex: 1; background: var(--panel); color: var(--fg); border: 1px solid var(--border);
  border-radius: 6px; padding: .2rem .3rem; font: inherit; font-size: .75rem;
}
.task-status-select:focus { outline: none; border-color: var(--accent); }
.task-add-btn {
  width: 100%; background: transparent; color: var(--muted); border: 1px dashed var(--border);
  border-radius: 8px; padding: .4rem; font: inherit; font-size: .8rem; cursor: pointer;
}
.task-add-btn:hover:not(:disabled) { border-color: var(--accent); color: var(--fg); }
.task-add-form { display: flex; flex-direction: column; gap: .35rem; }
.task-add-actions { display: flex; gap: .4rem; }
/* Drag-and-drop: dragging highlights the hovered column; hovering a card shows
   the insert-above line. */
.task-hint { font-size: .76rem; color: var(--muted); margin: 0 0 .6rem; }
.task-card { cursor: grab; }
.task-card:active { cursor: grabbing; }
.task-col-drop {
  border-color: var(--accent); background: var(--user);
  box-shadow: inset 0 0 0 1px var(--accent);
}
.task-card-over { box-shadow: 0 -3px 0 0 var(--accent); }
.task-assignee {
  align-self: flex-start; font-size: .68rem; color: var(--accent);
  background: var(--panel); border: 1px solid var(--accent-2); border-radius: 999px;
  padding: .05rem .45rem;
}
.task-card-controls { display: flex; align-items: center; gap: .3rem; }
.task-ctl-label {
  font-size: .62rem; color: var(--muted); text-transform: uppercase; letter-spacing: .4px; flex: none;
}
.task-icon {
  background: var(--panel); color: var(--muted); border: 1px solid var(--border);
  border-radius: 6px; padding: .15rem .45rem; font-size: .8rem; cursor: pointer; line-height: 1;
}
.task-icon:hover:not(:disabled) { color: var(--fg); border-color: var(--accent); }
.task-icon-del:hover:not(:disabled) { color: var(--err-fg); border-color: var(--err-border); background: var(--err-bg); }
.task-btn-danger { color: var(--err-fg); border-color: var(--err-border); }
.task-btn-danger:hover:not(:disabled) { background: var(--err-bg); border-color: var(--err-border); }
/* Task detail modal: view (rendered markdown) / edit (MarkdownField) modes. */
.task-modal-overlay {
  position: fixed; inset: 0; z-index: 50; background: var(--scrim);
  display: flex; align-items: flex-start; justify-content: center; padding: 4rem 1rem;
  overflow-y: auto;
}
.task-modal {
  background: var(--panel); border: 1px solid var(--border); border-radius: 12px;
  width: 100%; max-width: 760px; max-height: 82vh; box-shadow: 0 12px 40px rgba(0,0,0,.5);
  display: flex; flex-direction: column; overflow: hidden;
}
.task-modal-header {
  display: flex; align-items: center; gap: .75rem;
  padding: .9rem 1.1rem; border-bottom: 1px solid var(--border);
}
.task-modal-title {
  margin: 0; font-size: 1rem; font-weight: 700; flex: 1; min-width: 0; word-break: break-word;
}
.task-modal-title-input { flex: 1; min-width: 0; }
.task-modal-close {
  background: transparent; color: var(--muted); border: 0; border-radius: 6px;
  width: 1.8rem; height: 1.8rem; font-size: 1rem; cursor: pointer; flex: none;
}
.task-modal-close:hover { color: var(--fg); background: var(--panel-2); }
.task-modal-body {
  display: flex; flex-direction: column; gap: .8rem;
  overflow-y: auto; padding: .9rem 1.1rem; min-height: 0;
}
.task-modal-meta { display: flex; align-items: center; gap: .7rem; flex-wrap: wrap; }
.task-modal-field { display: flex; align-items: center; gap: .35rem; }
.task-modal-md { flex: none; min-height: 0; max-height: 48vh; }
.task-modal-empty { color: var(--muted); font-size: .85rem; font-style: italic; }
.task-modal-edit { display: flex; flex-direction: column; gap: .4rem; }
.task-modal-edit .notes-textarea { min-height: 11rem; }
.task-modal-edit .notes-preview { min-height: 11rem; max-height: 40vh; }
.task-modal-actions {
  display: flex; gap: .5rem; justify-content: flex-end;
  padding: .8rem 1.1rem; border-top: 1px solid var(--border);
}

/* --- Shared confirm / prompt dialog (replaces native window.confirm/prompt) --- */
.dlg-overlay {
  position: fixed; inset: 0; z-index: 60; background: var(--scrim);
  display: flex; align-items: center; justify-content: center; padding: 1.2rem;
  overflow-y: auto;
}
.dlg-modal {
  background: var(--panel); border: 1px solid var(--border); border-radius: 12px;
  width: 100%; max-width: 30rem; box-shadow: 0 12px 40px rgba(0,0,0,.5);
  display: flex; flex-direction: column; overflow: hidden;
}
.dlg-header { padding: 1rem 1.1rem .1rem; }
.dlg-title { margin: 0; font-size: 1rem; font-weight: 700; }
.dlg-body { padding: .5rem 1.1rem 1rem; display: flex; flex-direction: column; gap: .7rem; }
.dlg-message { margin: 0; color: var(--muted); font-size: .9rem; line-height: 1.5; white-space: pre-wrap; }
.dlg-input {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .5rem .6rem; font: inherit; font-size: .9rem; width: 100%;
}
.dlg-input:focus { outline: none; border-color: var(--accent); }
.dlg-actions {
  display: flex; gap: .5rem; justify-content: flex-end;
  padding: .8rem 1.1rem; border-top: 1px solid var(--border);
}
.dlg-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .45rem .8rem; font: inherit; font-size: .85rem; font-weight: 600; cursor: pointer;
}
.dlg-btn:hover:not(:disabled) { border-color: var(--accent); }
.dlg-btn:disabled { color: var(--muted); cursor: not-allowed; }
.dlg-btn-confirm { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.dlg-btn-confirm:hover:not(:disabled) { background: var(--accent); }
/* A destructive confirm mirrors the danger button trio (theme-token contrast). */
.dlg-btn-danger { background: var(--panel-2); color: var(--err-fg); border-color: var(--err-border); }
.dlg-btn-danger:hover:not(:disabled) { background: var(--err-bg); border-color: var(--err-border); }

/* --- Memory panel (SOUL §22 memories + profile) — a wide shared `.pane-list`
   whose rows are content cards, not nav rows --- */
.mem-list { width: 360px; }
.mem-new { display: flex; flex-direction: column; gap: .4rem; padding: .7rem .8rem; border-bottom: 1px solid var(--border); }
.mem-new-text {
  min-height: 3rem; resize: vertical; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .5rem .6rem; font: inherit; font-size: .88rem;
}
.mem-new-text:focus { outline: none; border-color: var(--accent); }
.mem-new-actions { display: flex; gap: .5rem; align-items: center; }
.mem-select {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .35rem .5rem; font: inherit; font-size: .82rem;
}
.mem-select:focus { outline: none; border-color: var(--accent); }
.mem-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .35rem .7rem; font: inherit; font-size: .8rem; font-weight: 600; cursor: pointer;
}
.mem-btn:hover:not(:disabled) { border-color: var(--accent); }
.mem-btn:disabled { color: var(--muted); cursor: not-allowed; }
.mem-btn-primary { background: var(--accent-2); color: var(--on-accent); border-color: var(--accent-2); }
.mem-btn-primary:hover:not(:disabled) { background: var(--accent); }
.mem-btn-danger { background: var(--err); color: var(--err-fg); border-color: var(--err-border); }
.mem-btn-danger:hover:not(:disabled) { border-color: var(--err-fg); }
.mem-items { list-style: none; margin: 0; padding: .5rem; display: flex; flex-direction: column; gap: .4rem; }
.mem-item {
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px;
  padding: .55rem .65rem; display: flex; flex-direction: column; gap: .3rem;
}
.mem-item-head { display: flex; align-items: baseline; gap: .5rem; }
.mem-scope { font-size: .6rem; font-weight: 700; text-transform: uppercase; letter-spacing: .3px; border-radius: 4px; padding: .05rem .35rem; }
.mem-scope-private { color: var(--accent); background: color-mix(in srgb, var(--accent) 15%, transparent); }
.mem-scope-shared { color: var(--ok-fg); background: var(--ok-bg); }
.mem-when { font-size: .7rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.mem-item-text { font-size: .9rem; white-space: pre-wrap; word-break: break-word; }
.mem-edit-text {
  min-height: 3rem; resize: vertical; background: var(--bg); color: var(--fg);
  border: 1px solid var(--accent-2); border-radius: 7px; padding: .45rem .6rem; font: inherit; font-size: .88rem;
}
.mem-edit-text:focus { outline: none; border-color: var(--accent); }
.mem-item-actions { display: flex; gap: .4rem; }
.mem-profile { flex: 1; min-width: 0; display: flex; flex-direction: column; padding: 1rem; min-height: 0; }
.mem-profile-head { display: flex; flex-direction: column; gap: .2rem; margin-bottom: .6rem; }
.mem-profile-hint { font-size: .78rem; color: var(--muted); }
.mem-profile-text {
  flex: 1; min-height: 12rem; resize: none; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .7rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .88rem; line-height: 1.5;
}
.mem-profile-text:focus { outline: none; border-color: var(--accent); }
.mem-profile-actions { display: flex; gap: .5rem; margin-top: .6rem; }

/* --- Graph panel (SOUL §6.3 explorer) --- */
.graph-panel { display: flex; flex-direction: column; flex: 1; min-height: 0; }
.graph-header {
  display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem;
  padding: .8rem 1rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.graph-header-titles { display: flex; flex-direction: column; gap: .15rem; flex-shrink: 0; }
.graph-title { margin: 0; font-size: 1.05rem; font-weight: 700; }
.graph-subtitle { color: var(--muted); font-size: .82rem; }
.graph-subtitle code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--accent); }
.graph-form { display: flex; flex-direction: column; gap: .5rem; flex: 1; max-width: 720px; }
.graph-cypher {
  min-height: 4.5rem; resize: vertical; background: var(--bg); color: var(--fg);
  border: 1px solid var(--border); border-radius: 8px; padding: .55rem .65rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; line-height: 1.5;
}
.graph-cypher:focus { outline: none; border-color: var(--accent); }
.graph-form-actions { display: flex; }
.graph-btn {
  background: var(--accent-2); color: var(--on-accent); border: 1px solid var(--accent-2);
  border-radius: 7px; padding: .4rem 1rem; font: inherit; font-size: .85rem; font-weight: 600; cursor: pointer;
}
.graph-btn:hover:not(:disabled) { background: var(--accent); }
.graph-btn:disabled { background: var(--panel-2); color: var(--muted); cursor: not-allowed; border-color: var(--border); }
.graph-body { flex: 1; min-height: 0; overflow: auto; padding: 1rem; }
.graph-status { color: var(--muted); padding: 1rem 0; font-size: .9rem; }
.graph-error { color: var(--err-fg); }
.graph-result-meta { color: var(--muted); font-size: .8rem; margin-bottom: .5rem; }
.graph-table-wrap { overflow-x: auto; border: 1px solid var(--border); border-radius: 8px; }
.graph-table { width: 100%; border-collapse: collapse; font-size: .85rem; }
.graph-table thead th {
  text-align: left; color: var(--accent); font-size: .72rem; font-weight: 700;
  text-transform: uppercase; letter-spacing: .5px; padding: .45rem .6rem;
  border-bottom: 1px solid var(--border); background: var(--panel); position: sticky; top: 0;
}
.graph-table tbody td {
  padding: .4rem .6rem; border-bottom: 1px solid var(--border); vertical-align: top;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .8rem;
  max-width: 32rem; overflow-wrap: anywhere;
}
.graph-table tbody tr:hover { background: var(--panel); }
.graph-cell-node {
  font: inherit; text-align: left; color: var(--accent); background: none;
  border: none; padding: 0; cursor: pointer; overflow-wrap: anywhere;
}
.graph-cell-node:hover { text-decoration: underline; }
.graph-detail {
  margin-top: .8rem; border: 1px solid var(--accent-2); border-radius: 8px;
  background: var(--panel); padding: .6rem .7rem;
}
.graph-detail-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: .4rem; }
.graph-detail-title { font-size: .8rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px; color: var(--muted); }
.graph-detail-fields { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .25rem; }
.graph-field {
  display: grid; grid-template-columns: minmax(6rem, 14rem) 1fr; gap: .6rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .8rem;
}
.graph-field-key { color: var(--accent); font-weight: 600; overflow-wrap: anywhere; }
.graph-field-val { overflow-wrap: anywhere; white-space: pre-wrap; }

/* --- Flow editor (SOUL §11 Phase C — visual node-graph canvas) --- */
.flow {
  display: flex; flex-direction: column; flex: 1; min-height: 0;
  --flow-trigger: #4fd1a1; --flow-action: #6ea8fe; --flow-code: #b692f6; --flow-condition: #f0c46c;
  --flow-agent: #22d3ee; --flow-classify: #f472b6; --flow-loop: #fb923c;
}
.flow-palette {
  display: flex; align-items: center; gap: .45rem; flex-wrap: wrap;
  padding: .55rem .7rem; border-bottom: 1px solid var(--border);
  background: linear-gradient(180deg, var(--panel), var(--panel-2));
}
.flow-palette-label {
  font-size: .72rem; font-weight: 700; text-transform: uppercase; letter-spacing: .5px;
  color: var(--muted); margin-right: .15rem;
}
.flow-palette-sep { width: 1px; height: 1.3rem; background: var(--border); margin: 0 .25rem; }
.flow-pal-btn {
  display: inline-flex; align-items: center; gap: .42rem;
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .35rem .7rem .35rem .58rem; font: inherit; font-size: .82rem; font-weight: 600;
  cursor: pointer; transition: border-color .12s, transform .08s, background .12s;
}
.flow-pal-btn::before {
  content: ""; width: .6rem; height: .6rem; border-radius: 50%;
  background: var(--dot, var(--accent)); box-shadow: 0 0 7px var(--dot, var(--accent));
}
.flow-pal-btn:hover { transform: translateY(-1px); background: var(--panel); border-color: var(--dot, var(--accent)); }
.flow-pal-trigger { --dot: var(--flow-trigger); }
.flow-pal-action { --dot: var(--flow-action); }
.flow-pal-code { --dot: var(--flow-code); }
.flow-pal-condition { --dot: var(--flow-condition); }
.flow-pal-agent { --dot: var(--flow-agent); }
.flow-pal-classifier { --dot: var(--flow-classify); }
.flow-pal-loop { --dot: var(--flow-loop); }
.flow-invalid {
  margin-left: auto; color: #ffb4a2; font-size: .76rem; font-weight: 600;
  background: rgba(90,34,48,.45); border: 1px solid #5a2230; border-radius: 999px; padding: .18rem .6rem;
}
/* Node-type semantic search (SOUL §11): describe a need, get ranked node types. */
.flow-node-search {
  position: relative;
  padding: .5rem .7rem; border-bottom: 1px solid var(--border);
  background: var(--panel-2);
}
.flow-node-search-bar { display: flex; align-items: center; gap: .45rem; flex-wrap: wrap; }
.flow-node-search-input {
  flex: 1; min-width: 14rem;
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .4rem .6rem; font: inherit; font-size: .82rem;
}
.flow-node-search-input:focus { outline: none; border-color: var(--accent); }
.flow-node-search-status { color: var(--muted); font-size: .76rem; }
.flow-node-search-err {
  margin-top: .4rem; color: #ffb4a2; font-size: .76rem;
  background: rgba(90,34,48,.4); border: 1px solid #5a2230; border-radius: 6px; padding: .3rem .55rem;
}
.flow-node-results {
  margin-top: .5rem; display: flex; flex-direction: column; gap: .3rem;
  max-height: 15rem; overflow-y: auto;
}
.flow-node-result {
  display: flex; align-items: flex-start; gap: .55rem; text-align: left;
  background: var(--panel); color: var(--fg); border: 1px solid var(--border);
  border-radius: 8px; padding: .45rem .6rem; font: inherit; cursor: pointer;
  transition: border-color .12s, background .12s, transform .08s;
}
.flow-node-result:hover { border-color: var(--accent); background: var(--panel-2); transform: translateY(-1px); }
.flow-node-result-badge {
  flex: none; margin-top: .05rem; font-size: .66rem; font-weight: 700; text-transform: uppercase;
  letter-spacing: .4px; color: #0b0f14; border-radius: 5px; padding: .12rem .4rem;
}
.flow-rb-trigger { background: var(--flow-trigger); }
.flow-rb-action { background: var(--flow-action); }
.flow-rb-code { background: var(--flow-code); }
.flow-rb-condition { background: var(--flow-condition); }
.flow-rb-for_each, .flow-rb-loop_end { background: var(--flow-loop); }
.flow-node-result-main { display: flex; flex-direction: column; gap: .1rem; min-width: 0; }
.flow-node-result-title { font-size: .82rem; font-weight: 600; }
.flow-node-result-summary { font-size: .74rem; color: var(--muted); }
.flow-main { display: flex; flex: 1; min-height: 0; }
.flow-canvas-wrap { flex: 1; min-width: 0; position: relative; overflow: hidden; }
.flow-canvas {
  width: 100%; height: 100%; display: block; background-color: var(--bg);
  background-image:
    radial-gradient(circle at 50% -10%, rgba(110,168,254,.07), transparent 55%),
    radial-gradient(var(--border) 1.1px, transparent 1.2px);
  background-size: auto, 24px 24px; touch-action: none; cursor: grab; user-select: none;
}
.flow-canvas:active { cursor: grabbing; }
/* Focusable (tabindex) so it can receive Delete/Backspace; suppress the ring —
   focus here is implicit from clicking, and a box around the whole canvas is noise. */
.flow-canvas:focus { outline: none; }
.flow-canvas:focus-visible { outline: none; }
.flow-zoom {
  position: absolute; bottom: .6rem; right: .6rem; display: flex; gap: .25rem;
  background: var(--panel); border: 1px solid var(--border); border-radius: 8px; padding: .2rem;
}
.flow-zoom-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border); border-radius: 6px;
  min-width: 1.7rem; height: 1.7rem; padding: 0 .3rem; font: inherit; font-size: .9rem; cursor: pointer;
}
.flow-zoom-btn:hover { border-color: var(--accent); }
.flow-zoom-pct { min-width: 3rem; font-size: .74rem; font-variant-numeric: tabular-nums; }
.flow-empty {
  position: absolute; inset: 0; display: flex; flex-direction: column;
  align-items: center; justify-content: center; gap: .5rem; padding: 1.5rem;
  pointer-events: none; text-align: center;
}
/* Wiring-gesture feedback (e.g. an edge into a non-collect trigger): a small
   dismissible banner floated over the canvas top edge. */
.flow-wire-hint {
  position: absolute; top: .6rem; left: 50%; transform: translateX(-50%);
  display: flex; align-items: center; gap: .5rem; max-width: min(34rem, 90%);
  background: var(--warn-bg); border: 1px solid var(--warn-border); color: var(--warn-fg);
  border-radius: 8px; padding: .4rem .65rem; font-size: .8rem; line-height: 1.35; z-index: 3;
}
.flow-wire-hint button {
  background: none; border: none; color: inherit; font: inherit; cursor: pointer;
  padding: 0 .1rem; font-size: .9rem;
}
.flow-empty-title { margin: 0; font-size: 1.05rem; font-weight: 700; color: var(--fg); }
.flow-empty-sub { margin: 0; max-width: 30rem; color: var(--muted); font-size: .88rem; line-height: 1.5; }
.flow-edge { fill: none; stroke: var(--accent-2); stroke-width: 2.2; cursor: pointer; transition: stroke .1s, stroke-width .1s; }
.flow-edge:hover { stroke: var(--accent); stroke-width: 3; }
.flow-edge-pending {
  stroke: var(--accent); stroke-width: 2.4; stroke-dasharray: 6 5; pointer-events: none;
  animation: flow-dash .5s linear infinite;
}
@keyframes flow-dash { to { stroke-dashoffset: -22; } }
.flow-arrow { fill: var(--accent-2); }
.flow-node { cursor: grab; }
.flow-node:hover .flow-node-box { filter: brightness(1.09) drop-shadow(0 3px 7px rgba(0,0,0,.5)); }
.flow-node-box {
  fill: var(--panel-2); stroke: var(--border); stroke-width: 1.5;
  filter: drop-shadow(0 3px 6px rgba(0,0,0,.45));
}
.flow-node-accent { fill: var(--accent); }
.flow-node-icon { font-size: 15px; pointer-events: none; }
.flow-node-title {
  fill: var(--fg); font-size: 13px; font-weight: 700; pointer-events: none;
  font-family: ui-sans-serif, system-ui, sans-serif;
}
.flow-node-id {
  fill: var(--muted); font-size: 10.5px; text-anchor: end; pointer-events: none;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.flow-node-sub {
  fill: var(--muted); font-size: 11px; pointer-events: none;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
/* per-kind colour identity (box border + accent bar + title) */
.flow-node-trigger .flow-node-box { stroke: var(--flow-trigger); }
.flow-node-trigger .flow-node-accent, .flow-node-trigger .flow-node-title { fill: var(--flow-trigger); }
.flow-node-action .flow-node-box { stroke: var(--flow-action); }
.flow-node-action .flow-node-accent, .flow-node-action .flow-node-title { fill: var(--flow-action); }
.flow-node-code .flow-node-box { stroke: var(--flow-code); }
.flow-node-code .flow-node-accent, .flow-node-code .flow-node-title { fill: var(--flow-code); }
.flow-node-condition .flow-node-box { stroke: var(--flow-condition); }
.flow-node-condition .flow-node-accent, .flow-node-condition .flow-node-title { fill: var(--flow-condition); }
.flow-node-loop .flow-node-box { stroke: var(--flow-loop); }
.flow-node-loop .flow-node-accent, .flow-node-loop .flow-node-title { fill: var(--flow-loop); }
.flow-node-agent .flow-node-box { stroke: var(--flow-agent); }
.flow-node-agent .flow-node-accent, .flow-node-agent .flow-node-title { fill: var(--flow-agent); }
.flow-node-classify .flow-node-box { stroke: var(--flow-classify); }
.flow-node-classify .flow-node-accent, .flow-node-classify .flow-node-title { fill: var(--flow-classify); }
.flow-node-selected .flow-node-box {
  stroke: var(--accent); stroke-width: 2.6;
  filter: drop-shadow(0 0 9px rgba(110,168,254,.5));
}
.flow-node-warn .flow-node-box { stroke: var(--flow-condition); stroke-dasharray: 5 4; }
.flow-node-warn-mark { fill: var(--flow-condition); font-size: 13px; pointer-events: none; }
.flow-port { fill: var(--panel); stroke: var(--accent); stroke-width: 2; cursor: crosshair; transition: fill .1s; }
.flow-port:hover { fill: var(--accent); }
.flow-port-in { stroke: var(--muted); }
.flow-port-in:hover { fill: var(--fg); stroke: var(--fg); }
.flow-port-true { stroke: var(--flow-trigger); }
.flow-port-true:hover { fill: var(--flow-trigger); }
.flow-port-false { stroke: #ff8b8b; }
.flow-port-false:hover { fill: #ff8b8b; }
.flow-port-label {
  fill: var(--muted); font-size: 9.5px; font-weight: 700; text-anchor: start; pointer-events: none;
  text-transform: uppercase; letter-spacing: .3px; font-family: ui-sans-serif, system-ui, sans-serif;
  /* Canvas-coloured halo: port labels sit outside the node where edges run, so
     without it a passing edge strikes the text through. */
  paint-order: stroke; stroke: var(--bg); stroke-width: 3px; stroke-linejoin: round;
}
.flow-plabel-true { fill: var(--flow-trigger); }
.flow-plabel-false { fill: #ff8b8b; }
/* The collect trigger's commit gate (SOUL §11/§28): dashed + amber so "advance
   the cursor when this write succeeds" never reads as a second data edge. */
.flow-edge-commit { stroke: var(--warn-fg); stroke-dasharray: 7 5; }
.flow-edge-commit:hover { stroke: var(--warn-fg); stroke-width: 3; }
.flow-arrow-commit { fill: var(--warn-fg); }
.flow-port-commit { stroke: var(--warn-fg); }
.flow-port-commit:hover { fill: var(--warn-fg); }
.flow-plabel-commit { fill: var(--warn-fg); }
.flow-config {
  width: 19rem; flex-shrink: 0; border-left: 1px solid var(--border); background: var(--panel);
  overflow-y: auto;
}
.flow-config-empty { color: var(--muted); font-size: .85rem; padding: 1rem; }
.flow-config-body { display: flex; flex-direction: column; gap: .45rem; padding: .8rem .85rem; }
.flow-config-head { display: flex; align-items: center; justify-content: space-between; gap: .5rem; margin-bottom: .2rem; }
.flow-cfg-head-btns { display: flex; gap: .3rem; flex: none; }
.flow-config-title {
  font-size: .82rem; font-weight: 700; color: var(--accent);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; overflow-wrap: anywhere;
}
.flow-cfg-label {
  font-size: .72rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px;
  color: var(--muted); margin-top: .3rem;
}
.flow-cfg-input, .flow-cfg-area {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .4rem .55rem; font: inherit; font-size: .82rem; width: 100%;
}
.flow-cfg-area {
  min-height: 4rem; resize: vertical;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .8rem; line-height: 1.5;
}
.flow-cfg-area-tall { min-height: 8rem; }
.flow-cfg-input:focus, .flow-cfg-area:focus { outline: none; border-color: var(--accent); }
.flow-cfg-btn {
  background: var(--panel-2); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .3rem .6rem; font: inherit; font-size: .78rem; font-weight: 600; cursor: pointer;
}
.flow-cfg-del { color: var(--err-fg); border-color: #5a2730; }
.flow-cfg-del:hover { background: #5a2730; color: var(--on-accent); }
/* The bottom-sheet dismiss button only exists on mobile (see the media block); on
   desktop the config rail is always docked, so it has nothing to close. */
.flow-cfg-close { display: none; }
.flow-cfg-err { color: var(--err-fg); font-size: .78rem; }
.flow-cfg-warn { color: var(--warn-fg); font-size: .76rem; }
.flow-cfg-hint { color: var(--muted); font-size: .76rem; }
.flow-cfg-field { display: flex; flex-direction: column; gap: .15rem; }
.flow-tools {
  display: flex; flex-direction: column; gap: .5rem;
  max-height: 16rem; overflow-y: auto; padding: .5rem .55rem;
  background: var(--bg); border: 1px solid var(--border); border-radius: 8px;
}
.flow-tool-group { display: flex; flex-direction: column; gap: .15rem; }
.flow-tool-grp {
  font-size: .66rem; font-weight: 700; text-transform: uppercase; letter-spacing: .4px;
  color: var(--flow-agent);
}
.flow-tool {
  display: flex; align-items: center; gap: .4rem; font-size: .8rem;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; color: var(--fg); cursor: pointer;
}
.flow-tool input { accent-color: var(--flow-agent); cursor: pointer; }

/* --- Emerged UIs (AI-authored declarative UIs, rendered inline) --- */
.eu-artifact { margin-top: .5rem; }
.eu-app {
  border: 1px solid var(--border); border-radius: 10px; background: var(--panel);
  overflow: hidden;
}
.eu-app-head {
  padding: .45rem .7rem; font-weight: 700; font-size: .9rem;
  background: var(--panel-2); border-bottom: 1px solid var(--border);
}
.eu-app-body { padding: .7rem; display: flex; flex-direction: column; gap: .6rem; }
.eu-loading, .eu-load-error {
  padding: .6rem .7rem; font-size: .85rem; color: var(--muted);
}
.eu-load-error { color: var(--err-fg); }

/* Layout containers. */
.eu-stack { display: flex; flex-direction: column; gap: .6rem; }
.eu-row { display: flex; flex-direction: row; gap: .6rem; flex-wrap: wrap; align-items: flex-start; }
.eu-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: .6rem; }
.eu-constrained { box-sizing: border-box; min-width: 0; }
.eu-constrained.eu-align-start { align-self: flex-start; }
.eu-constrained.eu-align-center { align-self: center; margin-inline: auto; }
.eu-constrained.eu-align-end { align-self: flex-end; margin-inline-start: auto; }
.eu-constrained.eu-align-stretch { align-self: stretch; width: 100%; }
.eu-constrained.eu-overflow-visible { overflow: visible; }
.eu-constrained.eu-overflow-hidden { overflow: hidden; }
.eu-constrained.eu-overflow-auto { overflow: auto; }
.eu-constrained > .eu-img { max-height: inherit; }
.eu-aspect-ratio { width: 100%; min-width: 0; overflow: hidden; }
.eu-aspect-ratio > * { width: 100%; height: 100%; min-width: 0; }
.eu-aspect-ratio > .eu-img { max-width: none; object-position: center; }
.eu-aspect-ratio.eu-fit-contain > .eu-img { object-fit: contain; }
.eu-aspect-ratio.eu-fit-cover > .eu-img { object-fit: cover; }
.eu-aspect-ratio.eu-fit-fill > .eu-img { object-fit: fill; }
.eu-card {
  border: 1px solid var(--border); border-radius: 8px; background: var(--panel-2);
  padding: .7rem; display: flex; flex-direction: column; gap: .5rem;
}
.eu-card-title { font-weight: 700; font-size: .9rem; }
.eu-cond { display: contents; }
.eu-divider { border: none; border-top: 1px solid var(--border); margin: .2rem 0; width: 100%; }

/* Tabs (header strip over the active panel). */
.eu-tabs { display: flex; flex-direction: column; gap: .6rem; }
.eu-tab-strip {
  display: flex; flex-wrap: wrap; gap: .15rem;
  border-bottom: 1px solid var(--border);
}
.eu-tab {
  background: none; border: none; cursor: pointer; color: var(--muted);
  font: inherit; font-size: .85rem; font-weight: 600;
  padding: .4rem .7rem; border-bottom: 2px solid transparent; margin-bottom: -1px;
}
.eu-tab:hover { color: var(--fg); }
.eu-tab-active { color: var(--fg); border-bottom-color: var(--accent-2); }
.eu-tab-panel { display: flex; flex-direction: column; gap: .6rem; }

/* Content. */
.eu-text { white-space: pre-wrap; word-break: break-word; }
.eu-heading { margin: 0; font-weight: 700; line-height: 1.2; }
.eu-md { white-space: normal; }
.eu-img { max-width: 100%; height: auto; border-radius: 7px; display: block; }
.eu-link { color: var(--accent); text-decoration: underline; text-underline-offset: 2px; }
.eu-link:hover { color: var(--accent-2); }

/* Status badge. */
.eu-badge {
  display: inline-flex; align-items: center; align-self: flex-start;
  padding: .1rem .5rem; border-radius: 999px; font-size: .72rem; font-weight: 700;
  line-height: 1.4; border: 1px solid var(--border); background: var(--panel-2); color: var(--fg);
}
.eu-badge-info { background: #1c2c4a; color: #cfe0ff; border-color: #2f4a7a; }
.eu-badge-success { background: var(--ok-bg); color: var(--ok-fg); border-color: var(--ok-border); }
.eu-badge-warn { background: #3a2e12; color: #f2d48b; border-color: #6a5320; }
.eu-badge-error { background: var(--err-bg); color: var(--err-fg); border-color: var(--err-border); }

/* Progress bar. */
.eu-progress-wrap { display: flex; flex-direction: column; gap: .2rem; width: 100%; }
.eu-progress-label { font-size: .78rem; color: var(--muted); font-weight: 600; }
.eu-progress {
  width: 100%; height: .55rem; background: var(--panel-2);
  border: 1px solid var(--border); border-radius: 999px; overflow: hidden;
}
.eu-progress-bar {
  height: 100%; background: var(--accent-2); border-radius: 999px;
  transition: width .25s ease;
}

/* Collections (list / table read-only leaves). */
.eu-list-host, .eu-table-host { width: 100%; }
.eu-list { margin: 0; padding-left: 1.25rem; display: flex; flex-direction: column; gap: .15rem; }
.eu-list li { color: var(--fg); font-size: .88rem; }
.eu-empty {
  padding: .6rem; text-align: center; font-size: .8rem; color: var(--muted);
  border: 1px dashed var(--border); border-radius: 8px; background: var(--panel-2);
}
.eu-table-scroll { overflow-x: auto; }
.eu-table { width: 100%; border-collapse: collapse; font-size: .88rem; }
.eu-table th {
  text-align: left; padding: .3rem .55rem; color: var(--muted); font-weight: 600;
  border-bottom: 1px solid var(--border); white-space: nowrap;
}
.eu-table td { padding: .3rem .55rem; border-bottom: 1px solid var(--border); color: var(--fg); }
.eu-table tbody tr:hover { background: var(--panel-2); }

/* for_each pagination (render.rs). `grid-column: 1 / -1` makes the pager / "load
   more" span every column when the loop's parent is a grid; harmless in a
   flex/block parent. */
.eu-pager {
  grid-column: 1 / -1; display: flex; align-items: center; justify-content: center;
  gap: .6rem; margin-top: .5rem;
}
.eu-pager-btn { padding: .25rem .6rem; font-size: .82rem; }
.eu-pager-status { font-size: .8rem; color: var(--muted); min-width: 6.5rem; text-align: center; }
.eu-scroll-sentinel {
  grid-column: 1 / -1; display: flex; justify-content: center;
  min-height: 1px; padding: .4rem 0;
}
.eu-more-btn { padding: .25rem .8rem; font-size: .82rem; }

/* ── Charts (charts.rs) ──────────────────────────────────────────────────────
   SVG data-viz primitives. Colours resolve to the --chart-N ramp / semantic
   tokens so every chart re-themes automatically. `.chart-host` is the reactive
   wrapper the emerged renderer emits; the figure/svg classes are shared by any
   standalone use. */
.chart-host { width: 100%; }
.chart { margin: 0; display: flex; flex-direction: column; gap: .3rem; min-width: 0; }
.chart-title { font-size: .82rem; font-weight: 700; color: var(--fg); }
.chart-svg { display: block; width: 100%; height: auto; overflow: visible; }
.chart-svg text { font-family: inherit; }
.chart-empty {
  padding: 1rem; text-align: center; font-size: .8rem; color: var(--muted);
  border: 1px dashed var(--border); border-radius: 8px; background: var(--panel-2);
}
/* Categorical marks. */
.chart-slice { stroke: var(--panel); stroke-width: 1.5; transition: opacity .15s ease; }
.chart-slice:hover { opacity: .82; }
.chart-bar { transition: opacity .15s ease; }
.chart-bar:hover { opacity: .82; }
.chart-cell { stroke: var(--panel); stroke-width: .5; transition: opacity .15s ease; }
.chart-cell:hover { opacity: .78; }
.chart-cell-label { fill: var(--fg); font-size: 9px; pointer-events: none; }
/* Invisible per-point hover targets (line / area / radar). */
.chart-hit { fill: transparent; pointer-events: all; }
/* Hover tooltips. */
.chart-tooltip { pointer-events: none; }
.chart-tooltip-bg {
  fill: var(--panel-2); stroke: var(--border); stroke-width: 1; fill-opacity: .97;
}
.chart-tooltip-text { fill: var(--fg); font-size: 11px; font-weight: 600; }
/* Axes / grid / labels. */
.chart-axis { stroke: var(--border); stroke-width: 1; }
.chart-grid { stroke: var(--border); stroke-width: 1; opacity: .55; }
.chart-axis-label { fill: var(--muted); font-size: 11px; }
/* Line / area / spark. */
.chart-line { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
.chart-area { stroke: none; fill-opacity: .18; }
.chart-spark { fill: none; stroke-width: 1.6; stroke-linejoin: round; stroke-linecap: round; }
.chart-dot { stroke: var(--panel); stroke-width: 1; }
/* Radar. */
.chart-radar-area { fill-opacity: .18; }
.chart-radar-line { fill: none; stroke-width: 2; stroke-linejoin: round; }
/* Gauge. */
.chart-gauge-track { stroke: var(--border); }
.chart-gauge-value { transition: stroke-dashoffset .25s ease; }
.chart-gauge-text { fill: var(--fg); font-weight: 700; font-size: 26px; }
.chart-gauge-label { fill: var(--muted); font-size: 11px; }
/* Legend. */
.chart-legend { display: flex; flex-wrap: wrap; gap: .25rem .8rem; margin-top: .1rem; }
.chart-legend-item { display: inline-flex; align-items: center; gap: .32rem; font-size: .76rem; color: var(--muted); }
.chart-legend-swatch { width: .7rem; height: .7rem; border-radius: 2px; flex: none; }
.chart-legend-label { white-space: nowrap; }

/* Inputs. */
.eu-field { display: flex; flex-direction: column; gap: .25rem; }
.eu-label { font-size: .78rem; color: var(--muted); font-weight: 600; }
.eu-input {
  background: var(--bg); color: var(--fg); border: 1px solid var(--border);
  border-radius: 7px; padding: .4rem .55rem; font: inherit; font-size: .88rem;
}
.eu-input:focus { outline: none; border-color: var(--accent); }
.eu-textarea { min-height: 4.5rem; resize: vertical; }
.eu-select { cursor: pointer; }
.eu-checkbox { display: flex; align-items: center; gap: .45rem; cursor: pointer; font-size: .88rem; }
.eu-checkbox input { accent-color: var(--accent-2); cursor: pointer; }
.eu-err { font-size: .75rem; color: var(--err-fg); min-height: .9rem; }

/* Radio group. */
.eu-radio-group { display: flex; flex-direction: column; gap: .3rem; }
.eu-radio { display: flex; align-items: center; gap: .45rem; cursor: pointer; font-size: .88rem; }
.eu-radio input { accent-color: var(--accent-2); cursor: pointer; }

/* Range slider. */
.eu-range-wrap { display: flex; align-items: center; gap: .6rem; }
.eu-range { flex: 1 1 auto; accent-color: var(--accent-2); cursor: pointer; }
.eu-range-value {
  flex: 0 0 auto; min-width: 2.2rem; text-align: right;
  font-size: .82rem; color: var(--muted); font-variant-numeric: tabular-nums;
}

/* Buttons. */
.eu-btn {
  background: var(--accent-2); color: var(--on-accent); border: 1px solid var(--accent-2);
  border-radius: 7px; padding: .4rem .8rem; font: inherit; font-size: .85rem;
  font-weight: 600; cursor: pointer; align-self: flex-start;
}
.eu-btn:hover:not(:disabled) { background: var(--accent); }
.eu-btn:disabled { opacity: .55; cursor: not-allowed; }

/* Timers (countdown / stopwatch). */
.eu-timer {
  display: inline-flex; align-items: center; gap: .7rem;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px;
  padding: .45rem .7rem;
}
.eu-timer-label { font-size: .85rem; color: var(--muted); }
.eu-timer-display {
  font-size: 1.25rem; font-weight: 700; color: var(--fg);
  font-variant-numeric: tabular-nums; letter-spacing: .03em;
}
.eu-timer-done .eu-timer-display { color: var(--accent-2); }
.eu-timer-controls { display: flex; gap: .35rem; }
.eu-timer-btn { padding: .25rem .6rem; font-size: .78rem; }

/* Dialogs (overlay shown when opened). */
.eu-dialog-backdrop {
  display: none; position: fixed; inset: 0; z-index: 50;
  background: rgba(0, 0, 0, .5); align-items: center; justify-content: center; padding: 1rem;
}
.eu-dialog-backdrop.eu-open { display: flex; }
.eu-dialog {
  background: var(--panel); border: 1px solid var(--border); border-radius: 10px;
  max-width: 32rem; width: 100%; max-height: 85vh; overflow: auto;
  display: flex; flex-direction: column;
}
.eu-dialog-head {
  display: flex; align-items: center; justify-content: space-between;
  padding: .55rem .7rem; border-bottom: 1px solid var(--border); font-weight: 700;
}
.eu-dialog-x {
  background: none; border: none; color: var(--muted); font-size: 1.2rem;
  line-height: 1; cursor: pointer; padding: 0 .25rem;
}
.eu-dialog-x:hover { color: var(--fg); }
.eu-dialog-body { padding: .7rem; display: flex; flex-direction: column; gap: .6rem; }

/* Inline notice for not-yet-wired (server-side) handlers. */
.eu-notice {
  display: flex; align-items: center; justify-content: space-between; gap: .5rem;
  margin: 0 .7rem .2rem; padding: .4rem .6rem; font-size: .8rem;
  background: #1c2c4a; color: #cfe0ff; border: 1px solid #2f4a7a; border-radius: 7px;
}
.eu-notice-x {
  background: none; border: none; color: #cfe0ff; font-size: 1.1rem;
  line-height: 1; cursor: pointer; padding: 0 .25rem;
}

/* Apps panel — the standalone emerged-UI browser (sidebar + stage). The
   sidebar is a narrow shared `.pane-list` that scrolls as one column (no
   header/body split — its rows are nav links, not a managed list). */
.apps-panel { height: 100%; }
.apps-sidebar { width: 220px; gap: .3rem; padding: .6rem; overflow-y: auto; }
.apps-sidebar-head {
  font-size: .7rem; text-transform: uppercase; letter-spacing: .08em;
  color: var(--muted); padding: .2rem .35rem;
}
.apps-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .15rem; }
.apps-row { display: flex; align-items: center; gap: .1rem; }
.apps-item {
  flex: 1 1 auto; min-width: 0; text-align: left; background: none; border: none;
  cursor: pointer; padding: .4rem .55rem; border-radius: 7px; color: var(--fg);
  font-size: .85rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.apps-item:hover { background: var(--panel-2); }
.apps-item-active { background: var(--accent); color: var(--on-accent); }
/* Per-row pin toggle feeding the nav quick menu: revealed on row hover, always
   visible once pinned (touch viewports show it unconditionally — no hover). */
.apps-pin {
  flex: 0 0 auto; background: none; border: none; cursor: pointer;
  color: var(--muted); padding: .3rem .35rem; border-radius: 6px;
  line-height: 1; visibility: hidden;
}
.apps-row:hover .apps-pin, .apps-pin:focus-visible, .apps-pin-on { visibility: visible; }
.apps-pin:hover { background: var(--panel-2); color: var(--fg); }
.apps-pin-on { color: var(--accent); }
.apps-error { font-size: .8rem; color: var(--err-fg); padding: .2rem .35rem; }
.apps-empty-list { font-size: .8rem; color: var(--muted); padding: .4rem .35rem; }
.apps-stage { flex: 1 1 auto; min-width: 0; overflow-y: auto; padding: .8rem; }
.apps-placeholder { color: var(--muted); font-size: .85rem; padding: 1rem; }

/* --- Quick-start / onboarding wizard (SOUL §12/§22/§23) --- */
.wizard { display: flex; flex-direction: column; flex: 1; min-width: 0; min-height: 0; }
.wizard-head {
  display: flex; flex-direction: column; gap: .3rem;
  padding: 1rem 1.2rem .7rem; border-bottom: 1px solid var(--border); background: var(--panel);
}
.wizard-title { margin: 0; font-size: 1.25rem; font-weight: 800; letter-spacing: .2px; }
.wizard-sub { margin: 0; color: var(--muted); font-size: .9rem; }
.wizard-steps {
  list-style: none; display: flex; flex-wrap: wrap; gap: .4rem; margin: .5rem 0 0; padding: 0;
  counter-reset: wizstep;
}
.wizard-pip {
  counter-increment: wizstep; font-size: .76rem; color: var(--muted);
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 999px;
  padding: .15rem .65rem;
}
.wizard-pip::before { content: counter(wizstep) ". "; opacity: .7; }
.wizard-pip-on { color: var(--on-accent); background: var(--accent-2); border-color: var(--accent-2); }
.wizard-pip-done { color: var(--ok-fg); background: var(--ok-bg); border-color: var(--ok-border); }
.wizard-body { flex: 1; min-height: 0; overflow-y: auto; padding: 1.1rem 1.2rem; }
.wizard-section { display: flex; flex-direction: column; gap: .7rem; max-width: 620px; }
.wizard-h2 { margin: 0; font-size: 1.05rem; font-weight: 700; }
.wizard-help { margin: 0; color: var(--muted); font-size: .88rem; line-height: 1.5; }
.wizard-muted { color: var(--muted); font-size: .85rem; font-style: italic; margin: .2rem 0; }
.wizard-warn {
  margin: .2rem 0 0; font-size: .85rem; padding: .55rem .7rem;
  color: var(--warn-fg); background: var(--warn-bg); border: 1px solid var(--warn-border); border-radius: 8px;
}
.wizard-error {
  margin: .7rem 0 0; font-size: .85rem; padding: .55rem .7rem; color: var(--err-fg);
  background: var(--err); border: 1px solid var(--err-border); border-radius: 8px;
}
.wizard-textarea { resize: vertical; min-height: 4.5rem; font: inherit; line-height: 1.5; }
.wizard-skill-list { list-style: none; margin: .3rem 0 0; padding: 0; display: flex; flex-direction: column; gap: .5rem; }
.wizard-skill {
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px; padding: .6rem .7rem;
  display: flex; flex-direction: column; gap: .3rem;
}
.wizard-skill-head { display: flex; align-items: center; gap: .5rem; cursor: pointer; }
.wizard-skill-head input { accent-color: var(--accent-2); cursor: pointer; }
.wizard-skill-name {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .85rem; font-weight: 600;
}
.wizard-skill-desc { margin: 0; color: var(--muted); font-size: .84rem; line-height: 1.45; }
.wizard-skill-detail { font-size: .82rem; }
.wizard-skill-detail > summary { cursor: pointer; color: var(--accent); user-select: none; }
.wizard-skill-md {
  margin: .35rem 0 0; white-space: pre-wrap; word-break: break-word; max-height: 16rem; overflow: auto;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .76rem;
  background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: .5rem .6rem;
}
/* Personalization chat (step 4): a bubble log, a composer, and the reviewable
   memory/skill proposals it accumulates. */
.wizard-chat-section { max-width: 680px; }
.wizard-chat {
  display: flex; flex-direction: column; gap: .5rem;
  border: 1px solid var(--border); border-radius: 12px; background: var(--panel-2); padding: .6rem;
}
.wizard-chat-log {
  display: flex; flex-direction: column; gap: .45rem;
  min-height: 8rem; max-height: 22rem; overflow-y: auto; padding: .2rem;
}
.wizard-msg {
  max-width: 82%; padding: .5rem .7rem; border-radius: 12px; font-size: .88rem; line-height: 1.5;
  white-space: pre-wrap; word-break: break-word;
}
.wizard-msg-assistant {
  align-self: flex-start; background: var(--panel); border: 1px solid var(--border);
  border-bottom-left-radius: 4px;
}
.wizard-msg-user {
  align-self: flex-end; background: var(--user); color: var(--on-accent);
  border-bottom-right-radius: 4px;
}
.wizard-msg-typing { display: flex; align-items: center; gap: .28rem; padding: .6rem .7rem; }
.wizard-typing-dot {
  width: .4rem; height: .4rem; border-radius: 50%; background: var(--muted);
  animation: wizard-typing 1s ease-in-out infinite;
}
.wizard-typing-dot:nth-child(2) { animation-delay: .15s; }
.wizard-typing-dot:nth-child(3) { animation-delay: .3s; }
@keyframes wizard-typing {
  0%, 60%, 100% { opacity: .3; transform: translateY(0); }
  30% { opacity: 1; transform: translateY(-2px); }
}
.wizard-chat-input { display: flex; gap: .5rem; align-items: flex-end; }
.wizard-chat-textarea { flex: 1; resize: vertical; min-height: 2.6rem; font: inherit; line-height: 1.45; }
.wizard-chat-send { flex: none; align-self: stretch; }
.wizard-proposals {
  display: flex; flex-direction: column; gap: .45rem; margin-top: .3rem;
  border-top: 1px solid var(--border); padding-top: .7rem;
}
.wizard-proposal-title { margin: .2rem 0 0; font-size: .82rem; font-weight: 700; color: var(--muted); text-transform: uppercase; letter-spacing: .4px; }
.wizard-mem-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: .35rem; }
.wizard-mem {
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 9px; padding: .45rem .6rem;
}
.wizard-mem-head { display: flex; align-items: center; gap: .5rem; cursor: pointer; }
.wizard-mem-head input { accent-color: var(--accent-2); cursor: pointer; }
.wizard-mem-text { font-size: .86rem; line-height: 1.4; }
.wizard-done { align-items: flex-start; }
.wizard-foot {
  display: flex; align-items: center; gap: .6rem;
  padding: .7rem 1.2rem; border-top: 1px solid var(--border); background: var(--panel);
}
.wizard-foot-spacer { flex: 1; }

/* Theme picker (Settings → Appearance + Quick start). A grid of cards, each a
   live mini-preview of its palette drawn from inline literals so the swatch
   shows the theme's own colours regardless of the one currently applied. */
.appearance-intro { margin: 0 0 .9rem; color: var(--muted); font-size: .85rem; line-height: 1.5; }
.theme-grid {
  display: grid; gap: .7rem;
  grid-template-columns: repeat(auto-fill, minmax(13.5rem, 1fr));
}
.theme-card {
  display: flex; flex-direction: column; gap: .6rem; text-align: left;
  padding: .6rem; background: var(--panel-2); color: var(--fg);
  border: 1px solid var(--border); border-radius: 12px; cursor: pointer;
  font: inherit; transition: border-color .15s ease, transform .15s ease, box-shadow .15s ease;
}
.theme-card:hover { border-color: var(--accent); transform: translateY(-2px); }
.theme-card:focus-visible { outline: none; border-color: var(--accent); }
.theme-card-active {
  border-color: var(--accent-2);
  box-shadow: 0 0 0 2px var(--accent-2), 0 8px 22px var(--scrim);
}
.theme-swatch {
  display: flex; flex-direction: column; height: 4.2rem; overflow: hidden;
  border: 1px solid var(--border); border-radius: 8px;
}
.theme-swatch-bar {
  display: flex; align-items: center; justify-content: flex-end;
  height: 1.15rem; padding: 0 .4rem; border-bottom: 1px solid var(--border); flex: none;
}
.theme-swatch-dot { width: .5rem; height: .5rem; border-radius: 50%; }
.theme-swatch-body {
  flex: 1; display: flex; flex-direction: column; gap: .3rem;
  padding: .45rem .5rem .5rem;
}
.theme-swatch-line { height: .3rem; border-radius: 99px; opacity: .92; }
.theme-swatch-line-lg { width: 72%; }
.theme-swatch-line-sm { width: 46%; opacity: .6; }
.theme-swatch-pill { width: 38%; height: .55rem; border-radius: 99px; margin-top: auto; }
.theme-card-meta { display: flex; flex-direction: column; gap: .15rem; }
.theme-card-name {
  display: flex; align-items: center; gap: .4rem;
  font-size: .92rem; font-weight: 700; letter-spacing: .1px;
}
.theme-card-check { color: var(--accent); font-size: .8rem; }
.theme-card-blurb { color: var(--muted); font-size: .76rem; line-height: 1.35; }
@media (prefers-reduced-motion: reduce) {
  .theme-card { transition: none; }
  .theme-card:hover { transform: none; }
}

/* Custom-palette editor (Settings → Appearance, revealed when "Custom" is the
   active theme). Per-token colour inputs apply live; the JSON panels mirror the
   palette for copy-out (export) and paste-in (import). The custom theme's tokens
   are injected at runtime as :root[data-theme="custom"] (see theme.rs), not as a
   static block above. */
.custom-theme {
  margin-top: 1rem; padding-top: 1rem; border-top: 1px solid var(--border);
  display: flex; flex-direction: column; gap: .8rem;
}
.custom-theme-head { display: flex; align-items: center; justify-content: space-between; gap: .6rem; }
.custom-theme-title { margin: 0; font-size: .95rem; font-weight: 700; }
.custom-theme-hint { margin: 0; color: var(--muted); font-size: .78rem; line-height: 1.45; }
.custom-theme-grid {
  display: grid; gap: .45rem .9rem;
  grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr));
}
.custom-theme-field { display: flex; align-items: center; justify-content: space-between; gap: .6rem; }
.custom-theme-label { color: var(--muted); font-size: .76rem; }
.custom-theme-inputs { display: flex; align-items: center; gap: .35rem; flex: none; }
.custom-theme-color {
  width: 1.9rem; height: 1.9rem; padding: 0; flex: none; cursor: pointer;
  background: var(--panel-2); border: 1px solid var(--border); border-radius: 6px;
}
.custom-theme-text {
  width: 8.5rem; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .76rem;
}
.custom-theme-io {
  display: grid; gap: .9rem;
  grid-template-columns: repeat(auto-fit, minmax(15rem, 1fr));
}
.custom-theme-io-col { display: flex; flex-direction: column; gap: .35rem; }
.custom-theme-json {
  min-height: 9rem; resize: vertical; white-space: pre;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: .72rem; line-height: 1.45;
}
.custom-theme-io-actions { display: flex; gap: .5rem; }

/* ── Mobile (≤768px) ─────────────────────────────────────────────────────────
   Phones and narrow tablets. Three moves:
   1. The workbench nav and the chat sessions sidebar become off-canvas drawers
      (hamburger in the header / "Chats" in the chat toolbar) so the content
      pane gets the full width.
   2. Every master-detail panel stacks: the list pane sits above the detail at
      a capped height instead of beside it at a fixed width.
   3. Modals go near-fullscreen and their tab/rail columns turn into
      horizontally scrollable rows.
   Hover-revealed row actions are always visible here — touch has no hover.  */
@media (max-width: 768px) {
  /* Header: tighter, hamburger shown, tagline dropped for space. */
  .wb-header { padding: .55rem .7rem; gap: .5rem; flex-wrap: wrap; align-items: center; }
  .wb-subtitle { display: none; }
  .wb-menu-btn { display: inline-flex; }

  /* Left nav → off-canvas drawer over a scrim. */
  .wb-nav {
    position: fixed; top: 0; bottom: 0; left: 0; z-index: 60;
    width: min(78vw, 280px); padding-top: .8rem; overflow-y: auto;
    transform: translateX(-105%); transition: transform .18s ease;
    box-shadow: 0 0 40px rgba(0,0,0,.45);
  }
  .wb-nav-open { transform: none; }
  .wb-nav-scrim-open { display: block; opacity: 1; pointer-events: auto; }
  .nav-item { padding: .65rem .7rem; }
  .nav-section-toggle { padding: .65rem .7rem; }
  /* Pinned-apps quick menu: no hover on touch — only the ▸ toggle opens it,
     inline (a right-side flyout would clip against the drawer's overflow). */
  .nav-apps-flyout {
    position: static; margin: 0 0 .2rem 1rem; padding: 0; min-width: 0;
    max-width: none; background: transparent; border: 0; box-shadow: none;
  }
  .nav-apps:hover .nav-apps-flyout { display: none; }
  .nav-apps.nav-apps-open .nav-apps-flyout { display: block; }
  /* The sidebar pin + delete controls can't be hover-revealed on touch. */
  .apps-pin { visibility: visible; }
  .apps-row .row-acts-reveal { opacity: 1; }

  /* Chat: the sessions sidebar becomes a drawer toggled from the toolbar. */
  .chat-sessions-toggle { display: inline-block; }
  .chat-sidebar {
    position: fixed; top: 0; bottom: 0; left: 0; z-index: 60;
    width: min(85vw, 320px);
    transform: translateX(-105%); transition: transform .18s ease;
    box-shadow: 0 0 40px rgba(0,0,0,.45);
  }
  .chat-sidebar-open { transform: none; }
  .chat-sidebar-scrim-open { display: block; opacity: 1; pointer-events: auto; }
  /* Session row actions inline instead of hover-overlaid — and always shown
     (no hover on touch), overriding the shared `.row-acts-reveal` opacity. */
  .chat-session-acts { position: static; transform: none; }
  .chat-session-row .row-acts-reveal { opacity: 1; }
  .chat-session-row .chat-session { width: auto; }
  /* Right side panel (Output/Settings) overlays instead of splitting the pane. */
  .chat-side {
    position: fixed; top: 0; right: 0; bottom: 0; z-index: 60;
    width: min(92vw, 360px); box-shadow: 0 0 40px rgba(0,0,0,.45);
  }
  .msg { max-width: 94%; }
  /* No hover on touch — keep the per-message actions visible. */
  .msg-acts { opacity: .6; }
  .chat-log { padding: .7rem; }
  /* Right side panel gets a tap-away backdrop like the other drawers. */
  .chat-side-scrim { display: block; }
  /* Composer: the textarea takes a full row and the actions wrap beneath it, so
     typing space never collapses when Send/Stop/📎 crowd the row. Touch-sized. */
  .chat-input { padding: .5rem .7rem; }
  .chat-input-row { flex-wrap: wrap; }
  .chat-textarea { flex-basis: 100%; }
  .chat-send { flex: 1 1 auto; }
  .chat-send, .chat-stop, .chat-attach-btn, .chat-mic, .chat-voice { min-height: 44px; }
  /* Larger session tap targets; row actions are already shown inline (no hover). */
  .chat-session { padding: .6rem .7rem; }
  .row-act { padding: .45rem .55rem; }

  /* Master-detail panels stack: list pane on top (capped), detail below. The
     drawer panels below also go column so their ☰ toggle bar sits atop the
     detail pane (the list itself is fixed/off-canvas, out of flow). */
  .pane-split { flex-direction: column; }
  /* Email keeps the stacked list-on-top layout (it has its own mailboxes-rail
     drawer, so the message list stays in flow beneath it). */
  .email-list {
    width: 100%; max-height: 42vh;
    border-right: 0; border-bottom: 1px solid var(--border);
  }
  /* Apps/Skills/Profiles/Grants/History/Memory/Calendars/Notes/Automations: the
     list pane becomes an off-canvas drawer instead of stacking, toggled by the
     detail pane's ☰ (`widgets::list_drawer_*`) — the same pattern as the chat
     sessions sidebar. */
  .list-drawer {
    position: fixed; top: 0; bottom: 0; left: 0; z-index: 60;
    width: min(85vw, 320px); max-height: none;
    background: var(--panel); border: 0; border-right: 1px solid var(--border);
    transform: translateX(-105%); transition: transform .18s ease;
    box-shadow: 0 0 40px rgba(0,0,0,.45);
  }
  .list-drawer-open { transform: none; }
  .list-drawer-scrim-open { display: block; opacity: 1; pointer-events: auto; }
  .list-drawer-toggle { display: inline-flex; }
  /* In a column layout the detail pane needs an explicit min-height to shrink
     (flex min-height:auto would otherwise let it overflow the viewport). */
  .pane-split > :not(.pane-list) { min-height: 0; }

  /* Email: the mailboxes rail becomes a left drawer over the stacked
     list/detail panes, toggled from the list header's ☰ button. */
  .email-mailboxes {
    position: fixed; top: 0; bottom: 0; left: 0; z-index: 60;
    width: min(85vw, 300px);
    transform: translateX(-105%); transition: transform .18s ease;
    box-shadow: 0 0 40px rgba(0,0,0,.45);
  }
  .email-mailboxes-open { transform: none; }
  .email-mbx-scrim-open { display: block; opacity: 1; pointer-events: auto; }
  .email-mbx-toggle { display: inline-flex; }

  /* Email on mobile is a true master→detail: the list fills the screen until a
     message is opened, which swaps to a full-screen detail carrying a Back bar;
     Back returns to the list. Overrides the shared 42vh list cap above. */
  .email-panel .email-list { flex: 1; max-height: none; border-bottom: 0; }
  .email-panel-detail .email-list { display: none; }
  .email-panel:not(.email-panel-detail) .email-detail { display: none; }
  .email-detail-topbar { display: flex; }

  /* Panel headers with action rows wrap instead of overflowing. */
  .cal-header, .files-header, .graph-header { flex-wrap: wrap; gap: .5rem; }
  /* The calendar's button cluster (Refresh/Add event/New calendar/Connect) is a
     nested flex row with `flex-shrink:0` — it keeps its full content width and
     pushes the page into horizontal overflow on a phone. Give it the full row
     (so `.cal-header`'s wrap drops it below the titles) and let its own buttons
     wrap within that width. */
  .cal-header-actions { flex-wrap: wrap; width: 100%; }

  /* Agenda density: on a phone the list is the primary surface, so spend less
     of the viewport on chrome and whitespace while retaining every event field.
     The mode classes keep Month/Week/Day at their roomier, grid-friendly scale. */
  .cal-panel-agenda .cal-header { padding: .55rem .65rem; }
  .cal-panel-agenda .cal-subtitle { display: none; }
  .cal-panel-agenda .cal-header-actions { gap: .3rem; }
  .cal-panel-agenda .cal-btn { padding: .35rem .55rem; font-size: .78rem; }
  .cal-panel-agenda .cal-viewbar { padding: .35rem .6rem; }
  .cal-panel-agenda .cal-viewtabs { width: 100%; }
  .cal-panel-agenda .cal-viewtab { flex: 1; padding: .28rem .35rem; }
  .cal-panel-agenda .cal-filter { gap: .25rem; padding: .35rem .6rem; }
  .cal-panel-agenda .cal-filter-date { min-width: 0; padding: .25rem .35rem; }
  .cal-panel-agenda .cal-labelbar { gap: .25rem; padding: .3rem .6rem .35rem; }
  .cal-panel-agenda .cal-label-chip { padding: .1rem .45rem; }

  .cal-body-agenda { padding: .45rem .55rem .7rem; }
  .cal-body-agenda .cal-agenda { gap: .65rem; max-width: none; }
  .cal-body-agenda .cal-day { gap: .18rem; }
  .cal-body-agenda .cal-day-heading {
    top: -.45rem; padding: .12rem 0; font-size: .7rem; letter-spacing: .45px;
  }
  .cal-body-agenda .cal-event-list { gap: .18rem; }
  .cal-body-agenda .cal-event {
    gap: .45rem; padding: .34rem .45rem; border-radius: 7px;
  }
  .cal-body-agenda .cal-event-time {
    min-width: 5.7rem; font-size: .72rem; line-height: 1.25;
  }
  .cal-body-agenda .cal-event-main { gap: .08rem; }
  .cal-body-agenda .cal-event-summary {
    gap: .28rem; font-size: .86rem; line-height: 1.2;
  }
  .cal-body-agenda .cal-event-meta { gap: .2rem .3rem; line-height: 1.15; }
  .cal-body-agenda .cal-event-loc { font-size: .72rem; }
  .cal-body-agenda .cal-event-cal { font-size: .62rem; padding: .02rem .3rem; }
  .cal-body-agenda .cal-event-label { font-size: .6rem; padding: .01rem .3rem; }
  .cal-body-agenda .cal-event-detail {
    margin-top: .2rem; gap: .25rem; padding: .4rem .45rem;
  }
  .cal-body-agenda .cal-event-body { font-size: .8rem; line-height: 1.4; }
  .cal-body-agenda .cal-event-kv { gap: .4rem; font-size: .76rem; }
  .cal-body-agenda .cal-event-detail-k { min-width: 4.4rem; }
  /* The shared mobile action rule enlarges all row controls. Agenda actions sit
     inside every writable event, so restore their already-usable base size to
     avoid making otherwise one-line events nearly twice as tall. */
  .cal-body-agenda .cal-event-acts .row-act { padding: .3rem .4rem; }

  /* Modals: near-fullscreen; side rails become horizontal scrollers. */
  .settings-overlay { padding: .6rem; }
  .settings-modal { max-height: calc(100vh - 1.2rem); max-height: calc(100dvh - 1.2rem); }
  .settings-layout { flex-direction: column; }
  .settings-tabs {
    flex-direction: row; overflow-x: auto; min-width: 0; padding: .45rem .5rem;
    border-right: 0; border-bottom: 1px solid var(--border);
  }
  .settings-tab { flex: none; white-space: nowrap; }
  .org-modal { height: auto; }
  .org-layout { flex-direction: column; }
  .org-rail {
    width: 100%; max-height: 11rem;
    border-right: 0; border-bottom: 1px solid var(--border);
  }
  .org-detail { min-height: 0; }

  /* Trim the tab-panel side padding so forms/lists aren't cramped on a phone. */
  .settings-content, .org-detail { padding: .9rem .85rem 1.1rem; }

  /* Row-layout forms (API keys: lifetime + scope + Issue) stack vertically; the
     fixed-narrow lifetime input goes full-width so the button isn't pushed off. */
  .settings-form-row { flex-direction: column; align-items: stretch; }
  .settings-input-narrow { max-width: none; }

  /* Key/value rows (LLM gateway) stack — a 9.5rem term leaves too little for the
     value on a narrow viewport. */
  .settings-kv-row { flex-direction: column; gap: .1rem; align-items: stretch; }
  .settings-kv-row dt { min-width: 0; }

  /* Service rows wrap and drop the wide fixed name column so the status badge
     never squeezes the name/detail off-screen. */
  .settings-svc { flex-wrap: wrap; }
  .settings-svc-name { min-width: 0; }

  /* Connection / token / section-head rows wrap their trailing controls under the
     label instead of overflowing when the row runs out of width. */
  .settings-conn, .settings-token, .settings-section-head { flex-wrap: wrap; }

  /* Topology editor keeps the ordered-chain semantics while collapsing its
     desktop two-column fields into touch-friendly single-column rows. */
  .llmleaf-heading { flex-direction: column; gap: .45rem; }
  .llmleaf-form-grid { grid-template-columns: 1fr; }
  .llmleaf-target-row { grid-template-columns: 1.7rem minmax(0, 1fr) 1.7rem; }
  .llmleaf-target-row .settings-field:nth-of-type(2) { grid-column: 2 / 3; }
  .llmleaf-target-remove { grid-column: 3; grid-row: 1 / 3; }
  .llmleaf-form-footer { align-items: stretch; flex-direction: column; }
  .llmleaf-save { width: 100%; min-height: 2.65rem; }
  .llmleaf-entry { align-items: flex-start; flex-wrap: wrap; }
  .llmleaf-entry-actions { width: 100%; justify-content: flex-end; }

  /* iOS zooms the page when a focused field's font is under 16px. */
  input, select, textarea { font-size: 16px; }

  /* Automation flow editor (SOUL §11). A 19rem side rail would crush the canvas on
     a phone, so the per-node inspector becomes a bottom sheet. It's viewport-FIXED
     (not absolute in the canvas) because the editor scrolls inside a long form — an
     absolute sheet would ride off-screen with the canvas. Shown only when a node is
     selected (flow-config-open); tap the canvas or ✕ to dismiss. */
  .flow-config {
    position: fixed; left: 0; right: 0; bottom: 0; width: auto;
    max-height: 60vh; max-height: 60dvh; z-index: 60;
    border-left: 0; border-top: 1px solid var(--border); border-radius: 16px 16px 0 0;
    box-shadow: 0 -14px 40px rgba(0,0,0,.55);
    transform: translateY(calc(100% + 8px)); transition: transform .2s ease;
  }
  .flow-config-open { transform: none; }
  .flow-cfg-close { display: inline-flex; }
  /* The config head sticks to the sheet top so Duplicate/Delete/✕ stay reachable
     while the body scrolls. */
  .flow-config-open .flow-config-head {
    position: sticky; top: 0; z-index: 1; margin: -.8rem -.85rem .2rem; padding: .7rem .85rem;
    background: var(--panel); border-bottom: 1px solid var(--border);
  }
  /* Give the canvas real estate: the editor block fills most of the phone viewport
     (it scrolls into view below the name/fire/palette chrome). */
  .auto-flow-wrap { height: 80vh; height: 80dvh; min-height: 26rem; }
  /* Comfortable touch targets for the zoom cluster (was ~27px). */
  .flow-zoom-btn { min-width: 2.2rem; height: 2.2rem; }
  /* Trim palette chrome so more of the fixed-height wrap is canvas, not buttons. */
  .flow-palette { padding: .45rem .55rem; gap: .35rem; }
  .flow-pal-btn { padding: .3rem .6rem .3rem .5rem; }
  .flow-node-search { padding: .4rem .55rem; }
}
"#;

/// wasm entrypoint, invoked by the Trunk-generated bootstrap.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
