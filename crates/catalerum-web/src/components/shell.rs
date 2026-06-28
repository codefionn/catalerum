//! The workbench app shell (SOUL §12).
//!
//! A header with the product title plus a left nav. The nav pins the
//! frequently-used panels — Chat, Calendar, Files, Notes, Email — and tucks the
//! rest — Skills, Tasks, Automations, Grants, History, Fetch, Memory, Graph —
//! into a collapsible "More" section. **All panels are active.** The selected
//! panel renders in the main content area.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::MouseEvent;

use crate::auth;
use crate::components::automations::AutomationsPanel;
use crate::components::calendar::CalendarPanel;
use crate::components::chat::ChatPanel;
use crate::components::conversations::ConversationsPanel;
use crate::components::dialogs::{DialogHost, Dialogs};
use crate::components::email::EmailPanel;
use crate::components::emerged::pins::{self, PinnedApp};
use crate::components::emerged::AppsPanel;
use crate::components::fetch::FetchPanel;
use crate::components::files::FilesPanel;
use crate::components::grants::GrantsPanel;
use crate::components::graph::GraphPanel;
use crate::components::icons::{Icon, MdIcon};
use crate::components::mcp_endpoints::McpEndpointsPanel;
use crate::components::memory::MemoryPanel;
use crate::components::notes::NotesPanel;
use crate::components::onboarding::OnboardingPanel;
use crate::components::profiles::ProfilesPanel;
use crate::components::settings::SettingsDialog;
use crate::components::skills::SkillsPanel;
use crate::components::tasks::TasksPanel;
use crate::components::workspace::WorkspaceSwitcher;
use crate::rest;

const APP_ROUTE_PREFIX: &str = "/app";

/// The panels surfaced in the left nav. They split into a [pinned](Panel::pinned)
/// group (always shown) and a [collapsed](Panel::collapsed) group (tucked into
/// the "More" section) per SOUL §12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    /// The streaming chat workbench (active in M1).
    Chat,
    /// Calendar view (M2).
    Calendar,
    /// File browser (M3).
    Files,
    /// Markdown notes editor (M3).
    Notes,
    /// Skills manager (SOUL §23).
    Skills,
    /// Agent-profile manager — scoped agents, subagents, channels (SOUL §19/§25).
    Profiles,
    /// Automations builder + run history (SOUL §11).
    Automations,
    /// Capability-grant builder (SOUL §19, admin).
    Grants,
    /// Scripted MCP endpoint manager (SOUL §30/§26).
    McpEndpoints,
    /// Conversation-history browser (SOUL §12).
    History,
    /// Read-only email inbox (SOUL §28).
    Email,
    /// Web-fetch utility (SOUL §27).
    Fetch,
    /// Kanban tasks board (SOUL §24).
    Tasks,
    /// Memories + profile (SOUL §22).
    Memory,
    /// Graph explorer — safe Datalog (SOUL §6.3).
    Graph,
    /// Emerged-UI browser — AI-authored apps, rendered standalone (SOUL §12).
    Apps,
    /// Quick-start / onboarding wizard (SOUL §12/§22/§23). Auto-opens on first run.
    Onboarding,
}

