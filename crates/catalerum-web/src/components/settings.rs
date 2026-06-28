//! The workbench **Settings** dialog (SOUL §12).
//!
//! A modal, opened from the gear button in the shell header, with a left tab
//! rail and a content pane. Tabs:
//! - **About** — product identity + version.
//! - **Appearance** — pick the workbench colour theme (incl. high contrast).
//! - **General** — user preferences stored on the profile (currently the
//!   timezone used to interpret/display dates & times).
//! - **LLM gateway** — the configured llmleaf/OpenRouter connection (SOUL §6.1).
//! - **Status** — live health of the LLM gateway + backing datastores.
//! - **API keys** — issue / list / revoke workspace bearer tokens (SOUL §18).
//! - **MCP servers** — register the external MCP servers this workspace connects
//!   *out* to as a client (SOUL §26): stdio (spawn a command) or http (a URL with
//!   optional auth); each enabled server's tools fold into the agent's tool set.
//!   Workspace-admin gated, so it collapses for a non-admin member.
//! - **MCP clients** — copy-paste config for using this workspace from external
//!   MCP products (Claude Code, Codex, Cursor, …) — see
//!   [`crate::components::mcp_connect`].
//!
//! Email ingest is **not** configured here (SOUL §28): an email source is set up
//! in a `CollectEmail` automation node, not a global settings tab.
//!
//! Read-only where the backend is read-only: it never surfaces a secret (gateway
//! key is masked; a freshly-minted token is shown once).

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::{JsCast, JsValue};

use crate::api::{
    ComputerAgentView, CreateToken, EnrollComputerAgent, Grant, LlmInfo, LlmSettings, McpAuthBody,
    McpServerBody, McpServerView, ModelInfo, SearchProviderInfo, SearchSettings, ServiceStatus,
    StatusInfo, StorageSettings, StorageStore, TokenView, VoiceInfo, WorkspaceMembership,
};
use crate::auth;
use crate::components::dialogs::{use_dialogs, ConfirmSpec};
use crate::components::icons::{Icon, MdIcon};
use crate::components::mcp_connect::McpConnectSection;
use crate::components::theme::ThemePicker;
use crate::components::widgets::{copy_button, model_autocomplete, model_options, voice_options};
use crate::components::workspace::is_multi_user;
use crate::rest;

/// The sections of the settings dialog, in tab-rail order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsTab {
    Appearance,
    General,
    Llm,
    Llmleaf,
    Models,
    Search,
    Storage,
    Status,
    ApiKeys,
    McpServers,
    McpClients,
    ComputerAgents,
    Users,
    About,
}

impl SettingsTab {
    /// Tab-rail order.
    fn all() -> [SettingsTab; 14] {
        [
            SettingsTab::Appearance,
            SettingsTab::General,
            SettingsTab::Llm,
            SettingsTab::Llmleaf,
            SettingsTab::Models,
            SettingsTab::Search,
            SettingsTab::Storage,
            SettingsTab::Status,
            SettingsTab::ApiKeys,
            SettingsTab::McpServers,
            SettingsTab::McpClients,
            SettingsTab::ComputerAgents,
            SettingsTab::Users,
            SettingsTab::About,
        ]
    }

    /// The rail label.
    fn label(self) -> &'static str {
        match self {
            SettingsTab::About => "About",
            SettingsTab::Appearance => "Appearance",
            SettingsTab::General => "General",
            SettingsTab::Llm => "LLM gateway",
            SettingsTab::Llmleaf => "LLM providers",
            SettingsTab::Models => "Models & voices",
            SettingsTab::Search => "Web search",
            SettingsTab::Storage => "Files",
            SettingsTab::Status => "Status",
            SettingsTab::ApiKeys => "API keys",
            SettingsTab::McpServers => "MCP servers",
            SettingsTab::McpClients => "MCP clients",
            SettingsTab::ComputerAgents => "Computer agents",
            SettingsTab::Users => "Users",
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-user settings split (SOUL §18/§29) — pure, presentation-only helpers.
//
// The deployment `mode` shapes only how much the web *shows*; the server still
// enforces every write by workspace role in both modes. A tab is either a
// **per-user preference / info** surface (every member keeps it, every mode) or
// **admin chrome** — the workspace-operational config *views* (LLM gateway +
// datastore Status) and the dangerous **API keys** token panel — which
// `multi_user` collapses away for a non-admin member ("curated defaults, fewer
// knobs", §18). Admins/owners and every `single_user` user see full depth.
// ---------------------------------------------------------------------------

/// Whether `tab` is **admin chrome** (a workspace-operational config view/write or
/// the dangerous token panel) that collapses for a non-admin member in
/// `multi_user`. Per-user preference + info tabs (About / Appearance / General /
/// Models / Search / Storage / MCP clients) are never chrome — members keep them
/// in every mode. **MCP servers** *is* chrome: registering an external MCP server
/// is a workspace-operational config write (workspace-shared credentials +
/// workspace-wide tools), admin-gated server-side, so a non-admin member has
/// nothing to do there. MCP clients, by contrast, stays per-user even though it
/// can mint a token: connecting one's own agents is the §26 per-user feature (and
/// `POST /tokens` is self-scoped server-side); what collapses is the raw
/// token-management *panel*, not the ability.
fn is_admin_chrome(tab: SettingsTab) -> bool {
    matches!(
        tab,
        SettingsTab::Llm
            | SettingsTab::Llmleaf
            | SettingsTab::Status
            | SettingsTab::ApiKeys
            | SettingsTab::McpServers
            | SettingsTab::ComputerAgents
            | SettingsTab::Users
    )
}

/// Whether a workspace role token is a **known** non-admin (member/viewer) — the
/// only case that collapses. An empty/unknown role is *not* treated as
/// known-non-admin, so an undetectable role shows full depth (the server remains
/// the sole authority on every write — over-showing a read-only view is harmless,
/// wrongly hiding one from an admin is not).
fn is_non_admin_member(role: &str) -> bool {
    matches!(role.trim(), "member" | "viewer")
}

/// The settings tabs to show, given the deployment `mode` and the caller's
/// workspace `role` token — the executable §29 answer (SOUL §18/§29):
///
/// - `single_user` (the sole-human default) → **full depth** for everyone.
/// - Admin / owner → **full depth**, in either mode.
/// - `multi_user` **non-admin member** (member/viewer) → the **curated subset**:
///   per-user preferences + info only (About, Appearance, General, Models,
///   Search, Storage); the admin-chrome tabs collapse away.
///
/// Presentation only — never authorization; the server re-checks every write.
fn settings_tabs_for(mode: &str, role: &str, llm_control_plane: bool) -> Vec<SettingsTab> {
    let available = SettingsTab::all()
        .into_iter()
        .filter(|tab| *tab != SettingsTab::Llmleaf || llm_control_plane);
    if is_multi_user(mode) && is_non_admin_member(role) {
        available.filter(|t| !is_admin_chrome(*t)).collect()
    } else {
        available.collect()
    }
}

/// The caller's role in the **active** workspace, read from the `/workspaces`
/// membership listing (the membership flagged `active`) — how the web learns the
/// current workspace role. Empty when no active membership resolves (an older
/// server / a transient error), which [`settings_tabs_for`] treats as full depth.
fn active_workspace_role(memberships: &[WorkspaceMembership]) -> String {
    memberships
        .iter()
        .find(|m| m.active)
        .map(|m| m.role.clone())
        .unwrap_or_default()
}

/// The settings modal. Renders only when `open` is `true`; the gear button in the
/// shell header flips that signal. A left tab rail switches the content pane —
/// each section component mounts fresh when selected, so it loads current data.
#[component]
pub fn SettingsDialog(open: RwSignal<bool>) -> impl IntoView {
    let tab = RwSignal::new(SettingsTab::Appearance);
    let close = move || open.set(false);

    // Deployment mode + the caller's active-workspace role drive the multi-user
    // tab collapse (SOUL §18/§29). Loaded when the dialog opens (presentation
    // only — the server enforces every write regardless). Until role/mode load,
    // role-gated tabs remain visible; explicit deployment capabilities such as
    // the llmleaf control plane fail closed until the status response opts in.
    let mode = RwSignal::new(String::new());
    let role = RwSignal::new(String::new());
    // Capabilities fail closed: until the status response explicitly opts this
    // deployment in, the topology editor is absent from the settings rail.
    let llm_control_plane = RwSignal::new(false);
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(st) = rest::get_status(token.as_deref()).await {
                mode.set(st.mode);
                llm_control_plane.set(st.llm_control_plane);
            }
            if let Ok(list) = rest::list_workspaces(token.as_deref()).await {
                role.set(active_workspace_role(&list));
            }
        });
    });
    let visible_tabs = Signal::derive(move || {
        settings_tabs_for(&mode.get(), &role.get(), llm_control_plane.get())
    });
    // Keep the selection valid: if the active tab collapses away (a non-admin
    // member in multi_user), fall back to the always-present Appearance tab.
    Effect::new(move |_| {
        let tabs = visible_tabs.get();
        if !tabs.contains(&tab.get_untracked()) {
            tab.set(SettingsTab::Appearance);
        }
    });

    view! {
        <Show when=move || open.get() fallback=|| ().into_view()>
            // Backdrop: a click outside the modal closes it.
            <div class="settings-overlay" on:click=move |_| close()>
                <div
                    class="settings-modal"
                    // Swallow clicks inside so they don't bubble to the backdrop.
                    on:click=move |ev| ev.stop_propagation()
                >
                    <header class="settings-header">
                        <div class="settings-header-titles">
                            <h2 class="settings-title">"Settings"</h2>
                            <span class="settings-subtitle">"catalerum workbench"</span>
                        </div>
                        <button class="settings-close" title="Close" on:click=move |_| close()>
                            <Icon icon=MdIcon::Close />
                        </button>
                    </header>

                    <div class="settings-layout">
                        <nav class="settings-tabs">
                            {move || {
                                visible_tabs
                                    .get()
                                    .into_iter()
                                    .map(|t| {
                                        let active = move || tab.get() == t;
                                        view! {
                                            <button
                                                class="settings-tab"
                                                class:settings-tab-active=active
                                                on:click=move |_| tab.set(t)
                                            >
                                                {t.label()}
                                            </button>
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }}
                        </nav>

                        <div class="settings-content">
                            {move || match tab.get() {
                                SettingsTab::About => view! { <AboutSection /> }.into_any(),
                                SettingsTab::Appearance => {
                                    view! { <AppearanceSection /> }.into_any()
                                }
                                SettingsTab::General => view! { <GeneralSection /> }.into_any(),
                                SettingsTab::Llm => view! { <LlmSection /> }.into_any(),
                                SettingsTab::Llmleaf => view! { <LlmleafSection /> }.into_any(),
                                SettingsTab::Models => view! { <ModelsSection /> }.into_any(),
                                SettingsTab::Search => view! { <SearchSection /> }.into_any(),
                                SettingsTab::Storage => view! { <StorageSection /> }.into_any(),
                                SettingsTab::Status => view! { <StatusSection /> }.into_any(),
                                SettingsTab::ApiKeys => view! { <ApiKeysSection /> }.into_any(),
                                SettingsTab::McpServers => {
                                    view! { <McpServersSection /> }.into_any()
                                }
                                SettingsTab::McpClients => {
                                    view! { <McpConnectSection /> }.into_any()
                                }
                                SettingsTab::ComputerAgents => {
                                    view! { <ComputerAgentsSection /> }.into_any()
                                }
                                SettingsTab::Users => view! { <UsersSection /> }.into_any(),
                            }}
                        </div>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// **Appearance** — pick the workbench colour theme. Selection applies live and
/// is cached in `localStorage`; the choice (including the high-contrast theme)
/// follows the user across reloads. See [`crate::components::theme`].
#[component]
fn AppearanceSection() -> impl IntoView {
    view! {
        <section class="settings-section">
            <h3 class="settings-section-title">"Theme"</h3>
            <p class="appearance-intro">
                "Choose how the workbench looks. Your pick applies instantly and is "
                "remembered on this device. The high-contrast theme maximises legibility "
                "for low-vision use; pick \"Custom\" to build your own palette and "
                "import or export it as JSON."
            </p>
            <ThemePicker />
        </section>
    }
}

/// The IANA timezone names for the picker, sourced live from the browser via
/// `Intl.supportedValuesOf('timeZone')` — always current with the platform's zone
/// database and free of any wasm bundle bloat. Falls back to a compact built-in
/// list on the rare (pre-2022) engine without that API; `UTC` is always present.
fn timezone_names() -> Vec<String> {
    fn from_intl() -> Option<Vec<String>> {
        let intl = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Intl")).ok()?;
        let func: js_sys::Function =
            js_sys::Reflect::get(&intl, &JsValue::from_str("supportedValuesOf"))
                .ok()?
                .dyn_into()
                .ok()?;
        let arr: js_sys::Array = func
            .call1(&intl, &JsValue::from_str("timeZone"))
            .ok()?
            .dyn_into()
            .ok()?;
        let out: Vec<String> = arr.iter().filter_map(|v| v.as_string()).collect();
        (!out.is_empty()).then_some(out)
    }
    let mut names = from_intl().unwrap_or_else(|| {
        FALLBACK_TIMEZONES
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    });
    if !names.iter().any(|n| n == "UTC") {
        names.insert(0, "UTC".to_string());
    }
    names
}

/// The browser's best guess at the user's timezone
/// (`Intl.DateTimeFormat().resolvedOptions().timeZone`) — used as the picker's
/// placeholder so a blank field still hints the auto-detected zone.
fn detected_timezone() -> Option<String> {
    let intl = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("Intl")).ok()?;
    let ctor: js_sys::Function = js_sys::Reflect::get(&intl, &JsValue::from_str("DateTimeFormat"))
        .ok()?
        .dyn_into()
        .ok()?;
    let dtf = js_sys::Reflect::construct(&ctor, &js_sys::Array::new()).ok()?;
    let resolved: js_sys::Function =
        js_sys::Reflect::get(&dtf, &JsValue::from_str("resolvedOptions"))
            .ok()?
            .dyn_into()
            .ok()?;
    let opts = resolved.call0(&dtf).ok()?;
    js_sys::Reflect::get(&opts, &JsValue::from_str("timeZone"))
        .ok()?
        .as_string()
}

/// A floor of common IANA zones for engines lacking `Intl.supportedValuesOf`; the
/// live browser list is preferred whenever it is available.
const FALLBACK_TIMEZONES: &[&str] = &[
    "UTC",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Paris",
    "Europe/Madrid",
    "Europe/Rome",
    "Europe/Moscow",
    "Africa/Cairo",
    "Africa/Johannesburg",
    "Asia/Dubai",
    "Asia/Kolkata",
    "Asia/Bangkok",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Australia/Sydney",
    "Pacific/Auckland",
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "America/Sao_Paulo",
    "America/Mexico_City",
];

/// **General** — user-level preferences kept on the profile. Currently the
/// timezone used to interpret and display dates & times (the calendar's day/week
/// grid, scheduling): a free-text autocomplete over the browser's IANA zone list,
/// the same widget the model pickers use. Saved by *merging* `{ "timezone": … }`
/// into the profile's `fields` — so it never clobbers the other profile fields and
/// the assistant sees it too (SOUL §22). A blank value falls back to the browser's
/// detected zone.
#[component]
fn GeneralSection() -> impl IntoView {
    let timezone = RwSignal::new(String::new());
    let loading = RwSignal::new(true);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    // The zone catalog and the auto-detected zone (placeholder) — both static, so
    // read once on mount rather than reactively.
    let zones = RwSignal::new(
        timezone_names()
            .into_iter()
            .map(|z| (z.clone(), z))
            .collect::<Vec<_>>(),
    );
    let detected = detected_timezone().unwrap_or_default();

    // Seed from the saved profile.
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::get_profile(token.as_deref()).await {
            Ok(p) => {
                let tz = p
                    .fields
                    .get("timezone")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                timezone.set(tz.to_string());
            }
            Err(e) => error.set(Some(e.to_string())),
        }
        loading.set(false);
    });

    let placeholder = {
        let detected = detected.clone();
        Signal::derive(move || {
            if detected.is_empty() {
                "e.g. Europe/Berlin".to_string()
            } else {
                format!("detected: {detected}")
            }
        })
    };

    let save = move || {
        saving.set(true);
        error.set(None);
        notice.set(None);
        let tz = timezone.get_untracked();
        let body = serde_json::json!({ "timezone": tz.trim() });
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::update_profile(token.as_deref(), &body).await {
                Ok(p) => {
                    // Re-sync from the server's stored record.
                    let saved = p
                        .fields
                        .get("timezone")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    timezone.set(saved.to_string());
                    notice.set(Some("Saved.".to_string()));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            saving.set(false);
        });
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Your timezone is used to interpret and display dates and times — the calendar's "
                "day / week grid and scheduling. Type to search; leave it blank to fall back to the "
                "timezone your browser reports."
            </p>

            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>

            <div class="settings-field">
                <label class="settings-label">"Timezone"</label>
                {model_autocomplete(
                    Signal::derive(move || timezone.get()),
                    move |v| timezone.set(v),
                    Signal::derive(move || zones.get()),
                    placeholder,
                    Signal::derive(|| false),
                    "settings-input",
                )}
            </div>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="settings-actions">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || saving.get() || loading.get()
                    on:click=move |_| save()
                >
                    {move || if saving.get() { "Saving…" } else { "Save" }}
                </button>
            </div>
        </section>
    }
}

#[component]
fn AboutSection() -> impl IntoView {
    // The web crate's package version == the workspace version.
    let version = env!("CARGO_PKG_VERSION");
    view! {
        <section class="settings-section">
            <div class="about-hero">
                <div class="about-mark">"catalerum"</div>
                <div class="about-tagline">"a catalogue of things"</div>
                <div class="about-version">{format!("version {version}")}</div>
            </div>
            <p class="settings-blurb">
                "A self-hostable, fully-integrated LLM assistant. It ingests your calendars, "
                "storage, and mail, keeps your notes, tasks, memories, and profile, and maintains a "
                "queryable model of your world that an LLM can search and act on through typed, "
                "capability-scoped tools — from the web, a messenger, or another agent over MCP."
            </p>
            <ul class="about-facts">
                <li>
                    <span class="about-fact-k">"Source of truth"</span>
                    <span class="about-fact-v">"Postgres"</span>
                </li>
                <li>
                    <span class="about-fact-k">"Derived stores"</span>
                    <span class="about-fact-v">"Neo4j (graph) · Qdrant (vectors) · Valkey (coordination)"</span>
                </li>
                <li>
                    <span class="about-fact-k">"Licence"</span>
                    <span class="about-fact-v">"MIT OR Apache-2.0"</span>
                </li>
            </ul>
            <details class="about-licenses">
                <summary>"Open source licenses"</summary>
                <p class="settings-blurb">
                    "catalerum is dual-licensed under MIT or Apache-2.0, at your option. It builds "
                    "on open-source components, each under its own licence:"
                </p>
                <ul class="license-list">
                    <li>
                        <span class="license-k">"Leptos · Tokio · Axum · Tower · Hyper"</span>
                        <span class="license-v">"MIT"</span>
                    </li>
                    <li>
                        <span class="license-k">"SQLx · Serde · wasm-bindgen"</span>
                        <span class="license-v">"MIT / Apache-2.0"</span>
                    </li>
                    <li>
                        <span class="license-k">"Boa (JS engine for code nodes)"</span>
                        <span class="license-v">"MIT / Unlicense"</span>
                    </li>
                    <li>
                        <span class="license-k">"PostgreSQL"</span>
                        <span class="license-v">"PostgreSQL License"</span>
                    </li>
                    <li>
                        <span class="license-k">"Neo4j Community"</span>
                        <span class="license-v">"GPL-3.0"</span>
                    </li>
                    <li>
                        <span class="license-k">"Qdrant"</span>
                        <span class="license-v">"Apache-2.0"</span>
                    </li>
                    <li>
                        <span class="license-k">"Valkey"</span>
                        <span class="license-v">"BSD-3-Clause"</span>
                    </li>
                </ul>
                <p class="settings-blurb license-note">
                    "The full Rust dependency tree carries its own licences (mostly MIT / "
                    "Apache-2.0)."
                </p>
            </details>
        </section>
    }
}

/// One row in the route builder. Signals keep each fallback target independently
/// editable while the outer keyed list changes only when rows are added/removed.
#[derive(Clone, Copy)]
struct LlmleafTargetRow {
    id: usize,
    provider: RwSignal<String>,
    model: RwSignal<String>,
}

/// Translate the guided provider form into llmleaf's wire shape.
fn provider_topology_spec(
    name: &str,
    provider_kind: &str,
    credential_env: &str,
    endpoint: &str,
    prefix: &str,
) -> Result<serde_json::Value, String> {
    let name = name.trim();
    let provider_kind = provider_kind.trim();
    if name.is_empty() {
        return Err("Give this provider a name.".into());
    }
    if provider_kind.is_empty() {
        return Err("Choose a provider type.".into());
    }
    let mut spec = serde_json::Map::new();
    spec.insert("name".into(), serde_json::Value::String(name.into()));
    spec.insert(
        "kind".into(),
        serde_json::Value::String(provider_kind.into()),
    );
    let credential_env = credential_env
        .trim()
        .strip_prefix("env:")
        .unwrap_or(credential_env.trim());
    if !credential_env.is_empty() {
        spec.insert(
            "credential".into(),
            serde_json::Value::String(format!("env:{credential_env}")),
        );
    }
    if !endpoint.trim().is_empty() {
        spec.insert(
            "endpoint".into(),
            serde_json::Value::String(endpoint.trim().into()),
        );
    }
    if !prefix.trim().is_empty() {
        spec.insert(
            "prefix".into(),
            serde_json::Value::String(prefix.trim().into()),
        );
    }
    Ok(serde_json::Value::Object(spec))
}

/// Translate the ordered route rows into llmleaf's fallback-chain wire shape.
fn route_topology_spec(
    model: &str,
    targets: Vec<(String, String)>,
) -> Result<serde_json::Value, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Give this route a model name.".into());
    }
    let mut built = Vec::with_capacity(targets.len());
    for (index, (provider, upstream_model)) in targets.into_iter().enumerate() {
        let provider = provider.trim();
        if provider.is_empty() {
            return Err(format!("Choose a provider for target {}.", index + 1));
        }
        let mut target = serde_json::Map::new();
        target.insert(
            "provider".into(),
            serde_json::Value::String(provider.into()),
        );
        if !upstream_model.trim().is_empty() {
            target.insert(
                "model".into(),
                serde_json::Value::String(upstream_model.trim().into()),
            );
        }
        built.push(serde_json::Value::Object(target));
    }
    if built.is_empty() {
        return Err("Add at least one provider target.".into());
    }
    Ok(serde_json::json!({ "model": model, "targets": built }))
}

