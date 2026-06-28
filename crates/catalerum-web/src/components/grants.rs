//! The Grants panel (SOUL §19, §12 — capability-grant builder, admin-only).
//!
//! A two-pane workbench panel: a left list of the workspace's grants and a right
//! editor with create-or-replace / delete. It is a thin client of the grant REST
//! surface (`/grants`, `/grants/{id}`) — every call carries the dev session token
//! and is workspace-scoped server-side (SOUL §18). Grant management is
//! **admin-only** (`grant:read`/`write`); a non-admin principal gets a `403`
//! surfaced in the panel.
//!
//! A grant is keyed by **name** for create-or-replace (idempotent `POST`, keeping
//! the id) but by **id** for delete — so the editor disables the name when editing.
//!
//! ## Two authoring modes
//!
//! - **Builder** (default): capabilities are edited as **rows** — an action
//!   dropdown, a domain field (with a datalist of common domains), an optional
//!   `@`-selector, and a ⚙ disclosure for per-capability JSON constraints — each
//!   with a plain-English preview. Constraints are a **friendly form**: a dry-run
//!   toggle, spend/rate caps, and allowed-environment / needs-approval chips, plus
//!   an "other (JSON)" box for the rest (e.g. a time window).
//! - **Raw**: the original two textareas — capabilities as `domain:action[@sel] {json}`
//!   tokens and constraints as a raw JSON object — kept verbatim as a lossless
//!   escape hatch. Switching modes re-syncs from the other representation.
//!
//! Both round-trip through the same core `Capability`/`Constraints` shapes, so a
//! grant authored either way is identical on the wire.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::{Map, Value};

use super::widgets::{list_drawer_scrim, list_drawer_toggle};
use crate::api::{Action, Capability, CreateGrant, Grant, Resource};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// Which authoring surface the editor shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The visual builder (rows + friendly constraints form).
    Builder,
    /// The raw token / JSON textareas (escape hatch).
    Raw,
}

/// One editable capability row in the builder. Every field is its own signal so a
/// row edits independently (and the `<For>` over rows only re-keys on add/remove).
/// `RwSignal` is `Copy`, so the whole row is `Copy`.
#[derive(Clone, Copy)]
struct CapRow {
    /// Stable key for the `<For>`.
    id: usize,
    /// Action wire token (`*`/`read`/`write`/…).
    action: RwSignal<String>,
    /// Resource domain (`notes`, `exec`, `*`, …).
    domain: RwSignal<String>,
    /// Optional `@`-selector (`bao`, `local/out/*`).
    selector: RwSignal<String>,
    /// Optional per-capability constraints, as a raw JSON object string.
    constraints: RwSignal<String>,
    /// Whether the per-capability ⚙ constraints field is revealed.
    show_advanced: RwSignal<bool>,
}