impl Panel {
    /// Human label for the nav entry.
    pub fn label(self) -> &'static str {
        match self {
            Panel::Chat => "Chat",
            Panel::Calendar => "Calendar",
            Panel::Files => "Files",
            Panel::Notes => "Notes",
            Panel::Skills => "Skills",
            Panel::Profiles => "Profiles",
            Panel::Automations => "Automations",
            Panel::Grants => "Grants",
            Panel::McpEndpoints => "MCP Endpoints",
            Panel::History => "History",
            Panel::Email => "Email",
            Panel::Fetch => "Fetch",
            Panel::Tasks => "Tasks",
            Panel::Memory => "Memory",
            Panel::Graph => "Graph",
            Panel::Apps => "Apps",
            Panel::Onboarding => "Quick start",
        }
    }

    /// Material icon paired with this panel in the workbench navigation.
    pub fn icon(self) -> MdIcon {
        match self {
            Panel::Chat => MdIcon::Chat,
            Panel::Calendar => MdIcon::Calendar,
            Panel::Files => MdIcon::Folder,
            Panel::Notes => MdIcon::Notes,
            Panel::Skills => MdIcon::Skills,
            Panel::Profiles => MdIcon::Profiles,
            Panel::Automations => MdIcon::Automations,
            Panel::Grants => MdIcon::Grants,
            Panel::McpEndpoints => MdIcon::McpEndpoints,
            Panel::History => MdIcon::History,
            Panel::Email => MdIcon::Email,
            Panel::Fetch => MdIcon::Fetch,
            Panel::Tasks => MdIcon::Tasks,
            Panel::Memory => MdIcon::Memory,
            Panel::Graph => MdIcon::Graph,
            Panel::Apps => MdIcon::Apps,
            Panel::Onboarding => MdIcon::QuickStart,
        }
    }

    /// Stable frontend route path for this panel. The `/app` prefix keeps the
    /// workbench routes distinct from root-level API endpoints such as `/fetch`,
    /// `/events`, or `/graph/query`.
    pub fn path(self) -> &'static str {
        match self {
            Panel::Chat => "/app/chat",
            Panel::Calendar => "/app/calendar",
            Panel::Files => "/app/files",
            Panel::Notes => "/app/notes",
            Panel::Skills => "/app/skills",
            Panel::Profiles => "/app/profiles",
            Panel::Automations => "/app/automations",
            Panel::Grants => "/app/grants",
            Panel::McpEndpoints => "/app/mcp-endpoints",
            Panel::History => "/app/history",
            Panel::Email => "/app/email",
            Panel::Fetch => "/app/fetch",
            Panel::Tasks => "/app/tasks",
            Panel::Memory => "/app/memory",
            Panel::Graph => "/app/graph",
            Panel::Apps => "/app/apps",
            Panel::Onboarding => "/app/quick-start",
        }
    }

    /// Anchor `href` for this panel. Ordinary same-window clicks are intercepted
    /// by the shell, but the URL remains real for copy/open-in-new-tab flows.
    pub fn href(self) -> &'static str {
        self.path()
    }

    /// Resolve a route path into a panel. Unknown paths intentionally fall back
    /// to Chat so a stale link still lands on a working workspace surface.
    pub fn from_path(path: &str) -> Panel {
        // Resolve on the first path segment only, so a panel-internal sub-route
        // (e.g. the calendar's `/app/calendar/month`) still maps to its owning
        // panel instead of falling through to Chat.
        let normalized = normalize_panel_path(path);
        let segment = normalized.split('/').next().unwrap_or_default();
        match segment {
            "" | "/" | "chat" => Panel::Chat,
            "calendar" => Panel::Calendar,
            "files" => Panel::Files,
            "notes" => Panel::Notes,
            "skills" => Panel::Skills,
            "profiles" => Panel::Profiles,
            "automations" => Panel::Automations,
            "grants" => Panel::Grants,
            "mcp-endpoints" | "mcp-endpoint" => Panel::McpEndpoints,
            "history" => Panel::History,
            "email" => Panel::Email,
            "fetch" => Panel::Fetch,
            "tasks" => Panel::Tasks,
            "memory" | "memories" => Panel::Memory,
            "graph" => Panel::Graph,
            "apps" | "uis" => Panel::Apps,
            "quick-start" | "onboarding" => Panel::Onboarding,
            _ => Panel::Chat,
        }
    }

    /// Whether the panel is implemented. Every workbench panel is now active.
    pub fn enabled(self) -> bool {
        let _ = self;
        true
    }

    /// Panels pinned to the top of the nav, always visible, in nav order.
    pub fn pinned() -> [Panel; 5] {
        [
            Panel::Chat,
            Panel::Calendar,
            Panel::Files,
            Panel::Notes,
            Panel::Email,
        ]
    }

    /// Panels tucked into the collapsible "More" section, in nav order.
    pub fn collapsed() -> [Panel; 12] {
        [
            Panel::Onboarding,
            Panel::Apps,
            Panel::Skills,
            Panel::Profiles,
            Panel::Tasks,
            Panel::Automations,
            Panel::Grants,
            Panel::McpEndpoints,
            Panel::History,
            Panel::Fetch,
            Panel::Memory,
            Panel::Graph,
        ]
    }
}