fn topology_entry_summary(entry: &crate::api::LlmleafTopologyEntry) -> String {
    if entry.kind == "provider" {
        let provider_kind = entry
            .spec
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("custom");
        let mut parts = vec![provider_kind.to_string()];
        if let Some(endpoint) = entry
            .spec
            .get("endpoint")
            .and_then(serde_json::Value::as_str)
        {
            parts.push(endpoint.to_string());
        }
        if let Some(prefix) = entry.spec.get("prefix").and_then(serde_json::Value::as_str) {
            parts.push(format!("prefix {prefix}/"));
        }
        parts.join(" · ")
    } else {
        entry
            .spec
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .map(|targets| {
                targets
                    .iter()
                    .filter_map(|target| {
                        let provider = target.get("provider")?.as_str()?;
                        let model = target.get("model").and_then(serde_json::Value::as_str);
                        Some(match model {
                            Some(model) => format!("{provider} → {model}"),
                            None => provider.to_string(),
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("  ›  ")
            })
            .filter(|summary| !summary.is_empty())
            .unwrap_or_else(|| "No targets".into())
    }
}

/// **llmleaf control plane** — a guided editor for the dynamic provider and
/// route overlay. The deployment config gates both this tab and its API routes.
#[component]
fn LlmleafSection() -> impl IntoView {
    let form_kind = RwSignal::new("providers".to_string());
    let provider_name = RwSignal::new(String::new());
    let provider_kind = RwSignal::new("openai".to_string());
    let credential_env = RwSignal::new(String::new());
    let provider_endpoint = RwSignal::new(String::new());
    let provider_prefix = RwSignal::new(String::new());
    let route_model = RwSignal::new(String::new());
    let enabled = RwSignal::new(true);
    let target_rows = RwSignal::new(Vec::<LlmleafTargetRow>::new());
    let next_target_id = RwSignal::new(0_usize);
    let entries = RwSignal::new(Vec::<crate::api::LlmleafTopologyEntry>::new());
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);
    let reload = RwSignal::new(0_u32);
    let busy = RwSignal::new(false);

    let new_target = move || {
        let id = next_target_id.get_untracked();
        next_target_id.set(id + 1);
        LlmleafTargetRow {
            id,
            provider: RwSignal::new(String::new()),
            model: RwSignal::new(String::new()),
        }
    };
    target_rows.set(vec![new_target()]);

    Effect::new(move |_| {
        reload.get();
        spawn_local(async move {
            let token = auth::resolve_token();
            let providers = rest::list_llmleaf_topology(token.as_deref(), "providers").await;
            let routes = rest::list_llmleaf_topology(token.as_deref(), "routes").await;
            match (providers, routes) {
                (Ok(mut providers), Ok(routes)) => {
                    providers.extend(routes);
                    entries.set(providers);
                    error.set(None);
                }
                (Err(err), _) | (_, Err(err)) => error.set(Some(err.to_string())),
            }
        });
    });

    let save = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        if busy.get_untracked() {
            return;
        }
        error.set(None);
        notice.set(None);
        let selected = form_kind.get_untracked();
        let (resource_name, spec) = if selected == "providers" {
            let name = provider_name.get_untracked();
            let spec = provider_topology_spec(
                &name,
                &provider_kind.get_untracked(),
                &credential_env.get_untracked(),
                &provider_endpoint.get_untracked(),
                &provider_prefix.get_untracked(),
            );
            (name, spec)
        } else {
            let name = route_model.get_untracked();
            let targets = target_rows
                .get_untracked()
                .into_iter()
                .map(|row| (row.provider.get_untracked(), row.model.get_untracked()))
                .collect();
            (name.clone(), route_topology_spec(&name, targets))
        };
        let spec = match spec {
            Ok(spec) => spec,
            Err(message) => {
                error.set(Some(message));
                return;
            }
        };
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = crate::api::PutLlmleafTopology {
                enabled: enabled.get_untracked(),
                spec,
            };
            match rest::put_llmleaf_topology(
                token.as_deref(),
                &selected,
                resource_name.trim(),
                &body,
            )
            .await
            {
                Ok(_) => {
                    if selected == "providers" {
                        provider_name.set(String::new());
                        credential_env.set(String::new());
                        provider_endpoint.set(String::new());
                        provider_prefix.set(String::new());
                        notice.set(Some(
                            "Provider saved. llmleaf will pick it up shortly.".into(),
                        ));
                    } else {
                        route_model.set(String::new());
                        target_rows.set(vec![new_target()]);
                        notice.set(Some(
                            "Route saved. llmleaf will reconcile the fallback chain shortly."
                                .into(),
                        ));
                    }
                    reload.update(|value| *value += 1);
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            busy.set(false);
        });
    };

    let remove = move |entry_kind: String, resource_name: String| {
        busy.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_llmleaf_topology(token.as_deref(), &entry_kind, &resource_name).await
            {
                Ok(()) => reload.update(|value| *value += 1),
                Err(err) => error.set(Some(err.to_string())),
            }
            busy.set(false);
        });
    };

    let set_enabled = move |entry: crate::api::LlmleafTopologyEntry, next_enabled: bool| {
        busy.set(true);
        error.set(None);
        notice.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = crate::api::PutLlmleafTopology {
                enabled: next_enabled,
                spec: entry.spec,
            };
            match rest::put_llmleaf_topology(token.as_deref(), &entry.kind, &entry.name, &body)
                .await
            {
                Ok(_) => reload.update(|value| *value += 1),
                Err(err) => error.set(Some(err.to_string())),
            }
            busy.set(false);
        });
    };

    view! {
        <section class="settings-section llmleaf-section">
            <div class="llmleaf-heading">
                <div>
                    <h3 class="settings-section-title">"llmleaf control plane"</h3>
                    <p class="settings-hint">
                        "Connect model providers, then route friendly model names through an ordered fallback chain. Changes apply without restarting the gateway."
                    </p>
                </div>
                <span class="llmleaf-live"><i></i>"live topology"</span>
            </div>

            <div class="llmleaf-kind-switch" role="tablist" aria-label="Topology resource">
                <button type="button" role="tab"
                    class:llmleaf-kind-active=move || form_kind.get() == "providers"
                    aria-selected=move || form_kind.get() == "providers"
                    on:click=move |_| { form_kind.set("providers".into()); error.set(None); notice.set(None); }>
                    <span>"01"</span>"Providers"
                </button>
                <button type="button" role="tab"
                    class:llmleaf-kind-active=move || form_kind.get() == "routes"
                    aria-selected=move || form_kind.get() == "routes"
                    on:click=move |_| { form_kind.set("routes".into()); error.set(None); notice.set(None); }>
                    <span>"02"</span>"Routes"
                </button>
            </div>

            <form class="settings-form llmleaf-form-card" on:submit=save>
                <Show when=move || form_kind.get() == "providers" fallback=move || view! {
                    <div class="llmleaf-card-heading">
                        <div>
                            <strong>"New model route"</strong>
                            <span>"Name the model clients use and order its provider fallbacks."</span>
                        </div>
                        <span class="llmleaf-resource-mark">"R"</span>
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="llmleaf-route-model">"Route model"</label>
                        <input id="llmleaf-route-model" class="settings-input" placeholder="e.g. smart or gpt-4o"
                            prop:value=move || route_model.get()
                            on:input=move |ev| route_model.set(event_target_value(&ev)) />
                        <span class="llmleaf-field-help">"This is the model id Catalerum and other gateway clients request."</span>
                    </div>
                    <div class="llmleaf-target-head">
                        <label class="settings-label">"Fallback chain"</label>
                        <span>"Tried top to bottom"</span>
                    </div>
                    <datalist id="llmleaf-provider-names">
                        {move || entries.get().into_iter()
                            .filter(|entry| entry.kind == "provider")
                            .map(|entry| view! { <option value=entry.name></option> })
                            .collect::<Vec<_>>()}
                    </datalist>
                    <div class="llmleaf-targets">
                        <For
                            each=move || target_rows.get()
                            key=|row| row.id
                            children=move |row: LlmleafTargetRow| {
                                let remove_row = move |_| {
                                    target_rows.update(|rows| rows.retain(|candidate| candidate.id != row.id));
                                };
                                view! {
                                    <div class="llmleaf-target-row">
                                        <span class="llmleaf-target-order">{move || {
                                            target_rows.get().iter().position(|candidate| candidate.id == row.id)
                                                .map(|index| format!("{}", index + 1)).unwrap_or_default()
                                        }}</span>
                                        <div class="settings-field">
                                            <label class="settings-label">"Provider"</label>
                                            <input class="settings-input" list="llmleaf-provider-names" aria-label="Provider"
                                                placeholder="provider instance"
                                                prop:value=move || row.provider.get()
                                                on:input=move |ev| row.provider.set(event_target_value(&ev)) />
                                        </div>
                                        <div class="settings-field">
                                            <label class="settings-label">"Upstream model"</label>
                                            <input class="settings-input" aria-label="Upstream model"
                                                placeholder="same as route"
                                                prop:value=move || row.model.get()
                                                on:input=move |ev| row.model.set(event_target_value(&ev)) />
                                        </div>
                                        <button type="button" class="llmleaf-target-remove" title="Remove target"
                                            disabled=move || target_rows.get().len() <= 1
                                            on:click=remove_row><Icon icon=MdIcon::Close /></button>
                                    </div>
                                }
                            }
                        />
                    </div>
                    <button type="button" class="llmleaf-add-target"
                        on:click=move |_| target_rows.update(|rows| rows.push(new_target()))>
                        <Icon icon=MdIcon::Add />"Add fallback target"
                    </button>
                }>
                    <div class="llmleaf-card-heading">
                        <div>
                            <strong>"New provider"</strong>
                            <span>"Credentials stay in the environment; only their variable name is stored."</span>
                        </div>
                        <span class="llmleaf-resource-mark">"P"</span>
                    </div>
                    <datalist id="llmleaf-provider-kinds">
                        {[
                            "openai", "anthropic", "gemini", "openrouter", "mistral", "groq",
                            "xai", "requesty", "together", "fireworks", "cerebras", "ollama",
                            "lmstudio", "echo",
                        ].into_iter().map(|kind| view! { <option value=kind></option> }).collect::<Vec<_>>()}
                    </datalist>
                    <div class="llmleaf-form-grid">
                        <div class="settings-field">
                            <label class="settings-label" for="llmleaf-provider-name">"Provider name"</label>
                            <input id="llmleaf-provider-name" class="settings-input" placeholder="e.g. openai-main"
                                prop:value=move || provider_name.get()
                                on:input=move |ev| provider_name.set(event_target_value(&ev)) />
                        </div>
                        <div class="settings-field">
                            <label class="settings-label" for="llmleaf-provider-kind">"Provider type"</label>
                            <input id="llmleaf-provider-kind" class="settings-input" list="llmleaf-provider-kinds"
                                prop:value=move || provider_kind.get()
                                on:input=move |ev| provider_kind.set(event_target_value(&ev)) />
                        </div>
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="llmleaf-credential-env">"API key environment variable"</label>
                        <div class="llmleaf-env-input">
                            <span>"env:"</span>
                            <input id="llmleaf-credential-env" class="settings-input" placeholder="OPENAI_API_KEY"
                                prop:value=move || credential_env.get()
                                on:input=move |ev| credential_env.set(event_target_value(&ev)) />
                        </div>
                        <span class="llmleaf-field-help">"Leave blank for local or credential-free providers. Secret values never enter Catalerum."</span>
                    </div>
                    <details class="llmleaf-advanced">
                        <summary>"Connection options"</summary>
                        <div class="llmleaf-form-grid">
                            <div class="settings-field">
                                <label class="settings-label" for="llmleaf-provider-endpoint">"Custom endpoint"</label>
                                <input id="llmleaf-provider-endpoint" class="settings-input" type="url"
                                    placeholder="https://api.example.com/v1"
                                    prop:value=move || provider_endpoint.get()
                                    on:input=move |ev| provider_endpoint.set(event_target_value(&ev)) />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label" for="llmleaf-provider-prefix">"Model prefix"</label>
                                <input id="llmleaf-provider-prefix" class="settings-input" placeholder="e.g. or"
                                    prop:value=move || provider_prefix.get()
                                    on:input=move |ev| provider_prefix.set(event_target_value(&ev)) />
                            </div>
                        </div>
                    </details>
                </Show>

                <div class="llmleaf-form-footer">
                    <label class="settings-check llmleaf-enabled">
                        <input type="checkbox" prop:checked=move || enabled.get()
                            on:change=move |ev| enabled.set(event_target_checked(&ev)) />
                        <span><strong>"Enabled"</strong>"Include in the live topology immediately"</span>
                    </label>
                    <button type="submit" class="settings-btn settings-btn-primary llmleaf-save"
                        disabled=move || busy.get()>
                        {move || if busy.get() { "Saving…" } else if form_kind.get() == "providers" { "Add provider" } else { "Save route" }}
                    </button>
                </div>
                {move || error.get().map(|message| view! {
                    <div class="settings-form-error llmleaf-message">{message}</div>
                })}
                {move || notice.get().map(|message| view! {
                    <div class="settings-form-notice llmleaf-message">{message}</div>
                })}
            </form>

            <div class="llmleaf-list-heading">
                <div>
                    <span>{move || if form_kind.get() == "providers" { "Configured providers" } else { "Configured routes" }}</span>
                    <strong>{move || {
                        let selected = if form_kind.get() == "providers" { "provider" } else { "route" };
                        entries.get().into_iter().filter(|entry| entry.kind == selected).count()
                    }}</strong>
                </div>
                <span>"Dynamic overlay"</span>
            </div>
            <Show when=move || {
                let selected = if form_kind.get() == "providers" { "provider" } else { "route" };
                entries.get().into_iter().any(|entry| entry.kind == selected)
            } fallback=|| view! { <div class="llmleaf-empty">"Nothing configured here yet."</div> }>
                <ul class="settings-conn-list llmleaf-entry-list">
                    <For
                        each=move || {
                            let selected = if form_kind.get() == "providers" { "provider" } else { "route" };
                            entries.get().into_iter().filter(|entry| entry.kind == selected).collect::<Vec<_>>()
                        }
                        key=|entry| (entry.kind.clone(), entry.name.clone(), entry.enabled, entry.spec.to_string())
                        children=move |entry: crate::api::LlmleafTopologyEntry| {
                            let summary = topology_entry_summary(&entry);
                            let is_enabled = entry.enabled;
                            let toggle_entry = entry.clone();
                            let remove_kind = entry.kind.clone();
                            let remove_name = entry.name.clone();
                            view! {
                                <li class="llmleaf-entry" class:llmleaf-entry-off=move || !is_enabled>
                                    <span class="llmleaf-entry-icon">{if entry.kind == "provider" { "P" } else { "R" }}</span>
                                    <div class="llmleaf-entry-copy">
                                        <div>
                                            <strong>{entry.name}</strong>
                                            <span class="settings-conn-state" class:settings-conn-synced=move || is_enabled>
                                                {if is_enabled { "active" } else { "paused" }}
                                            </span>
                                        </div>
                                        <span>{summary}</span>
                                    </div>
                                    <div class="llmleaf-entry-actions">
                                        <button type="button" class="settings-btn settings-btn-mini"
                                            disabled=move || busy.get()
                                            on:click=move |_| set_enabled(toggle_entry.clone(), !is_enabled)>
                                            {if is_enabled { "Pause" } else { "Enable" }}
                                        </button>
                                        <button type="button" class="settings-conn-del" title="Delete"
                                            disabled=move || busy.get()
                                            on:click=move |_| remove(remove_kind.clone(), remove_name.clone())>"Delete"</button>
                                    </div>
                                </li>
                            }
                        }
                    />
                </ul>
            </Show>
        </section>
    }
}