/// The Grants panel component.
#[component]
pub fn GrantsPanel() -> impl IntoView {
    let grants = RwSignal::new(Vec::<Grant>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Editor identity. `selected_id` is the open grant's id (the delete key); None +
    // `is_new` = an unsaved draft.
    let selected_id = RwSignal::new(Option::<String>::None);
    let is_new = RwSignal::new(false);
    let edit_name = RwSignal::new(String::new());

    // Authoring mode.
    let mode = RwSignal::new(Mode::Builder);

    // Builder: capability rows + an id counter for stable keys.
    let cap_rows = RwSignal::new(Vec::<CapRow>::new());
    let next_id = RwSignal::new(0usize);

    // Builder: friendly constraints form.
    let c_dry_run = RwSignal::new(false);
    let c_cost = RwSignal::new(String::new());
    let c_rate = RwSignal::new(String::new());
    let c_env = RwSignal::new(Vec::<String>::new());
    let c_env_draft = RwSignal::new(String::new());
    let c_approval = RwSignal::new(Vec::<String>::new());
    let c_approval_draft = RwSignal::new(String::new());
    let c_other = RwSignal::new(String::new());

    // Raw: the original textareas.
    let edit_caps = RwSignal::new(String::new());
    let edit_constraints = RwSignal::new(String::new());

    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // Mint a fresh empty capability row.
    let new_row = move || {
        let id = next_id.get_untracked();
        next_id.set(id + 1);
        CapRow {
            id,
            action: RwSignal::new("read".to_string()),
            domain: RwSignal::new(String::new()),
            selector: RwSignal::new(String::new()),
            constraints: RwSignal::new(String::new()),
            show_advanced: RwSignal::new(false),
        }
    };

    // Seed every builder/raw field from a grant's capabilities + constraints. Both
    // representations are populated so a later mode switch is lossless from here.
    let seed_from = move |caps: &[Capability], constraints: &Value| {
        // Builder: rows.
        let mut rows = Vec::with_capacity(caps.len());
        let mut id = 0usize;
        for c in caps {
            rows.push(CapRow {
                id,
                action: RwSignal::new(action_token(c.action).to_string()),
                domain: RwSignal::new(c.resource.domain.clone()),
                selector: RwSignal::new(c.resource.selector.clone().unwrap_or_default()),
                constraints: RwSignal::new(if c.constraints.is_empty() {
                    String::new()
                } else {
                    serde_json::to_string(&Value::Object(c.constraints.clone())).unwrap_or_default()
                }),
                show_advanced: RwSignal::new(!c.constraints.is_empty()),
            });
            id += 1;
        }
        next_id.set(id);
        cap_rows.set(rows);
        // Builder: constraints form.
        let form = split_constraints(constraints);
        c_dry_run.set(form.dry_run);
        c_cost.set(form.cost);
        c_rate.set(form.rate);
        c_env.set(form.env);
        c_approval.set(form.approval);
        c_other.set(form.other);
        c_env_draft.set(String::new());
        c_approval_draft.set(String::new());
        // Raw.
        edit_caps.set(capabilities_to_text(caps));
        edit_constraints.set(pretty_constraints(constraints));
    };

    let load_into_editor = move |g: &Grant| {
        selected_id.set(Some(g.id.clone()));
        is_new.set(false);
        edit_name.set(g.name.clone());
        seed_from(&g.capabilities, &g.constraints);
        save_error.set(None);
    };

    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_grants(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_id.get_untracked().is_none()
                    {
                        if let Some(first) = list.first() {
                            load_into_editor(first);
                        }
                    }
                    grants.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    grants.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    refresh(true);

    let start_new = move || {
        selected_id.set(None);
        is_new.set(true);
        edit_name.set(String::new());
        mode.set(Mode::Builder);
        // Start with one empty row so the builder isn't blank.
        cap_rows.set(vec![new_row()]);
        c_dry_run.set(false);
        c_cost.set(String::new());
        c_rate.set(String::new());
        c_env.set(Vec::new());
        c_env_draft.set(String::new());
        c_approval.set(Vec::new());
        c_approval_draft.set(String::new());
        c_other.set(String::new());
        edit_caps.set(String::new());
        edit_constraints.set(String::new());
        save_error.set(None);
    };

    // Gather the builder capability rows into core `Capability`s (or the first
    // error). Blank rows (no domain) are skipped so a stray empty row isn't fatal.
    let builder_capabilities = move || -> Result<Vec<Capability>, String> {
        let mut out = Vec::new();
        for (i, row) in cap_rows.get_untracked().into_iter().enumerate() {
            if row.domain.get_untracked().trim().is_empty()
                && row.selector.get_untracked().trim().is_empty()
                && row.constraints.get_untracked().trim().is_empty()
            {
                continue; // an untouched blank row
            }
            let cap = row_to_capability(&row).map_err(|e| format!("capability {}: {e}", i + 1))?;
            out.push(cap);
        }
        Ok(out)
    };

    // Gather the builder constraints form into a JSON object (or an error).
    let builder_constraints = move || -> Result<Value, String> {
        build_constraints(
            c_dry_run.get_untracked(),
            &c_cost.get_untracked(),
            &c_rate.get_untracked(),
            &c_env.get_untracked(),
            &c_approval.get_untracked(),
            &c_other.get_untracked(),
        )
    };

    // Switch authoring mode, syncing the target representation from the source so no
    // edits are lost. Raw→Builder can fail to parse — then we surface the error and
    // stay in Raw.
    let switch_mode = move |target: Mode| {
        if mode.get_untracked() == target {
            return;
        }
        save_error.set(None);
        match target {
            Mode::Raw => {
                // Builder → Raw: serialize current rows + form into the textareas.
                let caps_text = cap_rows
                    .get_untracked()
                    .iter()
                    .map(row_to_token)
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                edit_caps.set(caps_text);
                if let Ok(v) = builder_constraints() {
                    edit_constraints.set(pretty_constraints(&v));
                }
                mode.set(Mode::Raw);
            }
            Mode::Builder => {
                // Raw → Builder: parse the textareas, then reseed the builder.
                let caps = match parse_capabilities(&edit_caps.get_untracked()) {
                    Ok(c) => c,
                    Err(e) => {
                        save_error.set(Some(format!("Capabilities: {e}")));
                        return;
                    }
                };
                let constraints = match parse_constraints(&edit_constraints.get_untracked()) {
                    Ok(c) => c,
                    Err(e) => {
                        save_error.set(Some(format!("Constraints: {e}")));
                        return;
                    }
                };
                seed_from(&caps, &constraints);
                mode.set(Mode::Builder);
            }
        }
    };

    // Save: build the grant from the active mode and POST (create-or-replace by name).
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        let name = edit_name.get_untracked().trim().to_string();
        if name.is_empty() {
            save_error.set(Some("Give the grant a name.".to_string()));
            return;
        }
        let (capabilities, constraints) = if mode.get_untracked() == Mode::Builder {
            let caps = match builder_capabilities() {
                Ok(c) => c,
                Err(e) => {
                    save_error.set(Some(e));
                    return;
                }
            };
            let cons = match builder_constraints() {
                Ok(c) => c,
                Err(e) => {
                    save_error.set(Some(e));
                    return;
                }
            };
            (caps, cons)
        } else {
            let caps = match parse_capabilities(&edit_caps.get_untracked()) {
                Ok(c) => c,
                Err(e) => {
                    save_error.set(Some(format!("Capabilities: {e}")));
                    return;
                }
            };
            let cons = match parse_constraints(&edit_constraints.get_untracked()) {
                Ok(c) => c,
                Err(e) => {
                    save_error.set(Some(format!("Constraints: {e}")));
                    return;
                }
            };
            (caps, cons)
        };

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::create_grant(
                token.as_deref(),
                &CreateGrant {
                    name,
                    capabilities,
                    constraints,
                },
            )
            .await;
            saving.set(false);
            match result {
                Ok(g) => {
                    load_into_editor(&g);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    let delete = move || {
        let Some(id) = selected_id.get_untracked() else {
            return;
        };
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        save_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_grant(token.as_deref(), &id).await {
                Ok(()) => {
                    saving.set(false);
                    selected_id.set(None);
                    is_new.set(false);
                    edit_name.set(String::new());
                    cap_rows.set(Vec::new());
                    edit_caps.set(String::new());
                    edit_constraints.set(String::new());
                    refresh(true);
                }
                Err(e) => {
                    saving.set(false);
                    save_error.set(Some(e.to_string()));
                }
            }
        });
    };

    let editor_open = move || selected_id.get().is_some() || is_new.get();
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    let add_cap = move || {
        cap_rows.update(|rows| rows.push(new_row()));
    };

    // Whether the list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Grants"</h2>
                    <button
                        class="pane-btn pane-btn-primary"
                        disabled=move || saving.get()
                        on:click=move |_| {
                            start_new();
                            list_open.set(false);
                        }
                    >
                        "New"
                    </button>
                </header>

                <div class="pane-list-body">
                    <Show when=move || loading.get() fallback=|| ().into_view()>
                        <div class="pane-list-status">"Loading…"</div>
                    </Show>

                    <Show
                        when=move || !loading.get() && load_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status pane-list-error">
                            {move || {
                                format!(
                                    "Could not load grants: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && grants.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No grants yet. Create one →"</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || grants.get()
                            key=|g| (g.id.clone(), g.capabilities.len())
                            children=move |g: Grant| {
                                let id = g.id.clone();
                                let is_active = {
                                    let id = id.clone();
                                    move || selected_id.get().as_deref() == Some(id.as_str())
                                };
                                let class = move || {
                                    if is_active() {
                                        "pane-item pane-item-active"
                                    } else {
                                        "pane-item"
                                    }
                                };
                                let label = g.name.clone();
                                let n = g.capabilities.len();
                                let g_for_click = g.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            on:click=move |_| {
                                                load_into_editor(&g_for_click);
                                                list_open.set(false);
                                            }
                                        >
                                            <span class="pane-item-title">{label}</span>
                                            <span class="pane-item-meta">
                                                {format!(
                                                    "{n} capabilit{}",
                                                    if n == 1 { "y" } else { "ies" },
                                                )}
                                            </span>
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            {list_drawer_toggle("Grants", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select a grant, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <form class="grant-form" on:submit=on_save_submit>
                        <input
                            class="grant-input grant-input-name"
                            placeholder="Grant name"
                            disabled=move || saving.get() || !is_new.get()
                            prop:value=move || edit_name.get()
                            on:input=move |ev| edit_name.set(event_target_value(&ev))
                        />

                        <div class="auto-mode" role="tablist">
                            <button
                                type="button"
                                class=move || {
                                    if mode.get() == Mode::Builder {
                                        "auto-mode-btn auto-mode-active"
                                    } else {
                                        "auto-mode-btn"
                                    }
                                }
                                disabled=move || saving.get()
                                on:click=move |_| switch_mode(Mode::Builder)
                            >
                                "Builder"
                            </button>
                            <button
                                type="button"
                                class=move || {
                                    if mode.get() == Mode::Raw {
                                        "auto-mode-btn auto-mode-active"
                                    } else {
                                        "auto-mode-btn"
                                    }
                                }
                                disabled=move || saving.get()
                                on:click=move |_| switch_mode(Mode::Raw)
                            >
                                "Raw"
                            </button>
                            <span class="auto-mode-hint">
                                {move || {
                                    if mode.get() == Mode::Builder {
                                        "Pick an action + resource per row."
                                    } else {
                                        "Edit tokens / JSON directly."
                                    }
                                }}
                            </span>
                        </div>

                        // --- Builder mode ---
                        <Show when=move || mode.get() == Mode::Builder fallback=|| ().into_view()>
                            <datalist id="cap-domains">
                                {COMMON_DOMAINS
                                    .iter()
                                    .map(|d| view! { <option value=*d></option> })
                                    .collect::<Vec<_>>()}
                            </datalist>

                            <label class="grant-field-label">"Capabilities"</label>
                            <div class="cap-rows">
                                <For
                                    each=move || cap_rows.get()
                                    key=|r| r.id
                                    children=move |row: CapRow| {
                                        let remove = move |_| {
                                            cap_rows.update(|rows| rows.retain(|r| r.id != row.id));
                                        };
                                        view! {
                                            <div class="cap-row-wrap">
                                                <div class="cap-row">
                                                    <select
                                                        class="grant-input cap-action"
                                                        disabled=move || saving.get()
                                                        prop:value=move || row.action.get()
                                                        on:change=move |ev| {
                                                            row.action.set(event_target_value(&ev))
                                                        }
                                                    >
                                                        {ACTION_OPTS
                                                            .iter()
                                                            .map(|(value, label)| {
                                                                view! { <option value=*value>{*label}</option> }
                                                            })
                                                            .collect::<Vec<_>>()}
                                                    </select>
                                                    <input
                                                        class="grant-input cap-domain"
                                                        list="cap-domains"
                                                        placeholder="resource (e.g. notes)"
                                                        disabled=move || saving.get()
                                                        prop:value=move || row.domain.get()
                                                        on:input=move |ev| {
                                                            row.domain.set(event_target_value(&ev))
                                                        }
                                                    />
                                                    <input
                                                        class="grant-input cap-sel"
                                                        placeholder="@ selector (optional)"
                                                        disabled=move || saving.get()
                                                        prop:value=move || row.selector.get()
                                                        on:input=move |ev| {
                                                            row.selector.set(event_target_value(&ev))
                                                        }
                                                    />
                                                    <button
                                                        type="button"
                                                        class="cap-icon"
                                                        title="Per-capability constraints (advanced)"
                                                        disabled=move || saving.get()
                                                        on:click=move |_| {
                                                            row.show_advanced.update(|s| *s = !*s)
                                                        }
                                                    >
                                                        <Icon icon=MdIcon::Settings />
                                                    </button>
                                                    <button
                                                        type="button"
                                                        class="cap-icon cap-icon-del"
                                                        title="Remove this capability"
                                                        disabled=move || saving.get()
                                                        on:click=remove
                                                    >
                                                        <Icon icon=MdIcon::Delete />
                                                    </button>
                                                </div>
                                                <div class="cap-preview">
                                                    {move || {
                                                        cap_preview(
                                                            &row.action.get(),
                                                            &row.domain.get(),
                                                            &row.selector.get(),
                                                        )
                                                    }}
                                                </div>
                                                <Show
                                                    when=move || row.show_advanced.get()
                                                    fallback=|| ().into_view()
                                                >
                                                    <input
                                                        class="grant-input cap-cons"
                                                        placeholder=r#"per-capability JSON, e.g. {"lang":"python"}"#
                                                        disabled=move || saving.get()
                                                        prop:value=move || row.constraints.get()
                                                        on:input=move |ev| {
                                                            row.constraints.set(event_target_value(&ev))
                                                        }
                                                    />
                                                </Show>
                                            </div>
                                        }
                                    }
                                />
                            </div>
                            <button
                                type="button"
                                class="cap-add"
                                disabled=move || saving.get()
                                on:click=move |_| add_cap()
                            >
                                "+ Add capability"
                            </button>

                            <label class="grant-field-label">"Constraints (optional)"</label>
                            <div class="cap-constraints">
                                <label class="cap-check">
                                    <input
                                        type="checkbox"
                                        disabled=move || saving.get()
                                        prop:checked=move || c_dry_run.get()
                                        on:change=move |ev| c_dry_run.set(event_target_checked(&ev))
                                    />
                                    <span>"Simulate only — never commit (dry run)"</span>
                                </label>
                                <div class="cap-cap-row">
                                    <label class="cap-num">
                                        <span class="cap-num-label">"Max spend (USD)"</span>
                                        <input
                                            class="grant-input"
                                            r#type="number"
                                            step="0.01"
                                            min="0"
                                            placeholder="none"
                                            disabled=move || saving.get()
                                            prop:value=move || c_cost.get()
                                            on:input=move |ev| c_cost.set(event_target_value(&ev))
                                        />
                                    </label>
                                    <label class="cap-num">
                                        <span class="cap-num-label">"Max actions / run"</span>
                                        <input
                                            class="grant-input"
                                            r#type="number"
                                            min="0"
                                            placeholder="none"
                                            disabled=move || saving.get()
                                            prop:value=move || c_rate.get()
                                            on:input=move |ev| c_rate.set(event_target_value(&ev))
                                        />
                                    </label>
                                </div>
                                <span class="grant-field-label">"Allowed environments"</span>
                                {chip_input(c_env, c_env_draft, "e.g. dev, then Enter", saving)}
                                <span class="grant-field-label">
                                    "Actions needing approval (domain:action)"
                                </span>
                                {chip_input(
                                    c_approval,
                                    c_approval_draft,
                                    "e.g. exec:run, then Enter",
                                    saving,
                                )}
                                <details class="cap-other">
                                    <summary>"Other constraints (time window, etc.) — JSON"</summary>
                                    <textarea
                                        class="grant-textarea grant-textarea-sm"
                                        placeholder=r#"{"time_window": {"start": "…", "end": "…"}}"#
                                        disabled=move || saving.get()
                                        prop:value=move || c_other.get()
                                        on:input=move |ev| c_other.set(event_target_value(&ev))
                                    ></textarea>
                                </details>
                            </div>
                        </Show>

                        // --- Raw mode ---
                        <Show when=move || mode.get() == Mode::Raw fallback=|| ().into_view()>
                            <label class="grant-field-label">
                                "Capabilities (one per line — e.g. notes:read, exec:run@bao, *)"
                            </label>
                            <textarea
                                class="grant-textarea"
                                placeholder="notes:read&#10;exec:run@bao&#10;storage:write@local/out/*"
                                disabled=move || saving.get()
                                prop:value=move || edit_caps.get()
                                on:input=move |ev| edit_caps.set(event_target_value(&ev))
                            ></textarea>

                            <label class="grant-field-label">"Constraints (JSON object, optional)"</label>
                            <textarea
                                class="grant-textarea grant-textarea-sm"
                                placeholder=r#"{"dry_run": true, "env_allow": ["dev"]}"#
                                disabled=move || saving.get()
                                prop:value=move || edit_constraints.get()
                                on:input=move |ev| edit_constraints.set(event_target_value(&ev))
                            ></textarea>
                        </Show>

                        <Show
                            when=move || save_error.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <div class="grant-form-error">
                                {move || save_error.get().unwrap_or_default()}
                            </div>
                        </Show>

                        <div class="grant-form-actions">
                            <button
                                class="pane-btn pane-btn-primary"
                                type="submit"
                                disabled=move || saving.get()
                            >
                                {move || {
                                    if saving.get() {
                                        "Saving…"
                                    } else if is_new.get() {
                                        "Create"
                                    } else {
                                        "Save"
                                    }
                                }}
                            </button>
                            <Show
                                when=move || selected_id.get().is_some()
                                fallback=|| ().into_view()
                            >
                                <button
                                    class="pane-btn pane-btn-danger"
                                    type="button"
                                    disabled=move || saving.get()
                                    on:click=move |_| delete()
                                >
                                    "Delete"
                                </button>
                            </Show>
                        </div>
                    </form>
                </Show>
            </div>
        </section>
    }
}

/// A free-text chip input bound to `selected` (removable tags + an inline add
/// input; Enter appends, deduped + trimmed). Used for the env-allow and
/// requires-approval constraint lists.
fn chip_input(
    selected: RwSignal<Vec<String>>,
    draft: RwSignal<String>,
    placeholder: &'static str,
    disabled: RwSignal<bool>,
) -> impl IntoView {
    let add = move || {
        let v = draft.get_untracked().trim().to_string();
        if v.is_empty() {
            return;
        }
        selected.update(|s| {
            if !s.iter().any(|x| x == &v) {
                s.push(v.clone());
            }
        });
        draft.set(String::new());
    };
    view! {
        <div class="pf-chips">
            <For
                each=move || selected.get()
                key=|c| c.clone()
                children=move |chip: String| {
                    let chip_rm = chip.clone();
                    let remove = move |_| {
                        selected.update(|s| s.retain(|x| x != &chip_rm));
                    };
                    view! {
                        <span class="pf-chip">
                            {chip.clone()}
                            <button
                                type="button"
                                class="pf-chip-x"
                                disabled=move || disabled.get()
                                on:click=remove
                            >
                                <Icon icon=MdIcon::Close />
                            </button>
                        </span>
                    }
                }
            />
            <input
                class="pf-chip-input"
                placeholder=placeholder
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:input=move |ev| draft.set(event_target_value(&ev))
                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                    if ev.key() == "Enter" {
                        ev.prevent_default();
                        add();
                    }
                }
            />
        </div>
    }
}

/// Common resource domains, offered as a `<datalist>` for the builder's domain
/// field. Mirrors the canonical capability domains — `catalerum-iam`'s role-base
/// set plus the protected `exec`/`mcp` grant scopes. NB the web-fetch egress
/// domain is `web` (SOUL §27/§19 `web:read@<host-glob>`), not `fetch`: a grant on
/// `fetch` would never match the enforced `web:read`.
const COMMON_DOMAINS: [&str; 18] = [
    "calendar",
    "storage",
    "notes",
    "tasks",
    "email",
    "memory",
    "skill",
    "graph",
    "vector",
    "automation",
    "conversation",
    "profile",
    "channel",
    "agent",
    "ui",
    "web",
    "exec",
    "mcp",
];

/// The action dropdown's `(wire token, label)` options. `*` is the `Any` wildcard.
const ACTION_OPTS: [(&str, &str); 9] = [
    ("read", "Read"),
    ("write", "Write"),
    ("delete", "Delete"),
    ("use", "Use"),
    ("run", "Run"),
    ("query", "Query"),
    ("search", "Search"),
    ("expose", "Expose"),
    ("*", "Any"),
];

/// A one-line plain-English summary of a capability row (for the live preview).
fn cap_preview(action: &str, domain: &str, selector: &str) -> String {
    let dom = domain.trim();
    let dom = if dom.is_empty() { "—" } else { dom };
    let verb = match action {
        "*" => "Any action on",
        "read" => "Read",
        "write" => "Write to",
        "delete" => "Delete from",
        "use" => "Use",
        "run" => "Run in",
        "query" => "Query",
        "search" => "Search",
        "expose" => "Expose",
        _ => "Act on",
    };
    let sel = selector.trim();
    if sel.is_empty() {
        format!("{verb} {dom}")
    } else {
        format!("{verb} {dom} ({sel})")
    }
}

/// Build a core [`Capability`] from a builder row (or an error message).
fn row_to_capability(row: &CapRow) -> Result<Capability, String> {
    let domain = row.domain.get_untracked().trim().to_string();
    if domain.is_empty() {
        return Err("resource (domain) is required".to_string());
    }
    let action = action_from_token(row.action.get_untracked().trim())
        .ok_or_else(|| "pick an action".to_string())?;
    let selector = {
        let s = row.selector.get_untracked().trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    let constraints = {
        let c = row.constraints.get_untracked();
        let c = c.trim();
        if c.is_empty() {
            Map::new()
        } else {
            serde_json::from_str::<Map<String, Value>>(c)
                .map_err(|e| format!("advanced JSON ({e})"))?
        }
    };
    Ok(Capability {
        action,
        resource: Resource { domain, selector },
        constraints,
    })
}

/// Render a builder row into its raw token form (for the Builder→Raw sync). Best
/// effort — a half-filled row produces a token the user can fix in Raw mode.
fn row_to_token(row: &CapRow) -> String {
    let domain = row.domain.get_untracked().trim().to_string();
    if domain.is_empty() {
        return String::new();
    }
    let mut s = format!("{}:{}", domain, row.action.get_untracked().trim());
    let sel = row.selector.get_untracked().trim().to_string();
    if !sel.is_empty() {
        s.push('@');
        s.push_str(&sel);
    }
    let cons = row.constraints.get_untracked().trim().to_string();
    if !cons.is_empty() {
        s.push(' ');
        s.push_str(&cons);
    }
    s
}

/// The known-constraint fields split out of a raw constraints object.
struct ConstraintsForm {
    dry_run: bool,
    cost: String,
    rate: String,
    env: Vec<String>,
    approval: Vec<String>,
    /// Everything that isn't a known field, re-serialized as JSON (e.g. `time_window`).
    other: String,
}

/// Split a constraints JSON object into the friendly-form fields + a JSON remainder
/// for any keys the form doesn't model (so the round-trip stays lossless).
fn split_constraints(value: &Value) -> ConstraintsForm {
    let mut obj = value.as_object().cloned().unwrap_or_default();
    let dry_run = obj.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let cost = obj
        .get("cost_limit")
        .and_then(Value::as_f64)
        .map(trim_float)
        .unwrap_or_default();
    let rate = obj
        .get("rate_limit")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .unwrap_or_default();
    let env = string_array(obj.get("env_allow"));
    let approval = string_array(obj.get("requires_approval"));
    for k in [
        "dry_run",
        "cost_limit",
        "rate_limit",
        "env_allow",
        "requires_approval",
    ] {
        obj.remove(k);
    }
    let other = if obj.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_default()
    };
    ConstraintsForm {
        dry_run,
        cost,
        rate,
        env,
        approval,
        other,
    }
}

/// Rebuild a constraints JSON object from the friendly-form fields, merging the
/// `other` JSON remainder. Mirrors the core `Constraints` `skip_serializing_if`
/// semantics: an unset / default field is **omitted**, so the object stays minimal.
fn build_constraints(
    dry_run: bool,
    cost: &str,
    rate: &str,
    env: &[String],
    approval: &[String],
    other: &str,
) -> Result<Value, String> {
    let mut obj: Map<String, Value> = if other.trim().is_empty() {
        Map::new()
    } else {
        serde_json::from_str::<Value>(other.trim())
            .map_err(|e| format!("Other constraints JSON ({e})"))?
            .as_object()
            .cloned()
            .ok_or_else(|| "Other constraints must be a JSON object".to_string())?
    };
    if dry_run {
        obj.insert("dry_run".to_string(), Value::Bool(true));
    }
    let cost = cost.trim();
    if !cost.is_empty() {
        let f: f64 = cost
            .parse()
            .map_err(|_| "Max spend must be a number".to_string())?;
        obj.insert("cost_limit".to_string(), serde_json::json!(f));
    }
    let rate = rate.trim();
    if !rate.is_empty() {
        let n: u32 = rate
            .parse()
            .map_err(|_| "Max actions must be a whole number".to_string())?;
        obj.insert("rate_limit".to_string(), serde_json::json!(n));
    }
    if !env.is_empty() {
        obj.insert("env_allow".to_string(), serde_json::json!(env));
    }
    if !approval.is_empty() {
        obj.insert("requires_approval".to_string(), serde_json::json!(approval));
    }
    Ok(Value::Object(obj))
}

/// Read a JSON value as a `Vec<String>` (non-strings dropped); `None`/non-array → empty.
fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Render an `f64` without a trailing `.0` (so `5.0` shows as `5`).
fn trim_float(f: f64) -> String {
    if f.fract() == 0.0 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
    }
}

/// The wire token for an [`Action`] (`read`/`write`/…; `*` for `Any`).
fn action_token(a: Action) -> &'static str {
    match a {
        Action::Any => "*",
        Action::Read => "read",
        Action::Write => "write",
        Action::Delete => "delete",
        Action::Use => "use",
        Action::Run => "run",
        Action::Query => "query",
        Action::Search => "search",
        Action::Expose => "expose",
    }
}

/// Parse an action token back into an [`Action`] (`*`/`any` → `Any`).
fn action_from_token(s: &str) -> Option<Action> {
    Some(match s {
        "*" | "any" => Action::Any,
        "read" => Action::Read,
        "write" => Action::Write,
        "delete" => Action::Delete,
        "use" => Action::Use,
        "run" => Action::Run,
        "query" => Action::Query,
        "search" => Action::Search,
        "expose" => Action::Expose,
        _ => return None,
    })
}

/// Parse one `domain:action[@selector] [{json}]` capability token (or the owner
/// wildcard `*`) into a [`Capability`].
fn parse_capability(token: &str) -> Result<Capability, String> {
    let t = token.trim();
    if t.is_empty() {
        return Err("empty capability".to_string());
    }
    if t == "*" {
        return Ok(Capability {
            action: Action::Any,
            resource: Resource {
                domain: "*".to_string(),
                selector: None,
            },
            constraints: Map::new(),
        });
    }
    // Split off an optional trailing per-capability constraints `{ … }`.
    let (head, constraints) = match t.find('{') {
        Some(i) => {
            let map: Map<String, Value> = serde_json::from_str(t[i..].trim())
                .map_err(|e| format!("bad per-capability constraints JSON ({e})"))?;
            (t[..i].trim(), map)
        }
        None => (t, Map::new()),
    };
    // Split off an optional `@selector`.
    let (core, selector) = match head.split_once('@') {
        Some((c, s)) => (c.trim(), Some(s.trim().to_string())),
        None => (head, None),
    };
    // Split `domain:action`.
    let (domain, action_str) = core
        .split_once(':')
        .ok_or_else(|| format!("expected `domain:action` in `{token}`"))?;
    let domain = domain.trim();
    if domain.is_empty() {
        return Err(format!("missing domain in `{token}`"));
    }
    let action = action_from_token(action_str.trim())
        .ok_or_else(|| format!("unknown action `{}` in `{token}`", action_str.trim()))?;
    Ok(Capability {
        action,
        resource: Resource {
            domain: domain.to_string(),
            selector: selector.filter(|s| !s.is_empty()),
        },
        constraints,
    })
}

/// Render a [`Capability`] back into its token form.
fn capability_to_token(c: &Capability) -> String {
    if c.action == Action::Any
        && c.resource.domain == "*"
        && c.resource.selector.is_none()
        && c.constraints.is_empty()
    {
        return "*".to_string();
    }
    let mut s = format!("{}:{}", c.resource.domain, action_token(c.action));
    if let Some(sel) = &c.resource.selector {
        s.push('@');
        s.push_str(sel);
    }
    if !c.constraints.is_empty() {
        if let Ok(j) = serde_json::to_string(&Value::Object(c.constraints.clone())) {
            s.push(' ');
            s.push_str(&j);
        }
    }
    s
}

/// Parse a capabilities textarea (one token per non-blank line) into a list.
fn parse_capabilities(input: &str) -> Result<Vec<Capability>, String> {
    let mut out = Vec::new();
    for (i, line) in input.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        let cap = parse_capability(l).map_err(|e| format!("line {}: {e}", i + 1))?;
        out.push(cap);
    }
    Ok(out)
}

/// Render a capability list back into the textarea form (one token per line).
fn capabilities_to_text(caps: &[Capability]) -> String {
    caps.iter()
        .map(capability_to_token)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parse the constraints textarea into a JSON object. Empty → `{}` (the server's
/// default Constraints). A non-object or malformed input is a client-side error.
fn parse_constraints(input: &str) -> Result<Value, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    if !value.is_object() {
        return Err("expected a JSON object".to_string());
    }
    Ok(value)
}

/// Pretty-print grant constraints into the editor. An empty / `null` object →
/// a blank textarea.
fn pretty_constraints(value: &Value) -> String {
    let empty = value.is_null()
        || value
            .as_object()
            .map(serde_json::Map::is_empty)
            .unwrap_or(false);
    if empty {
        String::new()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_capability_common_tokens() {
        let c = parse_capability("notes:read").unwrap();
        assert_eq!(c.action, Action::Read);
        assert_eq!(c.resource.domain, "notes");
        assert!(c.resource.selector.is_none());
        assert!(c.constraints.is_empty());

        let c = parse_capability("exec:run@bao").unwrap();
        assert_eq!(c.action, Action::Run);
        assert_eq!(c.resource.selector.as_deref(), Some("bao"));

        let c = parse_capability("storage:write@local/out/*").unwrap();
        assert_eq!(c.resource.selector.as_deref(), Some("local/out/*"));

        let star = parse_capability("*").unwrap();
        assert_eq!(star.action, Action::Any);
        assert_eq!(star.resource.domain, "*");
    }

    #[test]
    fn parse_capability_with_per_cap_constraints() {
        let c = parse_capability(r#"exec:run@bao {"lang":"python"}"#).unwrap();
        assert_eq!(c.action, Action::Run);
        assert_eq!(c.resource.selector.as_deref(), Some("bao"));
        assert_eq!(c.constraints["lang"], "python");
    }

    #[test]
    fn parse_capability_rejects_bad_input() {
        assert!(parse_capability("nocolon").is_err());
        assert!(parse_capability("notes:telepathy").is_err());
        assert!(parse_capability(":read").is_err());
        assert!(parse_capability(r#"exec:run {bad json}"#).is_err());
    }

    #[test]
    fn capability_token_round_trips() {
        for token in [
            "notes:read",
            "exec:run@bao",
            "storage:write@local/out/*",
            "*",
            r#"exec:run@bao {"lang":"python"}"#,
        ] {
            let cap = parse_capability(token).unwrap();
            assert_eq!(capability_to_token(&cap), token, "round-trip of {token}");
        }
    }

    #[test]
    fn parse_and_render_capabilities_list() {
        let text = "notes:read\n\nexec:run@bao\n";
        let caps = parse_capabilities(text).unwrap();
        assert_eq!(caps.len(), 2, "blank lines skipped");
        assert_eq!(capabilities_to_text(&caps), "notes:read\nexec:run@bao");
        // A bad line reports its line number.
        assert!(parse_capabilities("notes:read\nbad")
            .unwrap_err()
            .contains("line 2"));
    }

    #[test]
    fn constraints_parse_and_pretty() {
        assert_eq!(parse_constraints("  ").unwrap(), json!({}));
        assert_eq!(
            parse_constraints(r#"{"dry_run":true}"#).unwrap(),
            json!({"dry_run": true})
        );
        assert!(parse_constraints("[1,2]").is_err());
        assert!(parse_constraints("{bad").is_err());
        // Empty object → blank editor; non-empty → pretty.
        assert!(pretty_constraints(&json!({})).is_empty());
        assert!(pretty_constraints(&Value::Null).is_empty());
        assert!(pretty_constraints(&json!({"dry_run":true})).contains("dry_run"));
    }

    #[test]
    fn constraints_form_round_trips_known_and_other() {
        // A mix of known fields + an unmodeled key (time_window) in `other`.
        let v = json!({
            "dry_run": true,
            "cost_limit": 5.0,
            "rate_limit": 10,
            "env_allow": ["dev"],
            "requires_approval": ["exec:run"],
            "time_window": {"start": "a", "end": "b"}
        });
        let form = split_constraints(&v);
        assert!(form.dry_run);
        assert_eq!(form.cost, "5");
        assert_eq!(form.rate, "10");
        assert_eq!(form.env, vec!["dev".to_string()]);
        assert_eq!(form.approval, vec!["exec:run".to_string()]);
        assert!(form.other.contains("time_window"));

        let rebuilt = build_constraints(
            form.dry_run,
            &form.cost,
            &form.rate,
            &form.env,
            &form.approval,
            &form.other,
        )
        .unwrap();
        assert_eq!(rebuilt, v);
    }

    #[test]
    fn build_constraints_omits_unset_fields() {
        // All-default form → an empty object (matches the server's default Constraints).
        let v = build_constraints(false, "", "", &[], &[], "").unwrap();
        assert_eq!(v, json!({}));
        // Bad numbers are surfaced.
        assert!(build_constraints(false, "abc", "", &[], &[], "").is_err());
        assert!(build_constraints(false, "", "1.5", &[], &[], "").is_err());
    }

    #[test]
    fn cap_preview_reads_friendly() {
        assert_eq!(cap_preview("run", "exec", "bao"), "Run in exec (bao)");
        assert_eq!(cap_preview("read", "notes", ""), "Read notes");
        assert_eq!(cap_preview("*", "*", ""), "Any action on *");
    }
}