fn normalize_panel_path(path: &str) -> String {
    let path = path
        .trim()
        .trim_start_matches('#')
        .split(['?', '&'])
        .next()
        .unwrap_or_default()
        .trim_start_matches('/')
        .trim_end_matches('/')
        .to_ascii_lowercase();
    path.strip_prefix("app/")
        .unwrap_or(path.as_str())
        .to_string()
}

fn panel_from_location() -> Panel {
    let Some(window) = web_sys::window() else {
        return Panel::Chat;
    };
    let location = window.location();
    if let Ok(pathname) = location.pathname() {
        let path = normalize_panel_path(&pathname);
        if pathname.starts_with(APP_ROUTE_PREFIX) || !path.is_empty() {
            return Panel::from_path(&path);
        }
    }
    location
        .hash()
        .ok()
        .filter(|hash| !hash.is_empty())
        .map(|hash| Panel::from_path(&hash))
        .unwrap_or(Panel::Chat)
}

fn current_frontend_path() -> Option<String> {
    web_sys::window()?.location().pathname().ok()
}

fn sync_location_to_panel(panel: Panel) {
    let base = panel.path();
    // Treat any path already under this panel's base as "already here" so a
    // panel-internal sub-route (e.g. the calendar's `/app/calendar/month`) is
    // left intact rather than clobbered back to the bare panel path.
    if let Some(current) = current_frontend_path() {
        if current == base || current.starts_with(&format!("{base}/")) {
            return;
        }
    }
    if let Some(window) = web_sys::window() {
        if let Ok(history) = window.history() {
            let _ = history.push_state_with_url(&JsValue::NULL, "", Some(base));
        }
    }
}

fn should_intercept_link_click(ev: &MouseEvent) -> bool {
    ev.button() == 0 && !ev.alt_key() && !ev.ctrl_key() && !ev.meta_key() && !ev.shift_key()
}

/// A single nav entry: a real link that selects `panel` in-app for ordinary
/// clicks and still supports copy/open-in-new-tab browser behavior. Selecting
/// a panel also closes the mobile nav drawer (`nav_open`); on desktop the nav
/// is a static column and the signal is inert.
#[component]
fn NavItem(panel: Panel, active: RwSignal<Panel>, nav_open: RwSignal<bool>) -> impl IntoView {
    let is_active = move || active.get() == panel;
    let enabled = panel.enabled();
    let class = move || {
        let mut c = String::from("nav-item");
        if is_active() {
            c.push_str(" nav-item-active");
        }
        if !enabled {
            c.push_str(" nav-item-disabled");
        }
        c
    };
    let href = panel.href();
    view! {
        <li>
            <a
                class=class
                href=href
                aria-disabled=(!enabled).to_string()
                on:click=move |ev: MouseEvent| {
                    if !enabled {
                        ev.prevent_default();
                        return;
                    }
                    let same_window = should_intercept_link_click(&ev);
                    if same_window {
                        ev.prevent_default();
                        sync_location_to_panel(panel);
                        active.set(panel);
                        nav_open.set(false);
                    }
                }
            >
                <span class="nav-item-label">
                    <Icon icon=panel.icon() />
                    <span>{panel.label()}</span>
                </span>
                <Show when=move || !enabled fallback=|| ().into_view()>
                    <span class="nav-soon">"soon"</span>
                </Show>
            </a>
        </li>
    }
}