/// Local-password account administration. The server scopes every operation to
/// the active workspace and re-checks the caller's owner/admin role.
#[component]
fn UsersSection() -> impl IntoView {
    let users = RwSignal::new(Vec::<crate::api::ManagedUser>::new());
    let loading = RwSignal::new(true);
    let email = RwSignal::new(String::new());
    let display_name = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let role = RwSignal::new("member".to_string());
    let reset_user = RwSignal::new(String::new());
    let reset_password = RwSignal::new(String::new());
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);
    let reload = RwSignal::new(0_u32);
    let busy = RwSignal::new(false);

    Effect::new(move |_| {
        reload.get();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_managed_users(token.as_deref()).await {
                Ok(rows) => {
                    if reset_user.get_untracked().is_empty() {
                        if let Some(first) = rows.first() {
                            reset_user.set(first.id.clone());
                        }
                    }
                    users.set(rows);
                    error.set(None);
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            loading.set(false);
        });
    });

    let create = move |_| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        notice.set(None);
        let body = crate::api::CreateManagedUser {
            email: email.get_untracked(),
            display_name: display_name.get_untracked(),
            password: password.get_untracked(),
            role: role.get_untracked(),
        };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::create_managed_user(token.as_deref(), &body).await {
                Ok(created) => {
                    notice.set(Some(format!("Created {}.", created.email)));
                    email.set(String::new());
                    display_name.set(String::new());
                    password.set(String::new());
                    reload.update(|value| *value += 1);
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            busy.set(false);
        });
    };

    let reset = move |_| {
        let user_id = reset_user.get_untracked();
        if busy.get_untracked() || user_id.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        notice.set(None);
        let next_password = reset_password.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::reset_managed_password(token.as_deref(), &user_id, next_password).await {
                Ok(()) => {
                    reset_password.set(String::new());
                    notice.set(Some("Password updated.".to_string()));
                }
                Err(err) => error.set(Some(err.to_string())),
            }
            busy.set(false);
        });
    };

    view! {
        <section class="settings-section settings-users">
            <header class="settings-users-heading">
                <div>
                    <h3 class="settings-users-title">"User management"</h3>
                    <p class="settings-hint">
                        "Create local accounts, assign access, and maintain workspace credentials."
                    </p>
                </div>
                <span class="settings-users-count">
                    <strong>{move || users.get().len()}</strong>
                    " accounts"
                </span>
            </header>

            {move || notice.get().map(|message| view! {
                <div class="settings-form-notice settings-users-message" role="status">{message}</div>
            })}
            {move || error.get().map(|message| view! {
                <div class="settings-form-error settings-users-message" role="alert">{message}</div>
            })}

            <div class="settings-user-card settings-user-create-card">
                <div class="settings-user-card-head">
                    <div>
                        <h4>"Add an account"</h4>
                        <p>"Invite someone directly to this workspace."</p>
                    </div>
                    <span class="settings-user-step" aria-hidden="true">"01"</span>
                </div>
                <div class="settings-form settings-user-create-form">
                    <div class="settings-field">
                        <label class="settings-label" for="managed-user-email">"Email"</label>
                        <input id="managed-user-email" class="settings-input" type="email"
                            autocomplete="email" placeholder="name@company.com"
                            prop:value=move || email.get()
                            on:input=move |ev| email.set(event_target_value(&ev)) />
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="managed-user-name">"Display name"</label>
                        <input id="managed-user-name" class="settings-input" autocomplete="name"
                            placeholder="Their name" prop:value=move || display_name.get()
                            on:input=move |ev| display_name.set(event_target_value(&ev)) />
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="managed-user-password">"Initial password"</label>
                        <input id="managed-user-password" class="settings-input" type="password"
                            autocomplete="new-password" minlength="12" placeholder="12 characters minimum"
                            prop:value=move || password.get()
                            on:input=move |ev| password.set(event_target_value(&ev)) />
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="managed-user-role">"Workspace role"</label>
                        <select id="managed-user-role" class="settings-input"
                            on:change=move |ev| role.set(event_target_value(&ev))>
                            <option value="member">"Member"</option>
                            <option value="viewer">"Viewer"</option>
                            <option value="admin">"Admin"</option>
                            <option value="owner">"Owner"</option>
                        </select>
                    </div>
                    <button class="settings-btn settings-btn-primary settings-user-submit" on:click=create
                        disabled=move || busy.get()>"Create user"</button>
                </div>
            </div>

            <div class="settings-user-card settings-user-reset-card">
                <div class="settings-user-card-head">
                    <div>
                        <h4>"Reset a password"</h4>
                        <p>"Issue a new temporary credential for an existing account."</p>
                    </div>
                    <span class="settings-user-step" aria-hidden="true">"02"</span>
                </div>
                <div class="settings-form settings-user-reset-form">
                    <div class="settings-field settings-user-reset-account">
                        <label class="settings-label" for="managed-reset-user">"Account"</label>
                        <select id="managed-reset-user" class="settings-input"
                            prop:value=move || reset_user.get()
                            on:change=move |ev| reset_user.set(event_target_value(&ev))>
                            <For
                                each=move || users.get()
                                key=|user| user.id.clone()
                                children=move |user| view! {
                                    <option value=user.id>{format!("{} ({})", user.display_name, user.email)}</option>
                                }
                            />
                        </select>
                    </div>
                    <div class="settings-field">
                        <label class="settings-label" for="managed-reset-password">"New password"</label>
                        <input id="managed-reset-password" class="settings-input" type="password"
                            autocomplete="new-password" minlength="12" placeholder="New password"
                            prop:value=move || reset_password.get()
                            on:input=move |ev| reset_password.set(event_target_value(&ev)) />
                    </div>
                    <button class="settings-btn settings-user-reset-submit" on:click=reset
                        disabled=move || busy.get() || reset_user.get().is_empty()>
                        "Reset password"
                    </button>
                </div>
            </div>

            <div class="settings-user-directory-head">
                <h4>"Workspace directory"</h4>
                <span>"Account / access"</span>
            </div>
            {move || if loading.get() {
                view! { <div class="settings-user-empty">"Loading accounts…"</div> }.into_any()
            } else if users.get().is_empty() {
                view! {
                    <div class="settings-user-empty">
                        <strong>"No accounts yet"</strong>
                        <span>"Create the first workspace account above."</span>
                    </div>
                }.into_any()
            } else {
                view! {
                    <ul class="settings-user-list">
                        <For
                            each=move || users.get()
                            key=|user| user.id.clone()
                            children=move |user| {
                                let initials = user.display_name
                                    .split_whitespace()
                                    .filter_map(|part| part.chars().next())
                                    .take(2)
                                    .collect::<String>()
                                    .to_uppercase();
                                let role = user.role.clone();
                                view! {
                                    <li class="settings-user-row">
                                        <span class="settings-user-avatar" aria-hidden="true">{initials}</span>
                                        <div class="settings-user-meta">
                                            <strong>{user.display_name}</strong>
                                            <span>{user.email}</span>
                                        </div>
                                        <span class="settings-user-role" data-role=role.clone()>{role.clone()}</span>
                                    </li>
                                }
                            }
                        />
                    </ul>
                }.into_any()
            }}
        </section>
    }
}

#[component]
fn LlmSection() -> impl IntoView {
    let status = RwSignal::new(Option::<StatusInfo>::None);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(Option::<String>::None);

    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::get_status(token.as_deref()).await {
            Ok(s) => status.set(Some(s)),
            Err(e) => error.set(Some(e.to_string())),
        }
        loading.set(false);
    });

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "The LLM gateway catalerum talks to (llmleaf / OpenRouter). This is set at boot via "
                "the " <code>"[llm]"</code> " config or " <code>"CATALERUM_LLM__*"</code>
                " environment variables — the API key is never shown."
            </p>
            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>
            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-status settings-error">
                    {move || format!("Could not load: {}", error.get().unwrap_or_default())}
                </div>
            </Show>
            {move || {
                status.get().map(|s| {
                    let llm = s.llm;
                    let ocr_engines = if llm.ocr_engines.is_empty() {
                        "not configured".to_string()
                    } else {
                        llm.ocr_engines.join(" → ")
                    };
                    view! {
                        <dl class="settings-kv">
                            <div class="settings-kv-row">
                                <dt>"Gateway URL"</dt>
                                <dd class="settings-mono">{llm.base_url}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"Chat model"</dt>
                                <dd class="settings-mono">{llm.default_model}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"Embedding model"</dt>
                                <dd class="settings-mono">{llm.embedding_model}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"Speech model"</dt>
                                <dd class="settings-mono">{llm.speech_model}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"Speech voice"</dt>
                                <dd class="settings-mono">{llm.speech_voice}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"Transcription model"</dt>
                                <dd class="settings-mono">{llm.transcription_model}</dd>
                            </div>
                            <div class="settings-kv-row">
                                <dt>"OCR engines"</dt>
                                <dd class="settings-mono">{ocr_engines}</dd>
                            </div>
                        </dl>
                    }
                })
            }}
        </section>
    }
}