/// The Apps nav entry plus its pinned-apps quick menu (SOUL §12): when any
/// apps are pinned a "▸" affordance appears, and hovering the entry (desktop)
/// or tapping the affordance (touch — the drawer has no hover) opens a flyout
/// of the pinned apps. Picking one lands on the Apps panel with that app open,
/// via the one-shot `app_target` signal the [`AppsPanel`] consumes (the same
/// handoff pattern as History's resume-in-Chat).
#[component]
fn AppsNavItem(
    active: RwSignal<Panel>,
    nav_open: RwSignal<bool>,
    pins: RwSignal<Vec<PinnedApp>>,
    app_target: RwSignal<Option<String>>,
) -> impl IntoView {
    let panel = Panel::Apps;
    // Click-toggled flyout state for pointers without hover; on desktop the
    // flyout also opens on plain :hover (see the `.nav-apps` CSS).
    let flyout_open = RwSignal::new(false);
    let li_class = move || {
        if flyout_open.get() {
            "nav-apps nav-apps-open"
        } else {
            "nav-apps"
        }
    };
    let link_class = move || {
        if active.get() == panel {
            "nav-item nav-item-active"
        } else {
            "nav-item"
        }
    };
    let has_pins = move || pins.with(|p| !p.is_empty());
    let rows = move || {
        pins.get()
            .into_iter()
            .map(|pin| {
                let id = pin.id.clone();
                // The full title doubles as the tooltip (rows ellipsize).
                let tooltip = pin.title.clone();
                view! {
                    <li>
                        <button
                            class="nav-apps-pin"
                            title=tooltip
                            on:click=move |_| {
                                app_target.set(Some(id.clone()));
                                // Only switch when not already on Apps: re-setting
                                // `active` remounts the panel, and the remount
                                // races away the one-shot target before the
                                // mounted panel's consumer effect runs. When
                                // already there, that effect applies the target
                                // in place.
                                if active.get_untracked() != panel {
                                    sync_location_to_panel(panel);
                                    active.set(panel);
                                }
                                flyout_open.set(false);
                                nav_open.set(false);
                            }
                        >
                            {pin.title}
                        </button>
                    </li>
                }
            })
            .collect::<Vec<_>>()
    };
    view! {
        <li class=li_class>
            <div class="nav-apps-row">
                <a
                    class=link_class
                    href=panel.href()
                    on:click=move |ev: MouseEvent| {
                        if should_intercept_link_click(&ev) {
                            ev.prevent_default();
                            sync_location_to_panel(panel);
                            active.set(panel);
                            flyout_open.set(false);
                            nav_open.set(false);
                        }
                    }
                >
                    <span class="nav-item-label">
                        <Icon icon=panel.icon() />
                        <span>{panel.label()}</span>
                    </span>
                </a>
                <Show when=has_pins fallback=|| ().into_view()>
                    <button
                        class="nav-apps-toggle"
                        title="Pinned apps"
                        aria-label="Pinned apps"
                        aria-expanded=move || flyout_open.get().to_string()
                        on:click=move |_| flyout_open.update(|o| *o = !*o)
                    >
                        <span class="nav-chevron"><Icon icon=MdIcon::ChevronRight /></span>
                    </button>
                </Show>
            </div>
            <Show when=has_pins fallback=|| ().into_view()>
                <ul class="nav-apps-flyout">{rows}</ul>
            </Show>
        </li>
    }
}