/// A trimmed selection, or `None` when blank — so a blank field **clears** the
/// override (the gateway default then applies) rather than storing `""`.
fn blank_to_none(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// **Models, voices, and voice input** — the per-user override of the gateway
/// model/voice defaults plus microphone time compression (SOUL §7/§13). Model
/// fields are manual text inputs with autocompletion:
/// type a model/voice id and a `<datalist>` populated from the gateway's catalog
/// (`/llm-models`, `/llm-voices`) suggests matches — but any free-text id is
/// accepted, and a blank field falls back to the configured default (shown as the
/// placeholder). The chat model takes effect on the next message; speech/voice
/// selections are stored for the text-to-speech surface.
#[component]
fn ModelsSection() -> impl IntoView {
    // Current selections (editable). An empty string means "use gateway default".
    let chat_model = RwSignal::new(String::new());
    let speech_model = RwSignal::new(String::new());
    let speech_voice = RwSignal::new(String::new());
    let transcription_model = RwSignal::new(String::new());
    let voice_input_speed = RwSignal::new(crate::api::default_voice_input_speed());
    let ocr_model = RwSignal::new(String::new());
    // Force-image-input override (SOUL §7/§9): model ids to treat as image-capable
    // regardless of the catalog. Edited via the dedicated `PUT /llm-settings/image-
    // models` (each add/remove saves immediately), NOT the "Save" button below.
    let image_input_models = RwSignal::new(Vec::<String>::new());
    // The autocomplete box for adding one — cleared after each commit.
    let add_image_model = RwSignal::new(String::new());

    // Autocomplete sources, one per field and each filtered to its model class:
    // chat (`llm`), speech (pure `tts`), transcription (pure `stt`). A mixed
    // catalog would offer transcription models in the speech picker and vice versa;
    // TTS/STT-only ids also live only under their kind, so the per-kind fetch is the
    // only way to surface them (see `GET /llm-models?kind=`).
    let chat_models = RwSignal::new(Vec::<ModelInfo>::new());
    let speech_models = RwSignal::new(Vec::<ModelInfo>::new());
    let transcription_models = RwSignal::new(Vec::<ModelInfo>::new());
    // OCR runs through a **vision** chat model, so its suggestions are the chat
    // catalog narrowed to models advertising `image` input — a text-only pick
    // would fail at OCR time (free text still accepted, as everywhere).
    let ocr_models = RwSignal::new(Vec::<ModelInfo>::new());
    let voices = RwSignal::new(Vec::<VoiceInfo>::new());

    // Gateway defaults (the `[llm]` config), shown as placeholders.
    let defaults = RwSignal::new(Option::<LlmInfo>::None);

    let loading = RwSignal::new(true);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    // On mount: load the saved selections, the gateway defaults (placeholders),
    // and the gateway model catalog (best-effort — a gateway with no catalog just
    // yields an empty datalist; the inputs still take free text).
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::get_llm_settings(token.as_deref()).await {
            Ok(s) => {
                chat_model.set(s.chat_model.unwrap_or_default());
                speech_model.set(s.speech_model.unwrap_or_default());
                speech_voice.set(s.speech_voice.unwrap_or_default());
                transcription_model.set(s.transcription_model.unwrap_or_default());
                voice_input_speed.set(s.voice_input_speed);
                ocr_model.set(s.ocr_model.unwrap_or_default());
                image_input_models.set(s.image_input_models);
            }
            Err(e) => error.set(Some(e.to_string())),
        }
        if let Ok(st) = rest::get_status(token.as_deref()).await {
            defaults.set(Some(st.llm));
        }
        if let Ok(m) = rest::list_llm_models(token.as_deref(), "llm").await {
            ocr_models.set(
                m.iter()
                    .filter(|mi| {
                        mi.input_modalities
                            .iter()
                            .any(|x| x.eq_ignore_ascii_case("image"))
                    })
                    .cloned()
                    .collect(),
            );
            chat_models.set(m);
        }
        if let Ok(m) = rest::list_llm_models(token.as_deref(), "tts").await {
            speech_models.set(m);
        }
        if let Ok(m) = rest::list_llm_models(token.as_deref(), "stt").await {
            transcription_models.set(m);
        }
        loading.set(false);
    });

    // Default-value placeholders, derived from the loaded gateway config.
    let default_chat =
        Signal::derive(move || defaults.get().map(|d| d.default_model).unwrap_or_default());
    let default_speech =
        Signal::derive(move || defaults.get().map(|d| d.speech_model).unwrap_or_default());
    let default_voice =
        Signal::derive(move || defaults.get().map(|d| d.speech_voice).unwrap_or_default());
    let default_transcription = Signal::derive(move || {
        defaults
            .get()
            .map(|d| d.transcription_model)
            .unwrap_or_default()
    });

    // Voices are per speech-model: reload them whenever the chosen speech model
    // (or, while it's blank, the gateway default) changes.
    Effect::new(move |_| {
        let chosen = speech_model.get();
        let effective = if chosen.trim().is_empty() {
            default_speech.get()
        } else {
            chosen
        };
        let token = auth::resolve_token();
        spawn_local(async move {
            match rest::list_llm_voices(token.as_deref(), effective.trim()).await {
                Ok(v) => voices.set(v),
                Err(_) => voices.set(Vec::new()),
            }
        });
    });

    // A labelled model field: a free-text input with autocomplete search over the
    // gateway's catalog. The placeholder shows the default a blank field falls back
    // to; the suggestion labels carry a context-length hint when one is known.
    let model_field = move |label: &'static str,
                            value: RwSignal<String>,
                            options: RwSignal<Vec<ModelInfo>>,
                            default_value: Signal<String>| {
        view! {
            <div class="settings-field">
                <label class="settings-label">{label}</label>
                {model_autocomplete(
                    Signal::derive(move || value.get()),
                    move |v| value.set(v),
                    model_options(options, true),
                    Signal::derive(move || {
                        let d = default_value.get();
                        if d.is_empty() {
                            "gateway default".to_string()
                        } else {
                            format!("default: {d}")
                        }
                    }),
                    Signal::derive(|| false),
                    "settings-input",
                )}
            </div>
        }
    };

    let voice_placeholder = move || {
        let d = default_voice.get();
        if d.is_empty() {
            "gateway default".to_string()
        } else {
            format!("default: {d}")
        }
    };

    let save = move || {
        saving.set(true);
        error.set(None);
        notice.set(None);
        let body = LlmSettings {
            chat_model: blank_to_none(chat_model.get_untracked()),
            speech_model: blank_to_none(speech_model.get_untracked()),
            speech_voice: blank_to_none(speech_voice.get_untracked()),
            transcription_model: blank_to_none(transcription_model.get_untracked()),
            voice_input_speed: voice_input_speed.get_untracked(),
            ocr_model: blank_to_none(ocr_model.get_untracked()),
            // A plain PUT /llm-settings ignores this list (it's edited via the
            // dedicated route); echo the current value for a faithful full struct.
            image_input_models: image_input_models.get_untracked(),
        };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_llm_settings(token.as_deref(), &body).await {
                Ok(s) => {
                    // Re-sync from the server's normalized record.
                    chat_model.set(s.chat_model.unwrap_or_default());
                    speech_model.set(s.speech_model.unwrap_or_default());
                    speech_voice.set(s.speech_voice.unwrap_or_default());
                    transcription_model.set(s.transcription_model.unwrap_or_default());
                    voice_input_speed.set(s.voice_input_speed);
                    ocr_model.set(s.ocr_model.unwrap_or_default());
                    notice.set(Some("Saved.".to_string()));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            saving.set(false);
        });
    };

    // Persist the current force-image-input list to its dedicated route. Called on
    // every add/remove, so the list saves without the "Save" button.
    let save_image_models = move || {
        let models = image_input_models.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_image_input_models(token.as_deref(), &models).await {
                Ok(s) => image_input_models.set(s.image_input_models),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    // Add the committed model id (deduped, non-blank) and clear the box.
    let commit_add_image_model = move |v: String| {
        let v = v.trim().to_string();
        add_image_model.set(String::new());
        if v.is_empty() {
            return;
        }
        let mut changed = false;
        image_input_models.update(|l| {
            if !l.iter().any(|m| m == &v) {
                l.push(v.clone());
                changed = true;
            }
        });
        if changed {
            save_image_models();
        }
    };
    let remove_image_model = move |m: String| {
        image_input_models.update(|l| l.retain(|x| x != &m));
        save_image_models();
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Choose the models catalerum uses for you. These override the gateway defaults "
                "shown under " <strong>"LLM gateway"</strong> " — type a model or voice id (the box "
                "autocompletes from the gateway's catalog) or leave a field blank to keep the "
                "default. Your chat model takes effect on your next message."
            </p>

            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>

            {model_field("Chat model", chat_model, chat_models, default_chat)}

            <div class="settings-field">
                <label class="settings-label">"Force image input (vision)"</label>
                <p class="settings-blurb">
                    "Models listed here are treated as accepting image attachments even when "
                    "the gateway catalog doesn't advertise it — use this when a vision model's "
                    "capabilities are under-reported, so chat inlines your uploaded images to it. "
                    "Each change saves immediately."
                </p>
                <Show
                    when=move || !image_input_models.get().is_empty()
                    fallback=|| ().into_view()
                >
                    <div class="settings-chips">
                        <For
                            each=move || image_input_models.get()
                            key=|m| m.clone()
                            children=move |m: String| {
                                let for_remove = m.clone();
                                view! {
                                    <span class="settings-chip">
                                        {m.clone()}
                                        <button
                                            class="settings-chip-x"
                                            title="Remove"
                                            on:click=move |_| remove_image_model(for_remove.clone())
                                        >
                                            <Icon icon=MdIcon::Close />
                                        </button>
                                    </span>
                                }
                            }
                        />
                    </div>
                </Show>
                {model_autocomplete(
                    Signal::derive(move || add_image_model.get()),
                    commit_add_image_model,
                    model_options(chat_models, false),
                    Signal::derive(|| "Add a model id…".to_string()),
                    Signal::derive(|| false),
                    "settings-input",
                )}
            </div>

            {model_field("Speech model (text-to-speech)", speech_model, speech_models, default_speech)}

            <div class="settings-field">
                <label class="settings-label">"Speech voice"</label>
                {model_autocomplete(
                    Signal::derive(move || speech_voice.get()),
                    move |v| speech_voice.set(v),
                    voice_options(voices),
                    Signal::derive(voice_placeholder),
                    Signal::derive(|| false),
                    "settings-input",
                )}
            </div>

            {model_field(
                "Transcription model (speech-to-text)",
                transcription_model,
                transcription_models,
                default_transcription,
            )}

            <div class="settings-field">
                <label class="settings-label" for="voice-input-speed">
                    "Voice input speed"
                </label>
                <p class="settings-blurb">
                    "Shortens microphone audio before transcription for both dictation and "
                    "hands-free conversation. Faster audio can reduce usage with STT providers "
                    "that bill by audio duration, but very high values may reduce accuracy."
                </p>
                <input
                    id="voice-input-speed"
                    class="settings-input settings-input-narrow"
                    type="number"
                    min="1"
                    max="2"
                    step="0.05"
                    prop:value=move || format!("{:.2}", voice_input_speed.get())
                    on:input=move |ev| {
                        if let Ok(value) = event_target_value(&ev).parse::<f32>() {
                            voice_input_speed.set(value);
                        }
                    }
                />
                <span class="settings-hint">
                    {move || format!("{:.2}× (default 1.50×)", voice_input_speed.get())}
                </span>
            </div>

            <div class="settings-field">
                <label class="settings-label">"OCR model (image → text)"</label>
                <p class="settings-blurb">
                    "Vision model used when you run OCR on an image or PDF (only "
                    "image-capable chat models are suggested). Leave blank to use the "
                    "server's configured OCR engines."
                </p>
                {model_autocomplete(
                    Signal::derive(move || ocr_model.get()),
                    move |v| ocr_model.set(v),
                    model_options(ocr_models, true),
                    Signal::derive(move || {
                        let engines = defaults
                            .get()
                            .map(|d| d.ocr_engines)
                            .unwrap_or_default();
                        if engines.is_empty() {
                            "server OCR not configured".to_string()
                        } else {
                            format!("default: {}", engines.join(" → "))
                        }
                    }),
                    Signal::derive(|| false),
                    "settings-input",
                )}
            </div>

            <p class="settings-blurb">
                "The embedding model is fixed by the deployment — changing it would invalidate the "
                "existing vector index — so it is not selectable here."
            </p>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="settings-actions">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || saving.get() || loading.get()
                    on:click=move |_| save()
                >
                    {move || if saving.get() { "Saving…" } else { "Save" }}
                </button>
            </div>
        </section>
    }
}

/// **Web search** — pick your default search provider and see which engines are
/// configured (SOUL §27). The provider API keys themselves are **server-side
/// config** (`[search]`), never entered or shown here — this panel only chooses
/// among the engines an admin has enabled and sets your personal default.
#[component]
fn SearchSection() -> impl IntoView {
    // The configured-providers catalog (name + enabled + is_default).
    let providers = RwSignal::new(Vec::<SearchProviderInfo>::new());
    // The caller's default-provider override; "" means "use the server default".
    let default_provider = RwSignal::new(String::new());

    let loading = RwSignal::new(true);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    // On mount: load the provider catalog + the saved per-user default.
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::list_search_providers(token.as_deref()).await {
            Ok(p) => providers.set(p),
            Err(e) => error.set(Some(e.to_string())),
        }
        match rest::get_search_settings(token.as_deref()).await {
            Ok(s) => default_provider.set(s.default_provider.unwrap_or_default()),
            Err(e) => error.set(Some(e.to_string())),
        }
        loading.set(false);
    });

    // Whether any provider is configured — drives the "configure a key first" hint.
    let any_enabled = Signal::derive(move || providers.get().iter().any(|p| p.enabled));

    let save = move || {
        saving.set(true);
        error.set(None);
        notice.set(None);
        let body = SearchSettings {
            default_provider: blank_to_none(default_provider.get_untracked()),
        };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_search_settings(token.as_deref(), &body).await {
                Ok(s) => {
                    default_provider.set(s.default_provider.unwrap_or_default());
                    // Reload the catalog so the "(default)" marker reflects the save.
                    if let Ok(p) = rest::list_search_providers(token.as_deref()).await {
                        providers.set(p);
                    }
                    notice.set(Some("Saved.".to_string()));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            saving.set(false);
        });
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Choose which engine the " <strong>"web_search"</strong> " tool uses for you by "
                "default. Leave it on " <strong>"server default"</strong>
                " to follow the deployment's choice. Provider API keys are set in server "
                "configuration (" <code>"[search]"</code> "), not here."
            </p>

            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>

            <Show
                when=move || !loading.get() && !any_enabled.get()
                fallback=|| ().into_view()
            >
                <div class="settings-status">
                    "No search provider is configured. Set an API key under " <code>"[search]"</code>
                    " in the server config (e.g. " <code>"CATALERUM_SEARCH__BRAVE__API_KEY"</code>
                    ") to enable web search."
                </div>
            </Show>

            <div class="settings-field">
                <label class="settings-label">"Default provider"</label>
                <select
                    class="settings-input"
                    prop:value=move || default_provider.get()
                    on:change=move |ev| default_provider.set(event_target_value(&ev))
                >
                    <option value="">"Server default"</option>
                    {move || {
                        providers
                            .get()
                            .into_iter()
                            .filter(|p| p.enabled)
                            .map(|p| {
                                view! { <option value=p.name.clone()>{p.name.clone()}</option> }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
            </div>

            <div class="settings-field">
                <label class="settings-label">"Configured providers"</label>
                <ul class="settings-svc-list">
                    {move || {
                        providers
                            .get()
                            .into_iter()
                            .map(|p| {
                                let cls = if p.enabled {
                                    "settings-svc-state settings-svc-up"
                                } else {
                                    "settings-svc-state settings-svc-disabled"
                                };
                                let label = if p.enabled { "configured" } else { "not configured" };
                                let detail = if p.is_default {
                                    "default".to_string()
                                } else {
                                    String::new()
                                };
                                view! {
                                    <li class="settings-svc">
                                        <span class="settings-svc-name">{p.name.clone()}</span>
                                        <span class="settings-svc-detail">{detail}</span>
                                        <span class=cls>{label}</span>
                                    </li>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </ul>
            </div>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="settings-actions">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || saving.get() || loading.get()
                    on:click=move |_| save()
                >
                    {move || if saving.get() { "Saving…" } else { "Save" }}
                </button>
            </div>
        </section>
    }
}

/// **Files** — choose your default files store (SOUL §9/§13): where a chat upload
/// or a no-`?store=` op lands. The store list is the workspace's configured +
/// runtime backends; "server default" follows the `[storage]` config default. The
/// stores themselves are managed from the Files panel — this only sets *your*
/// default destination.
#[component]
fn StorageSection() -> impl IntoView {
    // The workspace's storage backends (config + runtime).
    let stores = RwSignal::new(Vec::<StorageStore>::new());
    // The caller's default-store override; "" means "use the server default".
    let default_store = RwSignal::new(String::new());

    let loading = RwSignal::new(true);
    let saving = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    // On mount: load the store list + the saved per-user default.
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::list_stores(token.as_deref()).await {
            Ok(s) => stores.set(s),
            Err(e) => error.set(Some(e.to_string())),
        }
        match rest::get_storage_settings(token.as_deref()).await {
            Ok(s) => default_store.set(s.default_store.unwrap_or_default()),
            Err(e) => error.set(Some(e.to_string())),
        }
        loading.set(false);
    });

    let has_stores = Signal::derive(move || !stores.get().is_empty());

    let save = move || {
        saving.set(true);
        error.set(None);
        notice.set(None);
        let body = StorageSettings {
            default_store: blank_to_none(default_store.get_untracked()),
        };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_storage_settings(token.as_deref(), &body).await {
                Ok(s) => {
                    default_store.set(s.default_store.unwrap_or_default());
                    // Reload the list so the "default" marker reflects the save.
                    if let Ok(list) = rest::list_stores(token.as_deref()).await {
                        stores.set(list);
                    }
                    notice.set(Some("Saved.".to_string()));
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            saving.set(false);
        });
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Choose where your uploads and any \u{201c}save to files\u{201d} action land by "
                "default — including files attached in chat. Leave it on "
                <strong>"server default"</strong>
                " to follow the deployment's configured store. Manage the stores themselves "
                "from the Files panel."
            </p>

            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>

            <Show
                when=move || !loading.get() && !has_stores.get()
                fallback=|| ().into_view()
            >
                <div class="settings-status">
                    "No storage backend is configured. Add one under " <code>"[storage]"</code>
                    " in the server config, or from the Files panel."
                </div>
            </Show>

            <div class="settings-field">
                <label class="settings-label">"Default files store"</label>
                <select
                    class="settings-input"
                    prop:value=move || default_store.get()
                    on:change=move |ev| default_store.set(event_target_value(&ev))
                >
                    <option value="">"Server default"</option>
                    {move || {
                        stores
                            .get()
                            .into_iter()
                            .map(|s| {
                                view! { <option value=s.name.clone()>{s.name.clone()}</option> }
                            })
                            .collect::<Vec<_>>()
                    }}
                </select>
            </div>

            <div class="settings-field">
                <label class="settings-label">"Stores"</label>
                <ul class="settings-svc-list">
                    {move || {
                        stores
                            .get()
                            .into_iter()
                            .map(|s| {
                                let detail = if s.is_default {
                                    format!("{} · default", s.kind)
                                } else {
                                    s.kind.clone()
                                };
                                view! {
                                    <li class="settings-svc">
                                        <span class="settings-svc-name">{s.name.clone()}</span>
                                        <span class="settings-svc-detail">{detail}</span>
                                        <span class="settings-svc-state settings-svc-up">
                                            {s.source.clone()}
                                        </span>
                                    </li>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </ul>
            </div>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || error.get().unwrap_or_default()}</div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            <div class="settings-actions">
                <button
                    class="settings-btn settings-btn-primary"
                    disabled=move || saving.get() || loading.get()
                    on:click=move |_| save()
                >
                    {move || if saving.get() { "Saving…" } else { "Save" }}
                </button>
            </div>
        </section>
    }
}

/// The CSS class for a service `state` token (`up` / `down` / `disabled`).
fn svc_state_class(state: &str) -> &'static str {
    match state {
        "up" => "settings-svc-state settings-svc-up",
        "down" => "settings-svc-state settings-svc-down",
        _ => "settings-svc-state settings-svc-disabled",
    }
}

/// **Status** — server version + live health of the LLM gateway and datastores.
#[component]
fn StatusSection() -> impl IntoView {
    let status = RwSignal::new(Option::<StatusInfo>::None);
    let loading = RwSignal::new(true);
    let error = RwSignal::new(Option::<String>::None);

    let load = move || {
        loading.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::get_status(token.as_deref()).await {
                Ok(s) => status.set(Some(s)),
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };
    load();

    view! {
        <section class="settings-section">
            <div class="settings-section-head">
                <p class="settings-blurb">
                    "Live connection health of the LLM gateway and the backing datastores."
                </p>
                <button
                    class="settings-btn"
                    disabled=move || loading.get()
                    on:click=move |_| load()
                >
                    {move || if loading.get() { "Checking…" } else { "Refresh" }}
                </button>
            </div>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-status settings-error">
                    {move || format!("Could not load status: {}", error.get().unwrap_or_default())}
                </div>
            </Show>

            {move || {
                status.get().map(|s| {
                    let version = s.version.clone();
                    let (health_cls, health_text) = if s.healthy {
                        ("settings-health settings-health-ok", "All systems operational")
                    } else {
                        ("settings-health settings-health-bad", "Degraded — a service is down")
                    };
                    view! {
                        <div class=health_cls>{health_text}</div>
                        <div class="settings-version">{format!("server version {version}")}</div>
                        <ul class="settings-svc-list">
                            {s.services
                                .into_iter()
                                .map(|svc: ServiceStatus| {
                                    let cls = svc_state_class(&svc.state);
                                    let label = svc.state.clone();
                                    view! {
                                        <li class="settings-svc">
                                            <span class="settings-svc-name">{svc.name}</span>
                                            <span class="settings-svc-detail">{svc.detail}</span>
                                            <span class=cls>{label}</span>
                                        </li>
                                    }
                                })
                                .collect::<Vec<_>>()}
                        </ul>
                    }
                })
            }}
        </section>
    }
}

/// **API keys** — list / issue / revoke the caller's workspace bearer tokens
/// (SOUL §18). A freshly-minted secret is shown **once** in a copy box.
#[component]
fn ApiKeysSection() -> impl IntoView {
    let tokens = RwSignal::new(Vec::<TokenView>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    let form_ttl = RwSignal::new("90".to_string());
    // The grant a new token is scoped to, by name; empty = full role authority.
    let form_grant = RwSignal::new(String::new());
    // The workspace's §19 grants, for the scope picker (admin-only; empty if the
    // caller can't list them, degrading the picker to "full role" only).
    let grants = RwSignal::new(Vec::<Grant>::new());
    let busy = RwSignal::new(false);
    let form_error = RwSignal::new(Option::<String>::None);
    // The just-minted raw secret, shown once.
    let fresh_token = RwSignal::new(Option::<String>::None);

    let load = move || {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_tokens(token.as_deref()).await {
                Ok(list) => tokens.set(list),
                Err(e) => load_error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };
    load();

    // Populate the grant picker once (best-effort; a non-admin who can't read
    // grants just sees the "full role" option).
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(list) = rest::list_grants(token.as_deref()).await {
            grants.set(list);
        }
    });

    let create = move || {
        let ttl_days: i64 = form_ttl.get_untracked().trim().parse().unwrap_or(90);
        form_error.set(None);
        if ttl_days < 1 {
            form_error.set(Some("Lifetime must be at least 1 day.".to_string()));
            return;
        }
        // Empty picker value → a full role-authority token (no grant field).
        let grant = {
            let g = form_grant.get_untracked();
            let g = g.trim();
            if g.is_empty() {
                None
            } else {
                Some(g.to_string())
            }
        };
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::create_token(token.as_deref(), &CreateToken { ttl_days, grant }).await {
                Ok(created) => {
                    fresh_token.set(Some(created.token));
                    load();
                }
                Err(e) => form_error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };

    let revoke = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            if rest::revoke_token(token.as_deref(), &id).await.is_ok() {
                tokens.update(|list| list.retain(|t| t.id != id));
            }
        });
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        create();
    };

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "Long-lived bearer tokens for scripts, CI, or MCP clients. A token carries your "
                "current role in this workspace, or — scope it to a capability grant — the grant's "
                "attenuated authority instead (never more than you hold). The secret is shown once "
                "at creation; store it now."
            </p>

            // The one-time secret reveal.
            <Show when=move || fresh_token.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-token-reveal">
                    <div class="settings-token-warn">
                        "Copy this token now — it will not be shown again:"
                    </div>
                    <code class="settings-token-value">
                        {move || fresh_token.get().unwrap_or_default()}
                    </code>
                    <button class="settings-btn" on:click=move |_| fresh_token.set(None)>
                        "Dismiss"
                    </button>
                </div>
            </Show>

            <h3 class="settings-section-title">"Issue a token"</h3>
            <form class="settings-form settings-form-row" on:submit=on_submit>
                <div class="settings-field">
                    <label class="settings-label">"Lifetime (days, max 365)"</label>
                    <input
                        class="settings-input settings-input-narrow"
                        r#type="number"
                        min="1"
                        max="365"
                        prop:value=move || form_ttl.get()
                        on:input=move |ev| form_ttl.set(event_target_value(&ev))
                    />
                </div>
                <div class="settings-field">
                    <label class="settings-label">"Scope (capability grant)"</label>
                    <select
                        class="settings-input"
                        prop:value=move || form_grant.get()
                        on:change=move |ev| form_grant.set(event_target_value(&ev))
                    >
                        <option value="">"Full role authority"</option>
                        <For
                            each=move || grants.get()
                            key=|g| g.id.clone()
                            children=move |g: Grant| {
                                let name = g.name.clone();
                                view! { <option value=name.clone()>{name.clone()}</option> }
                            }
                        />
                    </select>
                </div>
                <button
                    class="settings-btn settings-btn-primary"
                    type="submit"
                    disabled=move || busy.get()
                >
                    {move || if busy.get() { "Issuing…" } else { "Issue token" }}
                </button>
            </form>
            <Show when=move || form_error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">{move || form_error.get().unwrap_or_default()}</div>
            </Show>

            <h3 class="settings-section-title">"Active tokens"</h3>
            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>
            <Show when=move || load_error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-status settings-error">
                    {move || format!("Could not load tokens: {}", load_error.get().unwrap_or_default())}
                </div>
            </Show>
            <Show
                when=move || {
                    !loading.get() && load_error.with(Option::is_none) && tokens.with(Vec::is_empty)
                }
                fallback=|| ().into_view()
            >
                <div class="settings-empty">"No active tokens."</div>
            </Show>
            <ul class="settings-token-list">
                <For
                    each=move || tokens.get()
                    key=|t| t.id.clone()
                    children=move |t: TokenView| {
                        let id = t.id.clone();
                        let id_short: String = id.chars().take(8).collect();
                        let expires = t.expires_at.clone().unwrap_or_default();
                        // A scoped token shows its grant; a role token shows nothing.
                        let grant_label = t
                            .grant
                            .clone()
                            .map(|g| format!("grant: {g}"))
                            .unwrap_or_default();
                        view! {
                            <li class="settings-token">
                                <span class="settings-token-id" title=id.clone()>
                                    {format!("{id_short}…")}
                                </span>
                                <span class="settings-token-grant">{grant_label}</span>
                                <span class="settings-token-exp">
                                    {if expires.is_empty() {
                                        String::new()
                                    } else {
                                        format!("expires {}", fmt_date(&expires))
                                    }}
                                </span>
                                <button
                                    class="settings-btn settings-btn-danger"
                                    on:click=move |_| revoke(id.clone())
                                >
                                    "Revoke"
                                </button>
                            </li>
                        }
                    }
                />
            </ul>
        </section>
    }
}

// ---------------------------------------------------------------------------
// MCP servers (SOUL §26) — catalerum as an MCP *client* connecting out.
// ---------------------------------------------------------------------------

/// Split a textarea into a trimmed, non-empty list of lines (used for `args` and
/// the `tools` allow-list).
fn lines_to_vec(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Join a list back into one-per-line textarea content.
fn vec_to_lines(items: &[String]) -> String {
    items.join("\n")
}

/// Parse `KEY=VALUE` textarea lines into an env map. A blank value is kept (the
/// server preserves the stored value for an existing key on update); a line with
/// no `=` is treated as a bare key with an empty value.
fn parse_env(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (line, ""),
        };
        if !key.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

/// Render the redacted `env` keys as editable `KEY=` lines (values blanked — the
/// server keeps the stored value when a key's value is left blank on update).
fn env_keys_to_text(keys: &[String]) -> String {
    keys.iter()
        .map(|k| format!("{k}="))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a transport string means HTTP (else stdio) — mirrors the server.
fn web_transport_is_http(transport: &str) -> bool {
    matches!(
        transport.trim().to_ascii_lowercase().as_str(),
        "http" | "https" | "sse" | "streamable-http"
    )
}

/// **Computer agents** — the installed daemons on servers/desktops this workspace
/// can drive with the `computer_*` tools (SOUL §19/§20). Each row shows the
/// machine's platform, an online/offline badge, its served directories + exec
/// policy, and last-seen. "Enroll" mints a one-time token (shown once) the user
/// pastes into `catalerum-agent enroll …`; "Revoke" drops the token and any live
/// connection. Enroll/revoke are workspace-admin gated server-side.
#[component]
fn ComputerAgentsSection() -> impl IntoView {
    let dialogs = use_dialogs();
    let agents = RwSignal::new(Vec::<ComputerAgentView>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Enroll form + the one-time token reveal.
    let form_open = RwSignal::new(false);
    let f_name = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let form_error = RwSignal::new(Option::<String>::None);
    let fresh = RwSignal::new(Option::<crate::api::EnrolledComputerAgent>::None);

    let load = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_computer_agents(token.as_deref()).await {
                Ok(list) => {
                    agents.set(list);
                    load_error.set(None);
                }
                Err(e) => load_error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };
    load();

    // Poll while the tab is open so the online badges track connect/disconnect.
    if let Ok(handle) = set_interval_with_handle(load, std::time::Duration::from_secs(12)) {
        on_cleanup(move || handle.clear());
    }

    let submit = move |_| {
        let name = f_name.get().trim().to_string();
        if name.is_empty() {
            form_error.set(Some("a name is required".into()));
            return;
        }
        busy.set(true);
        form_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = EnrollComputerAgent { name };
            match rest::enroll_computer_agent(token.as_deref(), &body).await {
                Ok(created) => {
                    fresh.set(Some(created));
                    form_open.set(false);
                    f_name.set(String::new());
                    load();
                }
                Err(e) => form_error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };

    let revoke = move |agent: ComputerAgentView| {
        let id = agent.id.clone();
        let name = agent.name.clone();
        dialogs.confirm(
            ConfirmSpec::danger(
                "Revoke computer agent",
                format!(
                    "Revoke \"{name}\"? Its token stops working immediately and any live \
                     connection is dropped. You'll need to re-enroll to reconnect it."
                ),
                "Revoke",
            ),
            move || {
                let id = id.clone();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::revoke_computer_agent(token.as_deref(), &id).await {
                        Ok(()) => agents.update(|list| list.retain(|a| a.id != id)),
                        Err(e) => load_error.set(Some(e.to_string())),
                    }
                });
            },
        );
    };

    view! {
        <div class="settings-section">
            <div class="settings-section-title">"Computer agents"</div>
            <p class="settings-blurb">
                "Install the "<code>"catalerum-agent"</code>" daemon on a server or desktop and \
                 enroll it here to let the assistant read/write files, search, run commands, and \
                 (optionally) control the desktop on that machine — all confined to the directories \
                 and policy you configure on the machine itself. Commands run through the machine's \
                 exec policy (an auto safety classifier, or your explicit approval)."
            </p>

            <Show when=move || load_error.get().is_some()>
                <div class="settings-form-error">
                    {move || load_error.get().unwrap_or_default()}
                </div>
            </Show>

            // One-time enrollment token reveal.
            <Show when=move || fresh.get().is_some() fallback=|| ().into_view()>
                {move || {
                    let created = fresh.get().unwrap_or_default();
                    let tok = created.token.clone();
                    let tok_for_copy = tok.clone();
                    // Prefill --server with the API origin the SPA itself talks to —
                    // the daemon dials the API, not this web UI's address (pasting the
                    // web origin yields a plain 200 page instead of the WS upgrade).
                    let cmd = format!(
                        "catalerum-agent enroll --server {} --token {tok} \
                         --name \"{}\" --rw /path/to/dir",
                        crate::api::api_base(),
                        created.name
                    );
                    view! {
                        <div class="settings-token-reveal">
                            <div class="settings-token-warn">
                                "Copy this enrollment token now — it will not be shown again. \
                                 Run this on the machine (--server is prefilled with this \
                                 deployment's API address — the daemon dials the API, not \
                                 this web app's URL):"
                            </div>
                            <code class="settings-token-value">{cmd}</code>
                            <div class="settings-actions">
                                {copy_button(
                                    move || tok_for_copy.clone(),
                                    "Copy token",
                                    "Copied!",
                                    "",
                                )}
                                <button
                                    class="settings-btn"
                                    on:click=move |_| fresh.set(None)
                                >
                                    "Dismiss"
                                </button>
                            </div>
                        </div>
                    }
                }}
            </Show>

            // Enroll form (name → token).
            <Show when=move || form_open.get() fallback=move || {
                view! {
                    <div class="settings-actions">
                        <button
                            class="settings-btn settings-btn-primary"
                            on:click=move |_| { form_error.set(None); form_open.set(true); }
                        >
                            "Enroll a computer"
                        </button>
                        <button class="settings-btn" on:click=move |_| load()>"Refresh"</button>
                    </div>
                }
            }>
                <div class="settings-field">
                    <label class="settings-label" for="ca-name">"Machine name"</label>
                    <input
                        id="ca-name"
                        class="settings-input"
                        placeholder="build-server"
                        prop:value=move || f_name.get()
                        on:input=move |ev| f_name.set(event_target_value(&ev))
                    />
                </div>
                <Show when=move || form_error.get().is_some()>
                    <div class="settings-form-error">
                        {move || form_error.get().unwrap_or_default()}
                    </div>
                </Show>
                <div class="settings-actions">
                    <button
                        class="settings-btn settings-btn-primary"
                        prop:disabled=move || busy.get()
                        on:click=submit
                    >
                        {move || if busy.get() { "Enrolling…" } else { "Enroll" }}
                    </button>
                    <button
                        class="settings-btn"
                        on:click=move |_| { form_open.set(false); form_error.set(None); }
                    >
                        "Cancel"
                    </button>
                </div>
            </Show>

            // The enrolled-agent list.
            <Show
                when=move || !loading.get() && agents.get().is_empty()
                fallback=|| ().into_view()
            >
                <p class="settings-empty">"No computer agents enrolled yet."</p>
            </Show>
            <ul class="mcp-srv-list">
                <For
                    each=move || agents.get()
                    key=|a| (a.id.clone(), a.online, a.last_seen_at.clone())
                    children=move |a| {
                        let (status_class, status_text) = if a.online {
                            ("mcp-srv-status is-on", "online".to_string())
                        } else {
                            ("mcp-srv-status is-off", "offline".to_string())
                        };
                        let platform = a
                            .platform
                            .clone()
                            .filter(|p| !p.is_empty())
                            .unwrap_or_else(|| "unknown".into());
                        let detail = a.capabilities.as_ref().map(|c| {
                            let dirs = c
                                .dirs
                                .iter()
                                .map(|d| format!("{} ({})", d.path, if d.mode == "read_write" { "rw" } else { "ro" }))
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!(
                                "exec: {}{}{}",
                                if c.exec_policy.is_empty() { "auto" } else { &c.exec_policy },
                                if c.desktop { " · desktop" } else { "" },
                                if dirs.is_empty() { String::new() } else { format!(" · dirs: {dirs}") },
                            )
                        });
                        let last_seen = a
                            .last_seen_at
                            .clone()
                            .map(|t| format!("last seen {t}"))
                            .unwrap_or_else(|| "never connected".into());
                        let for_revoke = a.clone();
                        view! {
                            <li class="mcp-srv">
                                <div class="mcp-srv-head">
                                    <span class="mcp-srv-name">{a.name.clone()}</span>
                                    <span class="mcp-srv-badge">{platform}</span>
                                    <span class=status_class>{status_text}</span>
                                </div>
                                {detail
                                    .map(|d| view! { <div class="mcp-srv-target">{d}</div> })}
                                <div class="mcp-srv-target">{last_seen}</div>
                                <div class="settings-actions">
                                    <button
                                        class="settings-btn settings-btn-danger"
                                        on:click=move |_| revoke(for_revoke.clone())
                                    >
                                        "Revoke"
                                    </button>
                                </div>
                            </li>
                        }
                    }
                />
            </ul>
        </div>
    }
}

/// **MCP servers** — register the external MCP servers this workspace connects
/// *out* to as a client (SOUL §26). Each row shows its transport, target, and
/// live connection status; the editor adds/edits one (stdio: a spawned command;
/// http: a URL with optional bearer/header/oauth2 auth). Secrets never round-trip:
/// a stored secret shows as "leave blank to keep" and is preserved on save.
/// Lifecycle is workspace-admin gated server-side.
#[component]
fn McpServersSection() -> impl IntoView {
    let dialogs = use_dialogs();
    let servers = RwSignal::new(Vec::<McpServerView>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Editor state.
    let form_open = RwSignal::new(false);
    let form_is_new = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let form_error = RwSignal::new(Option::<String>::None);
    let notice = RwSignal::new(Option::<String>::None);

    let f_name = RwSignal::new(String::new());
    let f_transport = RwSignal::new("stdio".to_string());
    let f_command = RwSignal::new(String::new());
    let f_args = RwSignal::new(String::new());
    let f_env = RwSignal::new(String::new());
    let f_url = RwSignal::new(String::new());
    let f_tools = RwSignal::new(String::new());
    let f_enabled = RwSignal::new(true);
    let f_auth_kind = RwSignal::new("none".to_string());
    let f_token = RwSignal::new(String::new());
    let f_header_name = RwSignal::new(String::new());
    let f_header_value = RwSignal::new(String::new());
    let f_token_url = RwSignal::new(String::new());
    let f_grant_type = RwSignal::new("client_credentials".to_string());
    let f_client_id = RwSignal::new(String::new());
    let f_client_secret = RwSignal::new(String::new());
    let f_refresh_token = RwSignal::new(String::new());
    let f_scope = RwSignal::new(String::new());
    // Which secrets are already stored (edit mode) → "leave blank to keep" hints.
    let f_has_token = RwSignal::new(false);
    let f_has_header_value = RwSignal::new(false);
    let f_has_client_secret = RwSignal::new(false);
    let f_has_refresh_token = RwSignal::new(false);

    let load = move || {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_mcp_servers(token.as_deref()).await {
                Ok(list) => servers.set(list),
                Err(e) => load_error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    };
    load();

    // Reset the editor to a blank "new server" and open it.
    let open_new = move || {
        form_is_new.set(true);
        f_name.set(String::new());
        f_transport.set("stdio".to_string());
        f_command.set(String::new());
        f_args.set(String::new());
        f_env.set(String::new());
        f_url.set(String::new());
        f_tools.set(String::new());
        f_enabled.set(true);
        f_auth_kind.set("none".to_string());
        f_token.set(String::new());
        f_header_name.set(String::new());
        f_header_value.set(String::new());
        f_token_url.set(String::new());
        f_grant_type.set("client_credentials".to_string());
        f_client_id.set(String::new());
        f_client_secret.set(String::new());
        f_refresh_token.set(String::new());
        f_scope.set(String::new());
        f_has_token.set(false);
        f_has_header_value.set(false);
        f_has_client_secret.set(false);
        f_has_refresh_token.set(false);
        form_error.set(None);
        notice.set(None);
        form_open.set(true);
    };

    // Load an existing server's redacted view into the editor (secrets stay blank;
    // any http-flavoured transport collapses to "http", the one the picker offers).
    let open_edit = move |s: McpServerView| {
        form_is_new.set(false);
        f_name.set(s.name.clone());
        f_transport.set(if web_transport_is_http(&s.transport) {
            "http".to_string()
        } else {
            "stdio".to_string()
        });
        f_command.set(s.command.clone());
        f_args.set(vec_to_lines(&s.args));
        f_env.set(env_keys_to_text(&s.env_keys));
        f_url.set(s.url.clone());
        f_tools.set(vec_to_lines(&s.tools));
        f_enabled.set(s.enabled);
        f_auth_kind.set(if s.auth.kind.trim().is_empty() {
            "none".to_string()
        } else {
            s.auth.kind.clone()
        });
        f_header_name.set(s.auth.header_name.clone());
        f_token_url.set(s.auth.token_url.clone());
        f_grant_type.set(if s.auth.grant_type.trim().is_empty() {
            "client_credentials".to_string()
        } else {
            s.auth.grant_type.clone()
        });
        f_client_id.set(s.auth.client_id.clone());
        f_scope.set(s.auth.scope.clone());
        f_token.set(String::new());
        f_header_value.set(String::new());
        f_client_secret.set(String::new());
        f_refresh_token.set(String::new());
        f_has_token.set(s.auth.has_token);
        f_has_header_value.set(s.auth.has_header_value);
        f_has_client_secret.set(s.auth.has_client_secret);
        f_has_refresh_token.set(s.auth.has_refresh_token);
        form_error.set(None);
        notice.set(None);
        form_open.set(true);
    };

    let save = move || {
        form_error.set(None);
        notice.set(None);
        let name = f_name.get_untracked().trim().to_string();
        if name.is_empty() {
            form_error.set(Some("Name is required.".to_string()));
            return;
        }
        let transport = f_transport.get_untracked();
        let is_http = transport == "http";
        if is_http && f_url.get_untracked().trim().is_empty() {
            form_error.set(Some("URL is required for an http server.".to_string()));
            return;
        }
        if !is_http && f_command.get_untracked().trim().is_empty() {
            form_error.set(Some("Command is required for a stdio server.".to_string()));
            return;
        }
        let auth = if is_http {
            McpAuthBody {
                kind: f_auth_kind.get_untracked(),
                token: f_token.get_untracked(),
                header_name: f_header_name.get_untracked(),
                header_value: f_header_value.get_untracked(),
                token_url: f_token_url.get_untracked(),
                grant_type: f_grant_type.get_untracked(),
                client_id: f_client_id.get_untracked(),
                client_secret: f_client_secret.get_untracked(),
                refresh_token: f_refresh_token.get_untracked(),
                scope: f_scope.get_untracked(),
            }
        } else {
            McpAuthBody {
                kind: "none".to_string(),
                ..Default::default()
            }
        };
        let body = McpServerBody {
            name: name.clone(),
            transport,
            command: f_command.get_untracked(),
            args: lines_to_vec(&f_args.get_untracked()),
            env: parse_env(&f_env.get_untracked()),
            url: f_url.get_untracked(),
            auth,
            enabled: f_enabled.get_untracked(),
            tools: lines_to_vec(&f_tools.get_untracked()),
        };
        let is_new = form_is_new.get_untracked();
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let res = if is_new {
                rest::create_mcp_server(token.as_deref(), &body).await
            } else {
                rest::update_mcp_server(token.as_deref(), &name, &body).await
            };
            match res {
                Ok(view) => {
                    let msg = match &view.connect_error {
                        Some(e) => format!("Saved, but connecting failed: {e}"),
                        None if !view.enabled => "Saved (disabled — not connected).".to_string(),
                        None if view.connected => "Saved and connected.".to_string(),
                        None => "Saved.".to_string(),
                    };
                    notice.set(Some(msg));
                    form_open.set(false);
                    load();
                }
                Err(e) => form_error.set(Some(e.to_string())),
            }
            busy.set(false);
        });
    };

    let remove = move |name: String| {
        dialogs.confirm(
            ConfirmSpec::danger(
                "Delete MCP server",
                format!(
                    "Disconnect and remove “{name}”? Its imported tools disappear immediately."
                ),
                "Delete",
            ),
            move || {
                let name = name.clone();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::delete_mcp_server(token.as_deref(), &name).await {
                        Ok(()) => servers.update(|list| list.retain(|s| s.name != name)),
                        Err(e) => load_error.set(Some(e.to_string())),
                    }
                });
            },
        );
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // "leave blank to keep" placeholder for a secret that is already stored.
    let secret_placeholder = |stored: RwSignal<bool>| {
        Signal::derive(move || {
            if stored.get() {
                "leave blank to keep".to_string()
            } else {
                String::new()
            }
        })
    };
    let token_ph = secret_placeholder(f_has_token);
    let header_ph = secret_placeholder(f_has_header_value);
    let secret_ph = secret_placeholder(f_has_client_secret);
    let refresh_ph = secret_placeholder(f_has_refresh_token);

    view! {
        <section class="settings-section">
            <p class="settings-blurb">
                "External MCP servers this workspace connects " <strong>"out"</strong> " to as a "
                "client (SOUL §26). Each enabled server's tools are imported as "
                <code>"{name}_{tool}"</code> " and become callable by the agent. A "
                <strong>"stdio"</strong> " server spawns a local command; an " <strong>"http"</strong>
                " server connects to a URL (with optional auth). Changes connect live — no restart."
            </p>

            <Show when=move || load_error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-error">
                    {move || load_error.get().unwrap_or_default()}
                </div>
            </Show>
            <Show when=move || notice.with(Option::is_some) fallback=|| ().into_view()>
                <div class="settings-form-notice">{move || notice.get().unwrap_or_default()}</div>
            </Show>

            // The add/edit form.
            <Show when=move || form_open.get() fallback=|| ().into_view()>
                <form class="settings-form" on:submit=on_submit>
                    <h3 class="settings-section-title">
                        {move || if form_is_new.get() { "Add MCP server" } else { "Edit MCP server" }}
                    </h3>
                    <div class="settings-field">
                        <label class="settings-label">"Name"</label>
                        <input
                            class="settings-input"
                            r#type="text"
                            placeholder="e.g. playwright"
                            prop:value=move || f_name.get()
                            disabled=move || !form_is_new.get()
                            on:input=move |ev| f_name.set(event_target_value(&ev))
                        />
                    </div>
                    <div class="settings-field">
                        <label class="settings-label">"Transport"</label>
                        <select
                            class="settings-input"
                            prop:value=move || f_transport.get()
                            on:change=move |ev| f_transport.set(event_target_value(&ev))
                        >
                            <option value="stdio">"stdio (spawn a command)"</option>
                            <option value="http">"http (connect to a URL)"</option>
                        </select>
                    </div>

                    // --- stdio fields ---
                    <Show
                        when=move || f_transport.get() != "http"
                        fallback=|| ().into_view()
                    >
                        <div class="settings-field">
                            <label class="settings-label">"Command"</label>
                            <input
                                class="settings-input"
                                r#type="text"
                                placeholder="e.g. npx"
                                prop:value=move || f_command.get()
                                on:input=move |ev| f_command.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="settings-field">
                            <label class="settings-label">"Arguments (one per line)"</label>
                            <textarea
                                class="settings-input"
                                rows="3"
                                placeholder="@playwright/mcp@latest"
                                prop:value=move || f_args.get()
                                on:input=move |ev| f_args.set(event_target_value(&ev))
                            ></textarea>
                        </div>
                        <div class="settings-field">
                            <label class="settings-label">"Environment (KEY=VALUE, one per line)"</label>
                            <textarea
                                class="settings-input"
                                rows="2"
                                placeholder="API_KEY=…"
                                prop:value=move || f_env.get()
                                on:input=move |ev| f_env.set(event_target_value(&ev))
                            ></textarea>
                            <p class="settings-blurb">
                                "Values are hidden after saving; leave a value blank to keep the stored one."
                            </p>
                        </div>
                    </Show>

                    // --- http fields ---
                    <Show
                        when=move || f_transport.get() == "http"
                        fallback=|| ().into_view()
                    >
                        <div class="settings-field">
                            <label class="settings-label">"URL"</label>
                            <input
                                class="settings-input"
                                r#type="text"
                                placeholder="https://host/mcp"
                                prop:value=move || f_url.get()
                                on:input=move |ev| f_url.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="settings-field">
                            <label class="settings-label">"Authentication"</label>
                            <select
                                class="settings-input"
                                prop:value=move || f_auth_kind.get()
                                on:change=move |ev| f_auth_kind.set(event_target_value(&ev))
                            >
                                <option value="none">"None"</option>
                                <option value="bearer">"Bearer token"</option>
                                <option value="header">"Custom header"</option>
                                <option value="oauth2">"OAuth2"</option>
                            </select>
                        </div>

                        <Show
                            when=move || f_auth_kind.get() == "bearer"
                            fallback=|| ().into_view()
                        >
                            <div class="settings-field">
                                <label class="settings-label">"Bearer token"</label>
                                <input
                                    class="settings-input"
                                    r#type="password"
                                    prop:value=move || f_token.get()
                                    placeholder=move || token_ph.get()
                                    on:input=move |ev| f_token.set(event_target_value(&ev))
                                />
                            </div>
                        </Show>

                        <Show
                            when=move || f_auth_kind.get() == "header"
                            fallback=|| ().into_view()
                        >
                            <div class="settings-field">
                                <label class="settings-label">"Header name"</label>
                                <input
                                    class="settings-input"
                                    r#type="text"
                                    placeholder="X-Api-Key"
                                    prop:value=move || f_header_name.get()
                                    on:input=move |ev| f_header_name.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Header value"</label>
                                <input
                                    class="settings-input"
                                    r#type="password"
                                    prop:value=move || f_header_value.get()
                                    placeholder=move || header_ph.get()
                                    on:input=move |ev| f_header_value.set(event_target_value(&ev))
                                />
                            </div>
                        </Show>

                        <Show
                            when=move || f_auth_kind.get() == "oauth2"
                            fallback=|| ().into_view()
                        >
                            <div class="settings-field">
                                <label class="settings-label">"Token URL"</label>
                                <input
                                    class="settings-input"
                                    r#type="text"
                                    placeholder="https://host/oauth/token"
                                    prop:value=move || f_token_url.get()
                                    on:input=move |ev| f_token_url.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Grant type"</label>
                                <select
                                    class="settings-input"
                                    prop:value=move || f_grant_type.get()
                                    on:change=move |ev| f_grant_type.set(event_target_value(&ev))
                                >
                                    <option value="client_credentials">"client_credentials"</option>
                                    <option value="refresh_token">"refresh_token"</option>
                                </select>
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Client id"</label>
                                <input
                                    class="settings-input"
                                    r#type="text"
                                    prop:value=move || f_client_id.get()
                                    on:input=move |ev| f_client_id.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Client secret"</label>
                                <input
                                    class="settings-input"
                                    r#type="password"
                                    prop:value=move || f_client_secret.get()
                                    placeholder=move || secret_ph.get()
                                    on:input=move |ev| f_client_secret.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Refresh token"</label>
                                <input
                                    class="settings-input"
                                    r#type="password"
                                    prop:value=move || f_refresh_token.get()
                                    placeholder=move || refresh_ph.get()
                                    on:input=move |ev| f_refresh_token.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="settings-field">
                                <label class="settings-label">"Scope (space-separated)"</label>
                                <input
                                    class="settings-input"
                                    r#type="text"
                                    prop:value=move || f_scope.get()
                                    on:input=move |ev| f_scope.set(event_target_value(&ev))
                                />
                            </div>
                        </Show>
                    </Show>

                    <div class="settings-field">
                        <label class="settings-label">"Tools allow-list (one per line, blank = all)"</label>
                        <textarea
                            class="settings-input"
                            rows="2"
                            prop:value=move || f_tools.get()
                            on:input=move |ev| f_tools.set(event_target_value(&ev))
                        ></textarea>
                    </div>
                    <div class="settings-field">
                        <label class="settings-check">
                            <input
                                type="checkbox"
                                prop:checked=move || f_enabled.get()
                                on:change=move |ev| f_enabled.set(event_target_checked(&ev))
                            />
                            " Enabled (connect this server)"
                        </label>
                    </div>

                    <Show when=move || form_error.with(Option::is_some) fallback=|| ().into_view()>
                        <div class="settings-form-error">
                            {move || form_error.get().unwrap_or_default()}
                        </div>
                    </Show>

                    <div class="settings-actions">
                        <button
                            class="settings-btn settings-btn-primary"
                            type="submit"
                            disabled=move || busy.get()
                        >
                            {move || if busy.get() { "Saving…" } else { "Save server" }}
                        </button>
                        <button
                            class="settings-btn"
                            r#type="button"
                            on:click=move |_| form_open.set(false)
                        >
                            "Cancel"
                        </button>
                    </div>
                </form>
            </Show>

            // The add button (hidden while the form is open).
            <Show when=move || !form_open.get() fallback=|| ().into_view()>
                <div class="settings-actions">
                    <button
                        class="settings-btn settings-btn-primary"
                        on:click=move |_| open_new()
                    >
                        "Add MCP server"
                    </button>
                </div>
            </Show>

            <h3 class="settings-section-title">"Registered servers"</h3>
            <Show when=move || loading.get() fallback=|| ().into_view()>
                <div class="settings-status">"Loading…"</div>
            </Show>
            <Show
                when=move || !loading.get() && servers.with(Vec::is_empty)
                fallback=|| ().into_view()
            >
                <div class="settings-empty">"No external MCP servers registered."</div>
            </Show>
            <ul class="mcp-srv-list">
                <For
                    each=move || servers.get()
                    key=|s| s.name.clone()
                    children=move |s: McpServerView| {
                        let for_edit = s.clone();
                        let name_del = s.name.clone();
                        let is_http = web_transport_is_http(&s.transport);
                        let target = if is_http {
                            s.url.clone()
                        } else {
                            let mut cmd = s.command.clone();
                            if !s.args.is_empty() {
                                cmd.push(' ');
                                cmd.push_str(&s.args.join(" "));
                            }
                            cmd
                        };
                        let (status_class, status_text) = if !s.enabled {
                            ("mcp-srv-status is-disabled", "disabled".to_string())
                        } else if s.connected {
                            ("mcp-srv-status is-on", "connected".to_string())
                        } else {
                            ("mcp-srv-status is-off", "not connected".to_string())
                        };
                        let err = s.connect_error.clone();
                        view! {
                            <li class="mcp-srv">
                                <div class="mcp-srv-head">
                                    <span class="mcp-srv-name">{s.name.clone()}</span>
                                    <span class="mcp-srv-badge">
                                        {if is_http { "http" } else { "stdio" }}
                                    </span>
                                    <span class=status_class>{status_text}</span>
                                    <div class="mcp-srv-actions">
                                        <button
                                            class="settings-btn"
                                            on:click=move |_| open_edit(for_edit.clone())
                                        >
                                            "Edit"
                                        </button>
                                        <button
                                            class="settings-btn settings-btn-danger"
                                            on:click=move |_| remove(name_del.clone())
                                        >
                                            "Delete"
                                        </button>
                                    </div>
                                </div>
                                <Show
                                    when={let t = target.clone(); move || !t.is_empty()}
                                    fallback=|| ().into_view()
                                >
                                    <div class="mcp-srv-target">{target.clone()}</div>
                                </Show>
                                <Show
                                    when={let e = err.clone(); move || e.is_some()}
                                    fallback=|| ().into_view()
                                >
                                    <div class="mcp-srv-err">
                                        {format!("connect error: {}", err.clone().unwrap_or_default())}
                                    </div>
                                </Show>
                            </li>
                        }
                    }
                />
            </ul>
        </section>
    }
}

/// Trim an RFC 3339 timestamp to its `YYYY-MM-DD` date (no chrono in wasm).
fn fmt_date(rfc3339: &str) -> String {
    rfc3339.split('T').next().unwrap_or(rfc3339).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn membership(role: &str, active: bool) -> WorkspaceMembership {
        WorkspaceMembership {
            id: "w1".into(),
            name: "Home".into(),
            slug: "home".into(),
            role: role.into(),
            organisation_id: "o1".into(),
            active,
        }
    }

    #[test]
    fn admin_chrome_is_only_the_operational_and_token_tabs() {
        // LLM gateway + Status (operational config views), API keys (the dangerous
        // token panel), and MCP servers (workspace-operational config write) are
        // the admin chrome that collapses (SOUL §29).
        for chrome in [
            SettingsTab::Llm,
            SettingsTab::Llmleaf,
            SettingsTab::Status,
            SettingsTab::ApiKeys,
            SettingsTab::McpServers,
            SettingsTab::ComputerAgents,
            SettingsTab::Users,
        ] {
            assert!(is_admin_chrome(chrome), "{chrome:?} is admin chrome");
        }
        // Per-user preference / info tabs are never chrome — every member keeps
        // them in every mode (MCP clients included: connecting one's own agents
        // is per-user, and the token mint it offers is self-scoped server-side).
        for keep in [
            SettingsTab::About,
            SettingsTab::Appearance,
            SettingsTab::General,
            SettingsTab::Models,
            SettingsTab::Search,
            SettingsTab::Storage,
            SettingsTab::McpClients,
        ] {
            assert!(!is_admin_chrome(keep), "{keep:?} is a per-user preference");
        }
    }

    #[test]
    fn settings_tabs_collapse_only_for_a_multi_user_non_admin_member() {
        let full = SettingsTab::all().to_vec();
        let curated: Vec<SettingsTab> = full
            .iter()
            .copied()
            .filter(|t| !is_admin_chrome(*t))
            .collect();

        // single_user → full depth for everyone (member included, sole human).
        assert_eq!(settings_tabs_for("single_user", "member", true), full);
        assert_eq!(settings_tabs_for("single_user", "owner", true), full);
        // multi_user admin/owner → full depth.
        assert_eq!(settings_tabs_for("multi_user", "owner", true), full);
        assert_eq!(settings_tabs_for("multi_user", "admin", true), full);
        // multi_user member/viewer → the curated subset (chrome collapsed).
        assert_eq!(settings_tabs_for("multi_user", "member", true), curated);
        assert_eq!(settings_tabs_for("multi_user", "viewer", true), curated);
        // The curated set drops exactly the admin-chrome tabs and keeps the
        // seven per-user preference / info tabs.
        assert_eq!(curated.len(), 7);
        assert_eq!(
            full.len() - curated.len(),
            SettingsTab::all()
                .iter()
                .filter(|t| is_admin_chrome(**t))
                .count()
        );
        assert!(!curated.contains(&SettingsTab::ApiKeys));
        assert!(curated.contains(&SettingsTab::Appearance));
        assert!(curated.contains(&SettingsTab::McpClients));

        // Undetectable role (empty / unknown) → full depth, even in multi_user:
        // we collapse only for a *known* non-admin (fail toward showing more).
        assert_eq!(settings_tabs_for("multi_user", "", true), full);
        assert_eq!(settings_tabs_for("multi_user", "robot", true), full);
        // An absent/blank mode defaults to single_user → full depth.
        assert_eq!(settings_tabs_for("", "member", true), full);

        // The config capability removes the control-plane tab in every mode and
        // role; older servers omit it and therefore also take this false path.
        let disabled = settings_tabs_for("single_user", "owner", false);
        assert!(!disabled.contains(&SettingsTab::Llmleaf));
        assert_eq!(disabled.len(), full.len() - 1);
    }

    #[test]
    fn active_role_reads_the_active_membership() {
        let list = vec![
            membership("owner", false),
            membership("member", true),
            membership("viewer", false),
        ];
        assert_eq!(active_workspace_role(&list), "member");
        // No active membership (or an empty list) → empty, which shows full depth.
        assert_eq!(active_workspace_role(&[membership("admin", false)]), "");
        assert_eq!(active_workspace_role(&[]), "");
    }

    #[test]
    fn guided_provider_form_builds_env_indirection() {
        let spec = provider_topology_spec(
            " openai-main ",
            "openai",
            "env:OPENAI_API_KEY",
            "https://proxy.example/v1",
            "oa",
        )
        .unwrap();
        assert_eq!(spec["name"], "openai-main");
        assert_eq!(spec["kind"], "openai");
        assert_eq!(spec["credential"], "env:OPENAI_API_KEY");
        assert_eq!(spec["endpoint"], "https://proxy.example/v1");
        assert_eq!(spec["prefix"], "oa");
    }

    #[test]
    fn guided_route_form_preserves_fallback_order() {
        let spec = route_topology_spec(
            "smart",
            vec![
                ("anthropic-main".into(), "claude-sonnet-4".into()),
                ("openai-main".into(), String::new()),
            ],
        )
        .unwrap();
        assert_eq!(spec["model"], "smart");
        let targets = spec["targets"].as_array().unwrap();
        assert_eq!(targets[0]["provider"], "anthropic-main");
        assert_eq!(targets[0]["model"], "claude-sonnet-4");
        assert_eq!(targets[1]["provider"], "openai-main");
        assert!(targets[1].get("model").is_none());
        assert!(route_topology_spec("smart", vec![(String::new(), String::new())]).is_err());
    }
}