/// The root workbench shell: header, left nav, and the active panel.
#[component]
pub fn Workbench() -> impl IntoView {
    // The app-wide confirm/prompt dialog service (SOUL §12). Provided here so
    // every panel (and the workspace switcher in the header) reaches it via
    // `use_dialogs()`; rendered once by `<DialogHost/>` at the shell root.
    let dialogs = Dialogs::new();
    provide_context(dialogs);
    // The currently selected panel, derived from the frontend URL.
    let active = RwSignal::new(panel_from_location());
    {
        let on_hash_change = Closure::<dyn FnMut(web_sys::Event)>::wrap(Box::new(move |_| {
            active.set(panel_from_location());
        }));
        if let Some(window) = web_sys::window() {
            let _ = window.add_event_listener_with_callback(
                "popstate",
                on_hash_change.as_ref().unchecked_ref(),
            );
            let _ = window.add_event_listener_with_callback(
                "hashchange",
                on_hash_change.as_ref().unchecked_ref(),
            );
            on_hash_change.forget();
        }
    }
    Effect::new(move |_| sync_location_to_panel(active.get()));
    // First run: if the user hasn't finished the quick-start, bring it forward so
    // the wizard greets a fresh account. Best-effort — a failed/again-completed
    // probe just leaves Chat selected.
    {
        let token = auth::resolve_token();
        spawn_local(async move {
            if let Ok(st) = rest::get_onboarding_state(token.as_deref()).await {
                if !st.completed {
                    active.set(Panel::Onboarding);
                }
            }
        });
    }
    // A one-shot "resume this conversation in the live Chat panel" request: the
    // History panel sets it (its "Resume in Chat" button), the Chat panel reads
    // and clears it as it mounts. Lives here (not in a panel) so it survives the
    // panel switch that re-mounts both panels.
    let resume_target = RwSignal::new(Option::<String>::None);
    // When History asks to resume a thread, bring the Chat panel forward; the
    // Chat panel consumes the target itself on mount.
    Effect::new(move |_| {
        if resume_target.with(Option::is_some) {
            active.set(Panel::Chat);
        }
    });
    // The current workspace's pinned emerged apps, backing the nav quick menu
    // and shared with the Apps panel (which toggles pins and reconciles them
    // against the live app list). Seeded from localStorage so the flyout
    // renders without a fetch; when any pins exist, one background list fetch
    // scopes them to this workspace and refreshes stale titles (pins are
    // per-browser, apps per-workspace).
    let pinned_apps = RwSignal::new(pins::load_all());
    if pinned_apps.with_untracked(|p| !p.is_empty()) {
        let token = auth::resolve_token();
        spawn_local(async move {
            if let Ok(list) = rest::list_uis(token.as_deref()).await {
                let list: Vec<_> = list
                    .into_iter()
                    .filter(|app| app.definition.parent_app.is_none())
                    .collect();
                pinned_apps.set(pins::reconcile_workspace(&list));
            }
        });
    }
    // A one-shot "open this app in the Apps panel" request from the nav quick
    // menu; the Apps panel consumes it (mirrors `resume_target`).
    let app_target = RwSignal::new(Option::<String>::None);
    // Whether the collapsible "More" section is expanded.
    let more_open = RwSignal::new(false);
    // Whether the Settings dialog (email reading setup) is open.
    let settings_open = RwSignal::new(false);
    // Whether the mobile nav drawer is open. Only meaningful on narrow
    // viewports where CSS turns .wb-nav into an off-canvas drawer; the desktop
    // nav ignores the class entirely.
    let nav_open = RwSignal::new(false);
    let chevron_class = move || {
        if more_open.get() {
            String::from("nav-chevron nav-chevron-open")
        } else {
            String::from("nav-chevron")
        }
    };

    view! {
        <div class="workbench">
            <header class="wb-header">
                <button
                    class="wb-menu-btn"
                    title="Menu"
                    aria-label="Open navigation"
                    on:click=move |_| nav_open.update(|o| *o = !*o)
                >
                    <Icon icon=MdIcon::Menu />
                </button>
                <span class="wb-title">"catalerum"</span>
                <span class="wb-subtitle">"a catalogue of things"</span>
                <div class="wb-header-spacer"></div>
                <WorkspaceSwitcher />
                <button
                    class="wb-settings-btn"
                    title="Settings"
                    on:click=move |_| settings_open.set(true)
                >
                    <Icon icon=MdIcon::Settings />
                </button>
            </header>

            <div class="wb-body">
                <button
                    class="wb-nav-scrim"
                    class:wb-nav-scrim-open=move || nav_open.get()
                    aria-label="Close navigation"
                    tabindex="-1"
                    on:click=move |_| nav_open.set(false)
                ></button>
                <nav class="wb-nav" class:wb-nav-open=move || nav_open.get()>
                    <ul>
                        {Panel::pinned()
                            .into_iter()
                            .map(|panel| view! { <NavItem panel active nav_open /> })
                            .collect::<Vec<_>>()}
                    </ul>
                    <button
                        class="nav-section-toggle"
                        on:click=move |_| more_open.update(|o| *o = !*o)
                    >
                        <span>"More"</span>
                        <span class=chevron_class><Icon icon=MdIcon::ChevronRight /></span>
                    </button>
                    <Show when=move || more_open.get() fallback=|| ().into_view()>
                        <ul class="nav-collapsed">
                            {Panel::collapsed()
                                .into_iter()
                                .map(|panel| match panel {
                                    Panel::Apps => {
                                        view! {
                                            <AppsNavItem
                                                active
                                                nav_open
                                                pins=pinned_apps
                                                app_target
                                            />
                                        }
                                            .into_any()
                                    }
                                    _ => view! { <NavItem panel active nav_open /> }.into_any(),
                                })
                                .collect::<Vec<_>>()}
                        </ul>
                    </Show>
                </nav>

                <main class="wb-main">
                    {move || match active.get() {
                        Panel::Chat => view! { <ChatPanel resume=resume_target /> }.into_any(),
                        Panel::Calendar => view! { <CalendarPanel /> }.into_any(),
                        Panel::Files => view! { <FilesPanel /> }.into_any(),
                        Panel::Notes => view! { <NotesPanel /> }.into_any(),
                        Panel::Skills => view! { <SkillsPanel /> }.into_any(),
                        Panel::Profiles => view! { <ProfilesPanel /> }.into_any(),
                        Panel::Automations => view! { <AutomationsPanel /> }.into_any(),
                        Panel::Grants => view! { <GrantsPanel /> }.into_any(),
                        Panel::McpEndpoints => view! { <McpEndpointsPanel /> }.into_any(),
                        Panel::History => {
                            view! { <ConversationsPanel resume=resume_target /> }.into_any()
                        }
                        Panel::Email => view! { <EmailPanel /> }.into_any(),
                        Panel::Fetch => view! { <FetchPanel /> }.into_any(),
                        Panel::Tasks => view! { <TasksPanel /> }.into_any(),
                        Panel::Memory => view! { <MemoryPanel /> }.into_any(),
                        Panel::Graph => view! { <GraphPanel /> }.into_any(),
                        Panel::Apps => {
                            view! { <AppsPanel pins=pinned_apps target=app_target /> }.into_any()
                        }
                        Panel::Onboarding => view! { <OnboardingPanel active /> }.into_any(),
                    }}
                </main>
            </div>

            <SettingsDialog open=settings_open />
            <DialogHost />
        </div>
    }
}

/// The unauthenticated sign-in surface (SOUL §12/§18). Shown by [`crate::App`]
/// when no session token resolves — the app otherwise renders panels that would
/// only 401.
///
/// It offers the SSO login (when the instance advertises it) and always names the
/// dev magic-link fallback. The probe reads the anonymous `GET /status/login`
/// slice (the authed `GET /status` 401s here); the button stays hidden while the
/// probe is in flight (see [`auth::show_sso_button`] — rendering it early made it
/// flash and vanish on dev instances), but a *failed* probe resolves to shown so
/// it can never strand an SSO-only deployment.
#[component]
pub fn LoginView() -> impl IntoView {
    // Surface a failed SSO callback: read + scrub the `?sso_error=` the API bounced
    // us back with, mapped to a friendly banner (unknown codes → generic message,
    // never the raw param). Runs once on mount, like `adopt_url_token`.
    let sso_error = auth::take_sso_error_message().map(|msg| {
        view! {
            <p class="wb-login-error" role="alert">
                {msg}
            </p>
        }
    });
    let sso_known = RwSignal::new(Option::<bool>::None);
    let sso_login_url = RwSignal::new(Option::<String>::None);
    let password_enabled = RwSignal::new(false);
    let setup_required = RwSignal::new(false);
    let email = RwSignal::new(String::new());
    let display_name = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let password_error = RwSignal::new(Option::<String>::None);
    let submitting = RwSignal::new(false);
    spawn_local(async move {
        // Probe failure → treat SSO as on (the button merely 404s on a non-SSO
        // instance; hiding it would strand an SSO-only deployment).
        let (sso, login_url) = rest::get_login_status()
            .await
            .map_or((true, None), |status| (status.sso, status.sso_login_url));
        sso_login_url.set(login_url);
        sso_known.set(Some(sso));
    });
    spawn_local(async move {
        if let Ok(status) = rest::get_setup_status().await {
            password_enabled.set(status.enabled);
            setup_required.set(status.required);
        }
    });
    // The login endpoint lives on the API origin (a relative href would only hit
    // the SPA's static server): the server-advertised URL when config pins one,
    // else derived from `api_base()`. Carry the current path so the IdP
    // round-trip returns the user here — except API-route paths a broken link
    // parked us on, which fold to `/`.
    let redirect_path = auth::sanitize_spa_redirect(&auth::current_relative_path()).to_string();
    let sso_button = move || {
        auth::show_sso_button(sso_known.get()).then(|| {
            let href = auth::sso_login_href(
                &crate::api::api_base(),
                sso_login_url.get().as_deref(),
                &redirect_path,
            );
            view! {
                <a class="wb-login-sso" href=href>
                    "Sign in with SSO"
                </a>
            }
        })
    };
    let submit_password = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if submitting.get_untracked() {
            return;
        }
        submitting.set(true);
        password_error.set(None);
        let request_email = email.get_untracked();
        let request_password = password.get_untracked();
        let request_name = display_name.get_untracked();
        spawn_local(async move {
            let result = if setup_required.get_untracked() {
                rest::setup_account(&crate::api::SetupAccount {
                    email: request_email,
                    display_name: request_name,
                    password: request_password,
                })
                .await
            } else {
                rest::password_login(&crate::api::PasswordLogin {
                    email: request_email,
                    password: request_password,
                })
                .await
            };
            match result {
                Ok(session) => auth::adopt_token_and_reload(&session.token),
                Err(error) => {
                    password_error.set(Some(error.to_string()));
                    submitting.set(false);
                }
            }
        });
    };

    view! {
        <div class="wb-login">
            <div class="wb-login-card">
                <div class="wb-login-brand">
                    <span class="wb-title">"catalerum"</span>
                    <span class="wb-subtitle">"a catalogue of things"</span>
                </div>
                {sso_error}
                {sso_button}
                {move || password_enabled.get().then(|| view! {
                    <form class="wb-login-form" on:submit=submit_password>
                        <h2>{move || if setup_required.get() { "Create the instance owner" } else { "Sign in" }}</h2>
                        {move || setup_required.get().then(|| view! {
                            <label>"Display name"</label>
                            <input required prop:value=move || display_name.get()
                                on:input=move |ev| display_name.set(event_target_value(&ev)) />
                        })}
                        <label>"Email"</label>
                        <input type="email" required autocomplete="username"
                            prop:value=move || email.get()
                            on:input=move |ev| email.set(event_target_value(&ev)) />
                        <label>"Password"</label>
                        <input type="password" required minlength="12"
                            autocomplete=move || if setup_required.get() { "new-password" } else { "current-password" }
                            prop:value=move || password.get()
                            on:input=move |ev| password.set(event_target_value(&ev)) />
                        {move || password_error.get().map(|error| view! {
                            <p class="wb-login-error" role="alert">{error}</p>
                        })}
                        <button class="wb-login-sso" type="submit" disabled=move || submitting.get()>
                            {move || if setup_required.get() { "Create owner" } else { "Sign in" }}
                        </button>
                    </form>
                })}
                {move || (!password_enabled.get()).then(|| view! {
                    <p class="wb-login-hint">
                        "Dev instances sign in with the magic-link URL printed at startup."
                    </p>
                })}
            </div>
        </div>
    }
}
