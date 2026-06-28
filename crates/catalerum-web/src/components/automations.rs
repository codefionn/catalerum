//! The Automations panel (SOUL §11, §12 — automations builder).
//!
//! A two-pane workbench panel: a left list of the workspace's automations and a
//! right editor (name, enabled, triggers, condition, actions) with create / save
//! / delete, a pause/resume toggle, and a **recent-runs** history for the open
//! automation. It is a thin client of the automations REST surface
//! (`/automations`, `/automations/{name}` + `/enabled` + `/runs`) — every call
//! carries the dev session token and is workspace-scoped server-side (SOUL §18).
//!
//! In **Raw** mode the whole automation is authored as one **raw-JSON** document —
//! the same shape the create/update REST body uses (`triggers` / `condition` /
//! `actions` / `spec`), plus the read-only `id` / `workspace_id` the server assigns
//! (so the view mirrors exactly what is stored). The editor parses it client-side
//! for well-formedness before sending; on save only the body is read back (`name` +
//! `enabled` come from the dedicated fields, ids are server-managed), and the
//! server's typed `400` (unknown kind / missing field / empty lists) surfaces in the
//! form error. A name keys the automation (the path key for update/delete), so it is
//! fixed once created. Because the Raw document carries `spec` verbatim, a graph
//! automation's `spec.graph` round-trips through Raw untouched (editing it here — or
//! saving from Raw when the canvas can't open a newer graph — no longer drops it).
//!
//! A **typed trigger builder** sits above the JSON editor as a
//! progressive-enhancement assist: pick a trigger kind, fill its typed fields, and
//! "Add trigger" appends a well-formed trigger object into the document's `triggers`
//! array (still hand-editable). The build/append logic ([`build_trigger`] /
//! [`append_trigger`]) is pure, so it is unit-tested; opaque predicates the builder
//! doesn't model (a `calendar_event` lead/filter, a channel `filter` object) remain
//! available via the raw editor. (A typed *action* builder is the natural next
//! refinement.)
//!
//! A **Visual ⇄ Raw mode toggle** sits above the editor body (SOUL §11 Phase C).
//! In **Visual** mode the panel owns a [`FlowGraph`] and embeds the [`FlowEditor`]
//! drag-and-drop canvas; save runs [`validate_flow`] client-side, serializes the
//! graph via [`graph_to_spec_value`] into `spec.graph`, and sends empty
//! `triggers`/`actions` (the backend compiles the dispatch triggers from the graph
//! and skips the linear validation). On load, an existing automation's `spec.graph`
//! is round-tripped back into the canvas via [`flow_from_spec`]. In **Raw** mode the
//! legacy JSON editor is unchanged (`spec: None`). [`default_mode_for`] (pure +
//! tested) picks the opening mode: a stored graph or an empty/new automation opens
//! Visual; a legacy linear automation opens Raw.

use std::collections::HashMap;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::api::{Automation, AutomationRun, CreateAutomation, RunDetail, UpdateAutomation};
use crate::auth;
use crate::components::flow::{
    flow_from_spec, graph_to_spec_value, validate_flow, FlowEditor, FlowGraph,
};
use crate::components::widgets::{list_drawer_scrim, list_drawer_toggle};
use crate::rest;

/// Which editor surface is active for the open automation: the visual node-graph
/// canvas, or the single raw-JSON document (+ typed trigger builder). Both stay
/// reachable via the mode toggle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorMode {
    /// The drag-and-drop [`FlowEditor`] canvas (persists to `spec.graph`).
    Visual,
    /// The raw-JSON editor: the whole automation as one JSON document
    /// (`triggers`/`condition`/`actions`/`spec` + read-only ids).
    Raw,
}

/// Pick the editor mode an automation should open in. A brand-new automation (no
/// graph, no legacy triggers) starts on the **Visual** canvas; an existing
/// automation that already carries a `spec.graph` also opens Visual (round-tripped
/// into the canvas); anything with legacy triggers (and no graph) opens on the
/// **Raw** editor so its hand-authored JSON stays front-and-centre. Pure + testable.
fn default_mode_for(spec: Option<&Value>, has_legacy_triggers: bool) -> EditorMode {
    if spec.and_then(flow_from_spec).is_some() {
        // A stored graph → edit it visually.
        EditorMode::Visual
    } else if has_legacy_triggers {
        // Legacy linear automation → keep the raw editor in front.
        EditorMode::Raw
    } else {
        // A fresh / empty automation → start on the canvas.
        EditorMode::Visual
    }
}

/// The Automations panel component.
#[component]
pub fn AutomationsPanel() -> impl IntoView {
    // Loaded list + load state.
    let automations = RwSignal::new(Vec::<Automation>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Editor state.
    let selected_name = RwSignal::new(Option::<String>::None);
    let is_new = RwSignal::new(false);
    let edit_name = RwSignal::new(String::new());
    let edit_enabled = RwSignal::new(true);
    // The Raw editor's single full-automation JSON document (the create/update body
    // shape + read-only ids). The source of truth for a Raw-mode save.
    let edit_json = RwSignal::new(String::new());
    // Typed trigger-builder state — an assist that appends a well-formed trigger into
    // the raw JSON document's `triggers` array (which stays hand-editable).
    let tb_kind = RwSignal::new(TRIGGER_KINDS[0].0.to_string());
    let tb_values = RwSignal::new(HashMap::<String, String>::new());
    let tb_error = RwSignal::new(Option::<String>::None);
    // Visual ⇄ Raw editor mode + the canvas graph owned by this panel (so the
    // FlowEditor's interaction state is the only thing the component itself holds).
    let mode = RwSignal::new(EditorMode::Visual);
    let flow_graph = RwSignal::new(FlowGraph::default());
    // `true` when the stored spec HAS a graph the canvas can't parse (a node kind
    // newer than this build). The Visual tab is then disabled: an empty canvas
    // over a real graph invites a save that silently overwrites it.
    let visual_blocked = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let toggling = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // Manual-run affordances (SOUL §11/§29): "collect now" for collect automations,
    // "fire" for named-signal trigger automations. `selected_auto` carries the loaded
    // automation so the affordances key off its stored trigger kind. Each notice is
    // `(is_error, message)`.
    let selected_auto = RwSignal::new(Option::<Automation>::None);
    let collecting = RwSignal::new(false);
    let collect_notice = RwSignal::new(Option::<(bool, String)>::None);
    let firing = RwSignal::new(false);
    let fire_payload = RwSignal::new(String::new());
    let fire_notice = RwSignal::new(Option::<(bool, String)>::None);

    // Run history for the open automation.
    let runs = RwSignal::new(Vec::<AutomationRun>::new());
    let runs_loading = RwSignal::new(false);
    let runs_error = RwSignal::new(Option::<String>::None);

    // The expanded run + its step detail.
    let selected_run = RwSignal::new(Option::<String>::None);
    let run_detail = RwSignal::new(Option::<RunDetail>::None);
    let run_detail_loading = RwSignal::new(false);
    let run_detail_error = RwSignal::new(Option::<String>::None);

    // Fetch the recent runs of `name` into the history section.
    let load_runs = move |name: String| {
        runs_loading.set(true);
        runs_error.set(None);
        runs.set(Vec::new());
        // Collapse any open run-detail when the run list reloads.
        selected_run.set(None);
        run_detail.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_automation_runs(token.as_deref(), &name).await {
                Ok(list) => {
                    runs.set(list);
                    runs_error.set(None);
                }
                Err(e) => runs_error.set(Some(e.to_string())),
            }
            runs_loading.set(false);
        });
    };

    // Expand a run to show its steps (or collapse it if it's already open).
    let open_run = move |run_id: String| {
        if selected_run.get_untracked().as_deref() == Some(run_id.as_str()) {
            selected_run.set(None);
            run_detail.set(None);
            return;
        }
        let Some(name) = selected_name.get_untracked() else {
            return;
        };
        selected_run.set(Some(run_id.clone()));
        run_detail.set(None);
        run_detail_loading.set(true);
        run_detail_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::get_automation_run(token.as_deref(), &name, &run_id).await {
                Ok(d) => {
                    run_detail.set(Some(d));
                    run_detail_error.set(None);
                }
                Err(e) => run_detail_error.set(Some(e.to_string())),
            }
            run_detail_loading.set(false);
        });
    };

    // Load an automation's fields into the editor + fetch its runs.
    let load_into_editor = move |a: &Automation| {
        selected_name.set(Some(a.name.clone()));
        is_new.set(false);
        edit_name.set(a.name.clone());
        edit_enabled.set(a.enabled);
        // The Raw editor shows the whole stored automation as one JSON document
        // (ids included) — the same shape create/update accept.
        edit_json.set(automation_json_pretty(a));
        // Populate the canvas from the stored graph (None → empty), and pick the
        // mode the automation should open in. A graph that EXISTS but doesn't parse
        // (authored by a newer build) locks the Visual tab — never an empty canvas
        // whose save would overwrite the real graph.
        let parsed = a.spec.as_ref().and_then(flow_from_spec);
        let has_graph = a
            .spec
            .as_ref()
            .and_then(|s| s.as_object())
            .is_some_and(|s| s.contains_key("graph"));
        visual_blocked.set(has_graph && parsed.is_none());
        flow_graph.set(parsed.unwrap_or_default());
        mode.set(default_mode_for(a.spec.as_ref(), !a.triggers.is_empty()));
        save_error.set(None);
        // Retain the loaded automation (for manual-run detection) + reset its notices.
        selected_auto.set(Some(a.clone()));
        collect_notice.set(None);
        fire_notice.set(None);
        fire_payload.set(String::new());
        load_runs(a.name.clone());
    };

    // Fetch the automation list. Auto-open the first on first paint.
    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_automations(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_name.get_untracked().is_none()
                    {
                        if let Some(first) = list.first() {
                            load_into_editor(first);
                        }
                    }
                    automations.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    automations.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    refresh(true);

    // Begin a new, unsaved automation.
    let start_new = move || {
        selected_name.set(None);
        is_new.set(true);
        edit_name.set(String::new());
        edit_enabled.set(true);
        edit_json.set(new_automation_template());
        // A fresh automation starts on the (empty) visual canvas.
        flow_graph.set(FlowGraph::default());
        visual_blocked.set(false);
        mode.set(default_mode_for(None, false));
        save_error.set(None);
        // A new (unsaved) automation has no manual-run affordances yet.
        selected_auto.set(None);
        collect_notice.set(None);
        fire_notice.set(None);
        fire_payload.set(String::new());
        runs.set(Vec::new());
        runs_error.set(None);
        selected_run.set(None);
        run_detail.set(None);
    };

    // Build a trigger from the typed builder and append it to the triggers JSON
    // (which the user can still hand-edit). Reports a clear error on a missing
    // required field or a malformed existing triggers array.
    let add_trigger = move || {
        tb_error.set(None);
        let kind = tb_kind.get_untracked();
        let values = tb_values.get_untracked();
        match build_trigger(&kind, &values) {
            Ok(trigger) => match append_trigger(&edit_json.get_untracked(), trigger) {
                Ok(text) => {
                    edit_json.set(text);
                    tb_values.set(HashMap::new());
                }
                Err(e) => tb_error.set(Some(format!("Automation JSON must be an object: {e}"))),
            },
            Err(e) => tb_error.set(Some(e)),
        }
    };

    // Save the editor: create a new automation or replace the open one. Triggers
    // / condition / actions are parsed from JSON first (client-side
    // well-formedness); the server then validates the typed spec.
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        let new = is_new.get_untracked();
        let name = if new {
            edit_name.get_untracked().trim().to_string()
        } else {
            selected_name.get_untracked().unwrap_or_default()
        };
        if name.is_empty() {
            save_error.set(Some("Give the automation a name.".to_string()));
            return;
        }
        let enabled = edit_enabled.get_untracked();

        // Assemble the request payload per the active editor mode.
        // - Visual: validate the canvas graph, persist it under `spec.graph`, and
        //   leave the linear `triggers`/`actions` empty — the backend compiles the
        //   dispatch triggers from the graph and skips the linear validation.
        // - Raw: parse the single full-automation JSON document. Its body
        //   (triggers/condition/actions/spec) is authoritative — including a
        //   `spec.graph`, so a graph edited/kept in Raw round-trips instead of being
        //   dropped; `name`/`enabled`/ids in the document are ignored (name + enabled
        //   come from the fields above, ids are server-managed).
        let (triggers, condition, actions, spec) = if mode.get_untracked() == EditorMode::Visual {
            let graph = flow_graph.get_untracked();
            if let Err(e) = validate_flow(&graph) {
                save_error.set(Some(format!("Graph: {e}")));
                return;
            }
            (
                Vec::<Value>::new(),
                None,
                Vec::<Value>::new(),
                Some(graph_to_spec_value(&graph)),
            )
        } else {
            match parse_raw_body(&edit_json.get_untracked()) {
                Ok(parts) => parts,
                Err(e) => {
                    save_error.set(Some(format!("Automation JSON: {e}")));
                    return;
                }
            }
        };

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            let result: Result<Automation, rest::RestError> = if new {
                rest::create_automation(
                    tok,
                    &CreateAutomation {
                        name,
                        enabled,
                        triggers,
                        condition,
                        actions,
                        spec,
                    },
                )
                .await
            } else {
                rest::update_automation(
                    tok,
                    &name,
                    &UpdateAutomation {
                        enabled,
                        triggers,
                        condition,
                        actions,
                        spec,
                    },
                )
                .await
            };
            saving.set(false);
            match result {
                Ok(a) => {
                    load_into_editor(&a);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    // Pause / resume the open automation via the dedicated endpoint (no spec
    // re-validation), then reflect the new state.
    let toggle_enabled = move || {
        let Some(name) = selected_name.get_untracked() else {
            return;
        };
        if toggling.get_untracked() || saving.get_untracked() {
            return;
        }
        let next = !edit_enabled.get_untracked();
        toggling.set(true);
        save_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_automation_enabled(token.as_deref(), &name, next).await {
                Ok(a) => {
                    edit_enabled.set(a.enabled);
                    toggling.set(false);
                    refresh(false);
                }
                Err(e) => {
                    toggling.set(false);
                    save_error.set(Some(e.to_string()));
                }
            }
        });
    };

    // "Collect now": enqueue one immediate poll of a Collect-headed automation
    // (SOUL §29), bypassing its `every` cadence. Surfaces the 202 as a notice or the
    // server's 400/404 text.
    let do_collect = move || {
        let Some(name) = selected_name.get_untracked() else {
            return;
        };
        if collecting.get_untracked() {
            return;
        }
        collecting.set(true);
        collect_notice.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::collect_now(token.as_deref(), &name).await {
                Ok(_) => collect_notice.set(Some((false, "Collect started.".to_string()))),
                Err(e) => collect_notice.set(Some((true, e.to_string()))),
            }
            collecting.set(false);
        });
    };

    // "Fire": fire this automation's named signal on demand (SOUL §11) with an optional
    // JSON payload (rejected client-side before send if malformed). Surfaces the 202
    // match count or the server error.
    let do_fire = move || {
        let Some(name) = selected_name.get_untracked() else {
            return;
        };
        if firing.get_untracked() {
            return;
        }
        fire_notice.set(None);
        let payload = match parse_fire_payload(&fire_payload.get_untracked()) {
            Ok(p) => p,
            Err(e) => {
                fire_notice.set(Some((true, format!("Payload: {e}"))));
                return;
            }
        };
        firing.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::fire_trigger(token.as_deref(), &name, payload.as_ref()).await {
                Ok(r) => {
                    let plural = if r.matched == 1 { "" } else { "s" };
                    fire_notice.set(Some((
                        false,
                        format!("Fired — {} automation{plural} matched.", r.matched),
                    )));
                }
                Err(e) => fire_notice.set(Some((true, e.to_string()))),
            }
            firing.set(false);
        });
    };

    // Delete the open automation.
    let delete = move || {
        let Some(name) = selected_name.get_untracked() else {
            return;
        };
        if saving.get_untracked() {
            return;
        }
        saving.set(true);
        save_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_automation(token.as_deref(), &name).await {
                Ok(()) => {
                    saving.set(false);
                    selected_name.set(None);
                    selected_auto.set(None);
                    collect_notice.set(None);
                    fire_notice.set(None);
                    is_new.set(false);
                    edit_name.set(String::new());
                    edit_json.set(String::new());
                    runs.set(Vec::new());
                    refresh(true);
                }
                Err(e) => {
                    saving.set(false);
                    save_error.set(Some(e.to_string()));
                }
            }
        });
    };

    let editor_open = move || selected_name.get().is_some() || is_new.get();
    let show_runs = move || selected_name.get().is_some() && !is_new.get();
    // Manual-run affordances key off the *saved* automation's trigger kind (detected
    // from its stored spec), so they only appear once it exists on the server.
    let is_collect = move || selected_auto.with(|o| o.as_ref().is_some_and(is_collect_automation));
    let is_trigger = move || selected_auto.with(|o| o.as_ref().is_some_and(is_trigger_automation));
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // Whether the automations list is open as a mobile drawer (SOUL §12) — the
    // same collapsible "second sidebar" as the chat sessions list. Inert on
    // desktop, where the list is a static column; the editor is the
    // always-visible detail pane.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Automations"</h2>
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
                                    "Could not load automations: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && automations.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No automations yet. Create one →"</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || automations.get()
                            key=|a| (a.name.clone(), a.enabled, a.triggers.len())
                            children=move |a: Automation| {
                                let name = a.name.clone();
                                let is_active = {
                                    let name = name.clone();
                                    move || {
                                        selected_name.get().as_deref() == Some(name.as_str())
                                    }
                                };
                                let class = move || {
                                    if is_active() {
                                        "pane-item pane-item-active"
                                    } else {
                                        "pane-item"
                                    }
                                };
                                let label = a.name.clone();
                                let enabled = a.enabled;
                                let trig = a.triggers.len();
                                let act = a.actions.len();
                                let a_for_click = a.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            on:click=move |_| {
                                                load_into_editor(&a_for_click);
                                                list_open.set(false);
                                            }
                                        >
                                            <span class="pane-item-title">
                                                {label}
                                                <span class=move || {
                                                    if enabled {
                                                        "auto-pill auto-pill-on"
                                                    } else {
                                                        "auto-pill auto-pill-off"
                                                    }
                                                }>
                                                    {if enabled { "on" } else { "off" }}
                                                </span>
                                            </span>
                                            <span class="pane-item-meta">
                                                {format!(
                                                    "{trig} trigger{} · {act} action{}",
                                                    if trig == 1 { "" } else { "s" },
                                                    if act == 1 { "" } else { "s" },
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

            {list_drawer_toggle("Automations", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select an automation, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <form class="auto-form" on:submit=on_save_submit>
                        <div class="auto-form-row">
                            <input
                                class="auto-input auto-input-name"
                                placeholder="Automation name"
                                disabled=move || saving.get() || !is_new.get()
                                prop:value=move || edit_name.get()
                                on:input=move |ev| edit_name.set(event_target_value(&ev))
                            />
                            <label class="auto-check">
                                <input
                                    type="checkbox"
                                    disabled=move || saving.get()
                                    prop:checked=move || edit_enabled.get()
                                    on:change=move |ev| {
                                        edit_enabled.set(event_target_checked(&ev))
                                    }
                                />
                                "Enabled"
                            </label>
                            <Show
                                when=move || selected_name.get().is_some()
                                fallback=|| ().into_view()
                            >
                                <button
                                    class="pane-btn"
                                    type="button"
                                    disabled=move || toggling.get() || saving.get()
                                    on:click=move |_| toggle_enabled()
                                >
                                    {move || {
                                        if toggling.get() {
                                            "…"
                                        } else if edit_enabled.get() {
                                            "Pause"
                                        } else {
                                            "Resume"
                                        }
                                    }}
                                </button>
                            </Show>
                            <Show when=is_collect fallback=|| ().into_view()>
                                <button
                                    class="pane-btn"
                                    type="button"
                                    title="Enqueue one immediate poll of this collect automation now"
                                    disabled=move || collecting.get() || saving.get()
                                    on:click=move |_| do_collect()
                                >
                                    {move || {
                                        if collecting.get() { "Collecting…" } else { "Collect now" }
                                    }}
                                </button>
                            </Show>
                        </div>

                        <Show
                            when=move || is_collect() && collect_notice.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            {move || {
                                collect_notice
                                    .get()
                                    .map(|(is_err, msg)| {
                                        let cls = if is_err {
                                            "auto-notice auto-notice-err"
                                        } else {
                                            "auto-notice auto-notice-ok"
                                        };
                                        view! { <div class=cls>{msg}</div> }
                                    })
                            }}
                        </Show>

                        <Show when=is_trigger fallback=|| ().into_view()>
                            <div class="auto-fire">
                                <div class="auto-fire-head">
                                    <span class="auto-fire-title">"Fire signal"</span>
                                    <button
                                        class="pane-btn pane-btn-primary"
                                        type="button"
                                        title="Fire this automation's named signal now"
                                        disabled=move || firing.get() || saving.get()
                                        on:click=move |_| do_fire()
                                    >
                                        {move || if firing.get() { "Firing…" } else { "Fire" }}
                                    </button>
                                </div>
                                <label class="auto-field-label">"Payload (JSON, optional)"</label>
                                <textarea
                                    class="auto-textarea auto-textarea-sm"
                                    placeholder=r#"{"key":"value"} — carried on the run's trigger event"#
                                    disabled=move || firing.get()
                                    prop:value=move || fire_payload.get()
                                    on:input=move |ev| fire_payload.set(event_target_value(&ev))
                                ></textarea>
                                {move || {
                                    fire_notice
                                        .get()
                                        .map(|(is_err, msg)| {
                                            let cls = if is_err {
                                                "auto-notice auto-notice-err"
                                            } else {
                                                "auto-notice auto-notice-ok"
                                            };
                                            view! { <div class=cls>{msg}</div> }
                                        })
                                }}
                            </div>
                        </Show>

                        <div class="auto-mode" role="tablist">
                            <button
                                class=move || {
                                    if mode.get() == EditorMode::Visual {
                                        "auto-mode-btn auto-mode-active"
                                    } else {
                                        "auto-mode-btn"
                                    }
                                }
                                type="button"
                                disabled=move || saving.get() || visual_blocked.get()
                                title=move || {
                                    if visual_blocked.get() {
                                        "This automation's graph uses node types this canvas \
                                         doesn't know — edit it in Raw JSON."
                                    } else {
                                        ""
                                    }
                                }
                                on:click=move |_| mode.set(EditorMode::Visual)
                            >
                                "Visual"
                            </button>
                            <button
                                class=move || {
                                    if mode.get() == EditorMode::Raw {
                                        "auto-mode-btn auto-mode-active"
                                    } else {
                                        "auto-mode-btn"
                                    }
                                }
                                type="button"
                                disabled=move || saving.get()
                                on:click=move |_| mode.set(EditorMode::Raw)
                            >
                                "Raw JSON"
                            </button>
                            <span class="auto-mode-hint">
                                {move || {
                                    if mode.get() == EditorMode::Visual {
                                        "Author a node graph; saved as spec.graph."
                                    } else {
                                        "Hand-edit triggers / condition / actions."
                                    }
                                }}
                            </span>
                        </div>

                        <Show when=move || mode.get() == EditorMode::Visual fallback=|| ().into_view()>
                            <div class="auto-flow-wrap">
                                <FlowEditor graph=flow_graph />
                            </div>
                        </Show>

                        <Show when=move || mode.get() == EditorMode::Raw fallback=|| ().into_view()>
                        <div class="auto-tb">
                            <div class="auto-tb-head">
                                <span class="auto-tb-title">"Add a trigger"</span>
                                <select
                                    class="auto-input auto-tb-kind"
                                    disabled=move || saving.get()
                                    on:change=move |ev| {
                                        tb_kind.set(event_target_value(&ev));
                                        tb_values.set(HashMap::new());
                                        tb_error.set(None);
                                    }
                                >
                                    {TRIGGER_KINDS
                                        .iter()
                                        .map(|(k, label)| {
                                            view! { <option value=*k>{*label}</option> }
                                        })
                                        .collect::<Vec<_>>()}
                                </select>
                                <button
                                    class="pane-btn"
                                    type="button"
                                    disabled=move || saving.get()
                                    on:click=move |_| add_trigger()
                                >
                                    "Add trigger"
                                </button>
                            </div>
                            <div class="auto-tb-fields">
                                {move || {
                                    trigger_fields(&tb_kind.get())
                                        .iter()
                                        .map(|(key, label, _required, multiline)| {
                                            let key = *key;
                                            let label = *label;
                                            let input = if *multiline {
                                                view! {
                                                    <textarea
                                                        class="auto-textarea auto-textarea-sm"
                                                        prop:value=move || {
                                                            tb_values
                                                                .with(|m| m.get(key).cloned().unwrap_or_default())
                                                        }
                                                        on:input=move |ev| {
                                                            let v = event_target_value(&ev);
                                                            tb_values.update(|m| {
                                                                m.insert(key.to_string(), v);
                                                            });
                                                        }
                                                    ></textarea>
                                                }
                                                .into_any()
                                            } else {
                                                view! {
                                                    <input
                                                        class="auto-input"
                                                        prop:value=move || {
                                                            tb_values
                                                                .with(|m| m.get(key).cloned().unwrap_or_default())
                                                        }
                                                        on:input=move |ev| {
                                                            let v = event_target_value(&ev);
                                                            tb_values.update(|m| {
                                                                m.insert(key.to_string(), v);
                                                            });
                                                        }
                                                    />
                                                }
                                                .into_any()
                                            };
                                            view! {
                                                <label class="auto-tb-field">
                                                    <span class="auto-tb-flabel">{label}</span>
                                                    {input}
                                                </label>
                                            }
                                        })
                                        .collect::<Vec<_>>()
                                }}
                            </div>
                            <Show
                                when=move || tb_error.with(Option::is_some)
                                fallback=|| ().into_view()
                            >
                                <div class="auto-form-error">
                                    {move || tb_error.get().unwrap_or_default()}
                                </div>
                            </Show>
                        </div>

                        <label class="auto-field-label">"Automation (JSON)"</label>
                        <textarea
                            class="auto-textarea auto-textarea-json"
                            placeholder=r#"{
  "triggers": [{"kind": "schedule", "cron": "0 9 * * *"}],
  "condition": null,
  "actions": [{"kind": "summarize"}]
}"#
                            disabled=move || saving.get()
                            prop:value=move || edit_json.get()
                            on:input=move |ev| edit_json.set(event_target_value(&ev))
                        ></textarea>
                        <p class="auto-mode-hint">
                            "The full automation, as stored (id / workspace_id are read-only; \
                             name and enabled use the fields above). Saving sends its \
                             triggers / condition / actions / spec."
                        </p>
                        </Show>

                        <Show
                            when=move || save_error.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <div class="auto-form-error">
                                {move || save_error.get().unwrap_or_default()}
                            </div>
                        </Show>

                        <div class="auto-form-actions">
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
                                when=move || selected_name.get().is_some()
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

                        <Show when=show_runs fallback=|| ().into_view()>
                            <div class="auto-runs">
                                <h3 class="auto-runs-title">"Recent runs"</h3>
                                <Show
                                    when=move || runs_loading.get()
                                    fallback=|| ().into_view()
                                >
                                    <div class="pane-list-status">"Loading runs…"</div>
                                </Show>
                                <Show
                                    when=move || runs_error.with(Option::is_some)
                                    fallback=|| ().into_view()
                                >
                                    <div class="pane-list-status pane-list-error">
                                        {move || runs_error.get().unwrap_or_default()}
                                    </div>
                                </Show>
                                <Show
                                    when=move || {
                                        !runs_loading.get()
                                            && runs_error.with(Option::is_none)
                                            && runs.with(Vec::is_empty)
                                    }
                                    fallback=|| ().into_view()
                                >
                                    <div class="pane-list-status">"No runs yet."</div>
                                </Show>
                                <ul class="auto-run-list">
                                    <For
                                        each=move || runs.get()
                                        key=|r| r.id.clone()
                                        children=move |r: AutomationRun| {
                                            let status = r.status.clone();
                                            let badge_class = format!(
                                                "auto-run-badge {}",
                                                run_status_class(&status),
                                            );
                                            let started = fmt_ts(&r.started_at);
                                            let finished = r
                                                .finished_at
                                                .as_deref()
                                                .map(fmt_ts)
                                                .unwrap_or_else(|| "running".to_string());
                                            let kind = trigger_kind(&r.trigger);
                                            let err = r.error.clone();
                                            let rid = r.id.clone();
                                            let rid_active = r.id.clone();
                                            let row_class = move || {
                                                if selected_run.get().as_deref()
                                                    == Some(rid_active.as_str())
                                                {
                                                    "auto-run auto-run-selected"
                                                } else {
                                                    "auto-run"
                                                }
                                            };
                                            view! {
                                                <li>
                                                    <button
                                                        class=row_class
                                                        on:click=move |_| open_run(rid.clone())
                                                    >
                                                        <span class=badge_class>{status}</span>
                                                        <span class="auto-run-when">
                                                            {format!("{started} → {finished}")}
                                                        </span>
                                                        <span class="auto-run-kind">{kind}</span>
                                                        <Show
                                                            when={
                                                                let has = err.is_some();
                                                                move || has
                                                            }
                                                            fallback=|| ().into_view()
                                                        >
                                                            <span class="auto-run-err">
                                                                {err.clone().unwrap_or_default()}
                                                            </span>
                                                        </Show>
                                                    </button>
                                                </li>
                                            }
                                        }
                                    />
                                </ul>

                                <Show
                                    when=move || selected_run.get().is_some()
                                    fallback=|| ().into_view()
                                >
                                    <div class="auto-steps">
                                        <Show
                                            when=move || run_detail_loading.get()
                                            fallback=|| ().into_view()
                                        >
                                            <div class="pane-list-status">"Loading run…"</div>
                                        </Show>
                                        <Show
                                            when=move || run_detail_error.with(Option::is_some)
                                            fallback=|| ().into_view()
                                        >
                                            <div class="pane-list-status pane-list-error">
                                                {move || run_detail_error.get().unwrap_or_default()}
                                            </div>
                                        </Show>
                                        {move || {
                                            run_detail
                                                .get()
                                                .map(|d| {
                                                    if d.steps.is_empty() {
                                                        return view! {
                                                            <div class="pane-list-status">
                                                                "No steps recorded for this run."
                                                            </div>
                                                        }
                                                            .into_any();
                                                    }
                                                    let items = d
                                                        .steps
                                                        .iter()
                                                        .map(|s| {
                                                            let badge = format!(
                                                                "auto-step-badge {}",
                                                                step_status_class(&s.status),
                                                            );
                                                            let status = s.status.clone();
                                                            let kind = action_kind(&s.action);
                                                            let started = fmt_ts(&s.started_at);
                                                            let finished = s
                                                                .finished_at
                                                                .as_deref()
                                                                .map(fmt_ts)
                                                                .unwrap_or_else(|| "—".to_string());
                                                            let ordinal = s.ordinal;
                                                            let (cost, cap) =
                                                                agent_run_cost_and_cap(s.output.as_ref());
                                                            let output = s.output.as_ref().map(compact_json);
                                                            let err = s.error.clone();
                                                            view! {
                                                                <li class="auto-step">
                                                                    <div class="auto-step-head">
                                                                        <span class="auto-step-ord">
                                                                            {format!("#{ordinal}")}
                                                                        </span>
                                                                        <span class=badge>{status}</span>
                                                                        <span class="auto-step-kind">{kind}</span>
                                                                        {cap.map(|note| view! {
                                                                            <span
                                                                                class="auto-step-cap"
                                                                                title="The agent run was truncated before finishing"
                                                                            >
                                                                                {note}
                                                                            </span>
                                                                        })}
                                                                        {cost.map(|c| view! {
                                                                            <span
                                                                                class="auto-step-cost"
                                                                                title="LLM cost for this agent run"
                                                                            >
                                                                                {format!("${c:.4}")}
                                                                            </span>
                                                                        })}
                                                                        <span class="auto-run-when">
                                                                            {format!("{started} → {finished}")}
                                                                        </span>
                                                                    </div>
                                                                    <Show
                                                                        when={
                                                                            let has = output.is_some();
                                                                            move || has
                                                                        }
                                                                        fallback=|| ().into_view()
                                                                    >
                                                                        <pre class="auto-step-out">
                                                                            {output.clone().unwrap_or_default()}
                                                                        </pre>
                                                                    </Show>
                                                                    <Show
                                                                        when={
                                                                            let has = err.is_some();
                                                                            move || has
                                                                        }
                                                                        fallback=|| ().into_view()
                                                                    >
                                                                        <div class="auto-step-err">
                                                                            {err.clone().unwrap_or_default()}
                                                                        </div>
                                                                    </Show>
                                                                </li>
                                                            }
                                                        })
                                                        .collect::<Vec<_>>();
                                                    view! { <ul class="auto-step-list">{items}</ul> }
                                                        .into_any()
                                                })
                                        }}
                                    </div>
                                </Show>
                            </div>
                        </Show>
                    </form>
                </Show>
            </div>
        </section>
    }
}

/// Pretty-print the whole stored automation into the Raw editor's JSON document —
/// the same shape create/update accept, with the server-assigned ids up front
/// (`id`, `workspace_id`) so the view mirrors exactly what is stored. Field order
/// follows the struct's declaration (ids, name, enabled, then the body).
fn automation_json_pretty(a: &Automation) -> String {
    serde_json::to_string_pretty(a).unwrap_or_default()
}

/// The Raw-editor skeleton for a brand-new (unsaved) automation: the create-body
/// shape with empty lists (no ids yet — the server assigns them on save). Name and
/// enabled are still driven by the dedicated fields.
fn new_automation_template() -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "triggers": [],
        "condition": null,
        "actions": [],
    }))
    .unwrap_or_default()
}

/// Parse the Raw editor's single full-automation JSON document into the body parts
/// the save path sends: `(triggers, condition, actions, spec)`. An empty / blank
/// document is an empty linear automation (the server then rejects empty
/// trigger/action lists with a typed `400`). `id` / `workspace_id` / `grant_id` /
/// `name` / `enabled` in the document are ignored here — ids are server-managed and
/// name + enabled come from the dedicated fields. Errors on malformed JSON, a
/// non-object root, or a non-array `triggers` / `actions`. Pure + testable.
#[allow(clippy::type_complexity)]
fn parse_raw_body(
    input: &str,
) -> Result<(Vec<Value>, Option<Value>, Vec<Value>, Option<Value>), String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok((Vec::new(), None, Vec::new(), None));
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| "expected a JSON object".to_string())?;
    // An absent or explicitly-null list is empty; anything non-array is an error.
    let array = |key: &str| -> Result<Vec<Value>, String> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => Ok(items.clone()),
            Some(_) => Err(format!("`{key}` must be a JSON array")),
        }
    };
    let triggers = array("triggers")?;
    let actions = array("actions")?;
    // An absent or null condition/spec is `None`; any other value is kept verbatim.
    let opt = |key: &str| match obj.get(key) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.clone()),
    };
    Ok((triggers, opt("condition"), actions, opt("spec")))
}

/// Best-effort label for what fired a run: the recorded trigger's `kind`, or `—`.
fn trigger_kind(trigger: &Option<Value>) -> String {
    trigger
        .as_ref()
        .and_then(|v| v.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or("—")
        .to_string()
}

/// The CSS modifier class for a run-status badge.
fn run_status_class(status: &str) -> &'static str {
    match status {
        "succeeded" => "auto-run-ok",
        "failed" => "auto-run-fail",
        "running" => "auto-run-running",
        _ => "auto-run-other",
    }
}

/// The CSS modifier class for a step-status badge (`skipped` gets its own shade;
/// the rest reuse the run-status colours).
fn step_status_class(status: &str) -> &'static str {
    match status {
        "skipped" => "auto-run-other",
        other => run_status_class(other),
    }
}

/// Best-effort label for a step's action: its `kind`, or `action`.
fn action_kind(action: &Value) -> String {
    action
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("action")
        .to_string()
}

/// Render a step output / value as compact JSON for the detail view.
fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// Pull `(cost_usd, cap_note)` from an agent step's output JSON for the run detail.
/// `cap_note` names *why* a run was truncated — "budget reached" (the grant's §19
/// `cost_limit` ceiling), "repeated tool loop", or "max tool rounds" — else `None`.
/// Non-agent steps (no such keys) yield `(None, None)`.
fn agent_run_cost_and_cap(output: Option<&Value>) -> (Option<f64>, Option<&'static str>) {
    let Some(o) = output else {
        return (None, None);
    };
    let cost = o.get("cost_usd").and_then(Value::as_f64);
    let cap = if o.get("cost_capped").and_then(Value::as_bool) == Some(true) {
        Some("budget reached")
    } else if o.get("tool_loop_capped").and_then(Value::as_bool) == Some(true) {
        Some("repeated tool loop")
    } else if o.get("iteration_capped").and_then(Value::as_bool) == Some(true) {
        Some("max tool rounds")
    } else {
        None
    };
    (cost, cap)
}

/// Format an RFC 3339 timestamp as a compact `YYYY-MM-DD HH:MM`, falling back to
/// the raw string if it isn't the expected shape (no chrono in the wasm bundle).
fn fmt_ts(rfc3339: &str) -> String {
    match rfc3339.find('T') {
        Some(t) => {
            let date = &rfc3339[..t];
            let hm: String = rfc3339[t + 1..].chars().take(5).collect();
            if hm.len() == 5 {
                format!("{date} {hm}")
            } else {
                rfc3339.to_string()
            }
        }
        None => rfc3339.to_string(),
    }
}

/// The trigger kinds the typed builder can assemble, as `(wire kind, menu label)`
/// in display order. Mirrors `catalerum_automation::Trigger` (SOUL §11).
const TRIGGER_KINDS: &[(&str, &str)] = &[
    ("trigger", "Named signal (on demand)"),
    ("task_moved", "Task moved"),
    ("webhook", "Webhook"),
    ("channel_message", "Channel message"),
    ("collect_email", "Collect email"),
    ("collect_calendar", "Collect calendar"),
    ("collect_sql", "Collect DB rows"),
    ("storage_object", "Storage object"),
    ("schedule", "Schedule (cron)"),
    ("graph_query", "Graph query"),
    ("calendar_event", "Calendar event"),
];

/// One builder field: `(json_key, label, required, multiline)`. Every value is
/// written verbatim as a JSON string. Shared with the flow-canvas trigger node.
pub(crate) type TriggerField = (&'static str, &'static str, bool, bool);

/// The builder fields shown for a trigger `kind` (empty for an unknown kind, or
/// for `calendar_event` whose `lead`/`filter` predicates stay opaque). Optional
/// predicates the builder doesn't model (e.g. a channel `filter` object) are left
/// to the raw-JSON editor. Shared by the Raw-mode trigger builder and the visual
/// canvas's trigger node config.
pub(crate) fn trigger_fields(kind: &str) -> &'static [TriggerField] {
    match kind {
        // A named on-demand signal (e.g. fired from an emerged-UI button via the
        // `fire_trigger` tool); matched exactly on `name`.
        "trigger" => &[("name", "Signal name", true, false)],
        "task_moved" => &[
            ("board", "Board", true, false),
            ("to_column", "To column", true, false),
        ],
        "webhook" => &[("path", "Path", true, false)],
        "channel_message" => &[("channel", "Channel", true, false)],
        // `connection` is set in the visual editor by the node's inline
        // "configure source" form (not typed here); `commit_on` is wired as a node
        // port, not a field. Raw mode keeps `connection` typeable as the escape hatch.
        "collect_email" => &[
            ("connection", "Email connection id", true, false),
            ("mailbox", "Mailbox (optional)", false, false),
        ],
        "collect_calendar" => &[
            ("connection", "Calendar connection id", true, false),
            ("calendar", "Calendar (optional)", false, false),
        ],
        "collect_sql" => &[
            ("connection", "Postgres connection id", true, false),
            ("tables", "Tables pattern (e.g. orders_*)", true, false),
            (
                "cursor_column",
                "Cursor column (optional, auto-detected)",
                false,
                false,
            ),
        ],
        "storage_object" => &[
            ("event", "Event: created / updated / deleted", true, false),
            ("bucket", "Bucket", true, false),
            ("prefix", "Key prefix (optional)", false, false),
            (
                "extensions",
                "Extensions (optional, comma-separated e.g. docx,xlsx,pptx)",
                false,
                false,
            ),
        ],
        "schedule" => &[
            ("cron", "Cron (e.g. 0 9 * * *)", true, false),
            ("tz", "Timezone (optional)", false, false),
        ],
        "graph_query" => &[("query", "Datalog", true, true)],
        _ => &[],
    }
}

/// Whether a builder field's value is a JSON **array of strings** entered as a
/// comma-separated list (e.g. a `storage_object` trigger's `extensions`) rather than
/// a plain JSON string. Shared by the Raw-mode builder and the flow canvas so both
/// serialise the field to the same shape the backend `Trigger` expects.
pub(crate) fn is_string_list_field(key: &str) -> bool {
    key == "extensions"
}

/// Split a comma-separated builder value into trimmed, non-empty tokens.
pub(crate) fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Render a trigger field's current value for the typed builder: a plain string
/// field verbatim, a [string-list field](is_string_list_field) joined back to
/// "a, b, c" so it round-trips through the comma-separated input.
pub(crate) fn trigger_field_display(trigger: &Value, key: &str) -> String {
    if is_string_list_field(key) {
        trigger
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    } else {
        trigger
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }
}

/// Build a trigger spec object from a `kind` + the builder's field `values`.
/// Returns an error (naming the field) if a required field is blank; non-blank
/// optional fields are included, blank ones omitted. A [string-list
/// field](is_string_list_field) becomes a JSON array (empty ⇒ omitted). Pure +
/// testable.
fn build_trigger(kind: &str, values: &HashMap<String, String>) -> Result<Value, String> {
    let mut obj = serde_json::Map::new();
    obj.insert("kind".to_string(), Value::String(kind.to_string()));
    for (key, label, required, _multiline) in trigger_fields(kind) {
        let v = values.get(*key).map(|s| s.trim()).unwrap_or("");
        if v.is_empty() {
            if *required {
                return Err(format!("{label} is required"));
            }
        } else if is_string_list_field(key) {
            let items: Vec<Value> = split_list(v).into_iter().map(Value::String).collect();
            if !items.is_empty() {
                obj.insert((*key).to_string(), Value::Array(items));
            }
        } else {
            obj.insert((*key).to_string(), Value::String(v.to_string()));
        }
    }
    Ok(Value::Object(obj))
}

/// Append a built `trigger` into the Raw JSON document's `triggers` array: parse
/// the existing text as an object (empty/blank → a fresh `{}`), push onto its
/// `triggers` (creating the array if absent), and pretty-print the whole document
/// back. A malformed value, a non-object root, or a non-array `triggers` is an error
/// (surfaced so the user can fix the editor). Pure + testable.
fn append_trigger(existing: &str, trigger: Value) -> Result<String, String> {
    let trimmed = existing.trim();
    let mut value: Value = if trimmed.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?
    };
    let obj = value
        .as_object_mut()
        .ok_or_else(|| "expected a JSON object".to_string())?;
    match obj
        .entry("triggers")
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        Value::Array(items) => items.push(trigger),
        _ => return Err("`triggers` must be a JSON array".to_string()),
    }
    Ok(serde_json::to_string_pretty(&value).unwrap_or_default())
}

// --- Manual-run detection + collect cadence / fire-payload helpers (SOUL §11/§29) ---

/// The trigger `kind` strings an automation carries, read from **both** the linear
/// `triggers` list and any `spec.graph` trigger nodes — so a visual (graph)
/// automation and a legacy linear one both resolve regardless of which shape the
/// backend projects back. Pure + testable.
fn automation_trigger_kinds(a: &Automation) -> Vec<String> {
    let mut kinds: Vec<String> = a
        .triggers
        .iter()
        .filter_map(|t| t.get("kind").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    if let Some(nodes) = a
        .spec
        .as_ref()
        .and_then(|s| s.get("graph"))
        .and_then(|g| g.get("nodes"))
        .and_then(Value::as_array)
    {
        for n in nodes {
            if n.get("kind").and_then(Value::as_str) == Some("trigger") {
                if let Some(k) = n
                    .get("trigger")
                    .and_then(|t| t.get("kind"))
                    .and_then(Value::as_str)
                {
                    kinds.push(k.to_string());
                }
            }
        }
    }
    kinds
}

/// Whether an automation is headed by a **collect** source trigger (`collect_email`
/// / `collect_calendar` / `collect_sql`) — the automations that support a
/// "collect now" immediate poll (SOUL §29). Pure + testable.
fn is_collect_automation(a: &Automation) -> bool {
    automation_trigger_kinds(a)
        .iter()
        .any(|k| k == "collect_email" || k == "collect_calendar" || k == "collect_sql")
}

/// Whether an automation is headed by a **named-signal** `trigger` — the automations
/// that can be fired on demand via `POST /triggers/{name}` (SOUL §11). Pure + testable.
fn is_trigger_automation(a: &Automation) -> bool {
    automation_trigger_kinds(a).iter().any(|k| k == "trigger")
}

/// Render a collect trigger's stored `every` value back into the cadence text input:
/// a JSON string verbatim, a number as its digits, an object (`{"seconds":N}`) as
/// compact JSON. Absent / null → empty. The display inverse of [`every_value`]. Pure.
pub(crate) fn every_display(trigger: &Value) -> String {
    match trigger.get("every") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Parse the raw cadence text into the JSON value to store on a collect trigger's
/// `every` field, persisting **exactly the shape the user typed**: a bare integer
/// becomes a JSON number (minutes — the `every` convention), an explicit JSON
/// object/array (e.g. `{"seconds":90}`) round-trips as that parsed value, and anything
/// else (a duration string like `5m` / `1h30m`) is stored verbatim as a string. Blank
/// → `None` (clears the field → the server's default cadence). Pure + testable.
pub(crate) fn every_value(raw: &str) -> Option<Value> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // A bare integer is minutes — persist it as a JSON number (the `every` convention).
    if let Ok(n) = t.parse::<u64>() {
        return Some(Value::from(n));
    }
    // An explicit JSON object (e.g. {"seconds":90}) round-trips as that parsed value.
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(t) {
            return Some(v);
        }
    }
    // Otherwise a duration string (5m, 1h30m, …) — stored verbatim.
    Some(Value::String(t.to_string()))
}

/// Whether the raw cadence text is one of the documented `every` shapes — a **soft**
/// client-side check driving an inline warning only. The server re-parses and clamps
/// `[60s, 1 year]` at scan time, so this NEVER blocks saving; it just flags an
/// obviously-unrecognized shape. Accepts: blank (every tick); a bare positive integer
/// (minutes); a compact duration string (`30s`, `5m`, `1h`, `1h30m`, `2d`, `1w`, `1y`);
/// or a JSON object carrying a numeric `minutes` / `seconds`. Pure + testable.
pub(crate) fn every_shape_ok(raw: &str) -> bool {
    let t = raw.trim();
    if t.is_empty() || t.parse::<u64>().is_ok() {
        return true;
    }
    if t.starts_with('{') {
        return match serde_json::from_str::<Value>(t) {
            Ok(Value::Object(m)) => {
                m.get("seconds").and_then(Value::as_u64).is_some()
                    || m.get("minutes").and_then(Value::as_u64).is_some()
            }
            _ => false,
        };
    }
    is_duration_string(t)
}

/// Whether `s` is a compact duration string: one or more `<digits><unit>` segments
/// with units `y`/`w`/`d`/`h`/`m`/`s`, and nothing else (e.g. `30s`, `1h30m`). A unit with
/// no preceding number, a trailing bare number, or any stray character fails. This is
/// only a soft shape check — the server's `parse_duration_secs` is the authority.
fn is_duration_string(s: &str) -> bool {
    let mut saw_unit = false;
    let mut has_digits = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            has_digits = true;
        } else if matches!(c, 'y' | 'w' | 'd' | 'h' | 'm' | 's') {
            if !has_digits {
                return false; // a unit with no preceding number
            }
            saw_unit = true;
            has_digits = false;
        } else {
            return false; // a stray character
        }
    }
    saw_unit && !has_digits // ends on a unit, no dangling bare number
}

/// Parse the fire-payload textarea into the optional JSON body for
/// `POST /triggers/{name}`. Empty/whitespace → `None` (no payload); an explicit `null`
/// → `None`; any other well-formed JSON is carried verbatim; malformed JSON is a
/// client-side error the caller surfaces inline before sending (mirrors the server's
/// `parse_payload`). Pure + testable.
fn parse_fire_payload(input: &str) -> Result<Option<Value>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let v: Value = serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    Ok(if v.is_null() { None } else { Some(v) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn agent_run_cost_and_cap_extracts_cost_and_truncation() {
        // No output / non-agent step → nothing.
        assert_eq!(agent_run_cost_and_cap(None), (None, None));
        assert_eq!(
            agent_run_cost_and_cap(Some(&json!({ "content": "hi" }))),
            (None, None)
        );
        // A clean agent run: cost, no cap note.
        assert_eq!(
            agent_run_cost_and_cap(Some(&json!({ "content": "hi", "cost_usd": 0.0123 }))),
            (Some(0.0123), None)
        );
        // Budget-capped takes precedence in the wording.
        assert_eq!(
            agent_run_cost_and_cap(Some(&json!({ "cost_usd": 1.5, "cost_capped": true }))),
            (Some(1.5), Some("budget reached"))
        );
        // Iteration-capped (no cost reported).
        assert_eq!(
            agent_run_cost_and_cap(Some(&json!({ "iteration_capped": true }))),
            (None, Some("max tool rounds"))
        );
        assert_eq!(
            agent_run_cost_and_cap(Some(&json!({ "tool_loop_capped": true }))),
            (None, Some("repeated tool loop"))
        );
    }

    #[test]
    fn parse_raw_body_extracts_body_and_ignores_ids_name_enabled() {
        // A full stored-shape document: only the body parts come back; ids / name /
        // enabled are ignored (name + enabled come from the fields; ids are server-set).
        let (triggers, condition, actions, spec) = parse_raw_body(
            r#"{
                "id": "11111111-1111-1111-1111-111111111111",
                "workspace_id": "22222222-2222-2222-2222-222222222222",
                "name": "digest",
                "enabled": false,
                "triggers": [{"kind":"schedule","cron":"0 9 * * *"}],
                "condition": {"all": true},
                "actions": [{"kind":"summarize"}],
                "spec": {"graph": {"nodes": [], "edges": []}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            triggers,
            vec![json!({"kind":"schedule","cron":"0 9 * * *"})]
        );
        assert_eq!(condition, Some(json!({"all": true})));
        assert_eq!(actions, vec![json!({"kind":"summarize"})]);
        // `spec` (incl. a graph) is carried verbatim so a graph survives a Raw save.
        assert_eq!(spec, Some(json!({"graph": {"nodes": [], "edges": []}})));

        // A blank document is an empty linear automation (all defaults).
        assert_eq!(parse_raw_body("   ").unwrap(), (vec![], None, vec![], None));

        // Absent / explicitly-null body fields default (empty lists, no condition/spec).
        let (t, c, a, s) = parse_raw_body(r#"{"actions":null}"#).unwrap();
        assert!(t.is_empty() && a.is_empty() && c.is_none() && s.is_none());

        // Malformed JSON, a non-object root, and a non-array list are all rejected.
        assert!(parse_raw_body("{bad").is_err());
        assert!(parse_raw_body("[]").is_err());
        assert!(parse_raw_body(r#"{"triggers": {}}"#).is_err());
    }

    #[test]
    fn automation_json_pretty_round_trips_through_parse_raw_body() {
        let a = Automation {
            id: "aaaa".into(),
            workspace_id: "wwww".into(),
            name: "digest".into(),
            enabled: true,
            triggers: vec![json!({"kind":"schedule","cron":"0 9 * * *"})],
            condition: Some(json!({"all": true})),
            actions: vec![json!({"kind":"summarize"})],
            spec: None,
        };
        // The pretty document shows the ids…
        let text = automation_json_pretty(&a);
        assert!(text.contains("\"id\": \"aaaa\""));
        assert!(text.contains("\"workspace_id\": \"wwww\""));
        // …and its body round-trips back through the save-path parser.
        let (triggers, condition, actions, spec) = parse_raw_body(&text).unwrap();
        assert_eq!(triggers, a.triggers);
        assert_eq!(condition, a.condition);
        assert_eq!(actions, a.actions);
        assert_eq!(spec, a.spec);
    }

    #[test]
    fn trigger_kind_extracts_or_dashes() {
        assert_eq!(trigger_kind(&Some(json!({"kind":"webhook"}))), "webhook");
        assert_eq!(trigger_kind(&Some(json!({"no_kind":1}))), "—");
        assert_eq!(trigger_kind(&None), "—");
    }

    #[test]
    fn run_status_class_maps_states() {
        assert_eq!(run_status_class("succeeded"), "auto-run-ok");
        assert_eq!(run_status_class("failed"), "auto-run-fail");
        assert_eq!(run_status_class("running"), "auto-run-running");
        assert_eq!(run_status_class("cancelled"), "auto-run-other");
    }

    #[test]
    fn step_helpers() {
        assert_eq!(step_status_class("succeeded"), "auto-run-ok");
        assert_eq!(step_status_class("skipped"), "auto-run-other");
        assert_eq!(action_kind(&json!({"kind":"notify","to":"ops"})), "notify");
        assert_eq!(action_kind(&json!({"no_kind":1})), "action");
        assert_eq!(compact_json(&json!({"ok":true})), r#"{"ok":true}"#);
    }

    #[test]
    fn fmt_ts_trims_to_minute() {
        assert_eq!(fmt_ts("2026-06-18T09:00:00Z"), "2026-06-18 09:00");
        assert_eq!(fmt_ts("nope"), "nope");
    }

    fn vals(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn build_trigger_requires_fields_and_includes_optionals() {
        // Required fields present → a full trigger object; whitespace is trimmed.
        let t = build_trigger(
            "task_moved",
            &vals(&[("board", " Sprint "), ("to_column", "Done")]),
        )
        .unwrap();
        assert_eq!(
            t,
            json!({ "kind": "task_moved", "board": "Sprint", "to_column": "Done" })
        );

        // A missing required field errors, naming the field's label.
        let err = build_trigger("task_moved", &vals(&[("board", "Sprint")])).unwrap_err();
        assert!(err.contains("To column"), "names the missing field: {err}");

        // Blank optionals are omitted; non-blank ones are included. (`commit_on` is
        // no longer a field — it's wired as a node port in the visual editor.)
        let collect = build_trigger(
            "collect_email",
            &vals(&[("connection", "conn-1"), ("mailbox", "INBOX")]),
        )
        .unwrap();
        assert_eq!(
            collect,
            json!({ "kind": "collect_email", "connection": "conn-1", "mailbox": "INBOX" })
        );

        // A kind with no fields builds a bare `{kind}` (calendar_event is opaque).
        assert_eq!(
            build_trigger("calendar_event", &HashMap::new()).unwrap(),
            json!({ "kind": "calendar_event" })
        );
    }

    #[test]
    fn append_trigger_extends_the_documents_triggers_array() {
        let t = json!({ "kind": "webhook", "path": "/hook" });

        // Empty document → a fresh object whose `triggers` holds the one trigger.
        let out = append_trigger("   ", t.clone()).unwrap();
        let (triggers, ..) = parse_raw_body(&out).unwrap();
        assert_eq!(triggers, vec![t.clone()]);

        // An existing document with a `triggers` array → appended after it, and the
        // rest of the document (ids, actions) is preserved.
        let existing = r#"{
            "id": "x", "actions": [{"kind":"summarize"}],
            "triggers": [{"kind":"schedule","cron":"0 9 * * *"}]
        }"#;
        let out = append_trigger(existing, t.clone()).unwrap();
        assert!(out.contains("\"id\": \"x\""), "keeps ids: {out}");
        let (triggers, _c, actions, _s) = parse_raw_body(&out).unwrap();
        assert_eq!(triggers.len(), 2);
        assert_eq!(triggers[1], t);
        assert_eq!(actions, vec![json!({"kind":"summarize"})]);

        // A document with no `triggers` yet gets the array created.
        let out = append_trigger(r#"{"actions":[]}"#, t.clone()).unwrap();
        assert_eq!(parse_raw_body(&out).unwrap().0, vec![t.clone()]);

        // Malformed JSON, a non-object root, and a non-array `triggers` are errors.
        assert!(append_trigger("{bad", json!({})).is_err());
        assert!(append_trigger("[1,2]", json!({})).is_err());
        assert!(append_trigger(r#"{"triggers": 3}"#, json!({})).is_err());
    }

    #[test]
    fn default_mode_picks_visual_unless_a_legacy_linear_automation() {
        // A brand-new / empty automation (no spec, no triggers) → Visual canvas.
        assert_eq!(default_mode_for(None, false), EditorMode::Visual);
        // An automation that already carries a spec.graph → Visual (round-trips in).
        let graph_spec = json!({ "graph": { "nodes": [], "edges": [] } });
        assert_eq!(
            default_mode_for(Some(&graph_spec), false),
            EditorMode::Visual
        );
        // A graph spec wins even when legacy compiled triggers are also present.
        assert_eq!(
            default_mode_for(Some(&graph_spec), true),
            EditorMode::Visual
        );
        // A legacy linear automation (triggers, no graph) → Raw editor.
        assert_eq!(default_mode_for(None, true), EditorMode::Raw);
        // A non-graph spec with legacy triggers also opens Raw.
        let other_spec = json!({ "note": "legacy" });
        assert_eq!(default_mode_for(Some(&other_spec), true), EditorMode::Raw);
    }

    #[test]
    fn trigger_kinds_build_when_every_field_is_filled() {
        // Every menu kind resolves to a field list and builds a trigger whose
        // `kind` round-trips once all its fields are filled.
        for (kind, _label) in TRIGGER_KINDS {
            let fields = trigger_fields(kind);
            let filled = vals(&fields.iter().map(|(k, ..)| (*k, "x")).collect::<Vec<_>>());
            let built = build_trigger(kind, &filled).expect("builds when filled");
            assert_eq!(built["kind"], json!(*kind));
        }
        // task_moved exposes exactly its two required fields.
        let f = trigger_fields("task_moved");
        assert_eq!(f.len(), 2);
        assert!(f.iter().all(|(_, _, required, _)| *required));
    }

    #[test]
    fn storage_object_extensions_build_as_array_and_round_trip() {
        // The comma-separated `extensions` field becomes a JSON array (trimmed,
        // empties dropped); blank ⇒ omitted entirely.
        let t = build_trigger(
            "storage_object",
            &vals(&[
                ("event", "created"),
                ("bucket", "docs"),
                ("extensions", " docx, xlsx ,, pptx "),
            ]),
        )
        .unwrap();
        assert_eq!(
            t,
            json!({
                "kind": "storage_object", "event": "created", "bucket": "docs",
                "extensions": ["docx", "xlsx", "pptx"]
            })
        );
        // A blank extensions value is omitted (not an empty array).
        let bare = build_trigger(
            "storage_object",
            &vals(&[
                ("event", "deleted"),
                ("bucket", "docs"),
                ("extensions", "  "),
            ]),
        )
        .unwrap();
        assert_eq!(
            bare,
            json!({ "kind": "storage_object", "event": "deleted", "bucket": "docs" })
        );

        // `trigger_field_display` renders the array back to the comma-separated input
        // so it round-trips through the typed builder; a plain string field is verbatim.
        assert_eq!(trigger_field_display(&t, "extensions"), "docx, xlsx, pptx");
        assert_eq!(trigger_field_display(&t, "event"), "created");
        assert_eq!(trigger_field_display(&bare, "extensions"), "");
    }

    /// Build a minimal `Automation` from its `triggers` list and optional `spec`.
    fn auto(triggers: Vec<Value>, spec: Option<Value>) -> Automation {
        Automation {
            id: "a1".into(),
            workspace_id: "w1".into(),
            name: "auto".into(),
            enabled: true,
            triggers,
            condition: None,
            actions: Vec::new(),
            spec,
        }
    }

    #[test]
    fn detects_collect_and_trigger_from_linear_triggers() {
        let collect = auto(
            vec![json!({ "kind": "collect_email", "connection": "c1" })],
            None,
        );
        assert!(is_collect_automation(&collect));
        assert!(!is_trigger_automation(&collect));

        let cal = auto(
            vec![json!({ "kind": "collect_calendar", "connection": "c2" })],
            None,
        );
        assert!(is_collect_automation(&cal));

        let named = auto(vec![json!({ "kind": "trigger", "name": "rebuild" })], None);
        assert!(is_trigger_automation(&named));
        assert!(!is_collect_automation(&named));

        // A plain schedule automation is neither.
        let sched = auto(
            vec![json!({ "kind": "schedule", "cron": "0 9 * * *" })],
            None,
        );
        assert!(!is_collect_automation(&sched));
        assert!(!is_trigger_automation(&sched));
    }

    #[test]
    fn detects_collect_and_trigger_from_graph_spec() {
        // A visual automation carries its trigger inside `spec.graph.nodes[].trigger`.
        let spec = json!({
            "graph": {
                "nodes": [
                    { "id": "t1", "kind": "trigger", "trigger": { "kind": "collect_email" } },
                    { "id": "w1", "kind": "action", "action": { "kind": "write_email" } }
                ],
                "edges": []
            }
        });
        let a = auto(Vec::new(), Some(spec));
        assert!(is_collect_automation(&a));
        assert!(!is_trigger_automation(&a));

        let named_spec = json!({
            "graph": { "nodes": [
                { "id": "t1", "kind": "trigger", "trigger": { "kind": "trigger", "name": "go" } }
            ], "edges": [] }
        });
        let n = auto(Vec::new(), Some(named_spec));
        assert!(is_trigger_automation(&n));
        assert!(!is_collect_automation(&n));

        // An empty/absent graph resolves to no kinds.
        assert!(automation_trigger_kinds(&auto(Vec::new(), None)).is_empty());
    }

    #[test]
    fn every_shape_ok_accepts_documented_shapes_and_flags_junk() {
        // Blank = "every tick"; a bare integer = minutes.
        assert!(every_shape_ok(""));
        assert!(every_shape_ok("  "));
        assert!(every_shape_ok("5"));
        assert!(every_shape_ok("300"));
        // Duration strings.
        assert!(every_shape_ok("30s"));
        assert!(every_shape_ok("5m"));
        assert!(every_shape_ok("1h"));
        assert!(every_shape_ok("1h30m"));
        assert!(every_shape_ok("2d"));
        assert!(every_shape_ok("1w"));
        assert!(every_shape_ok("1y"));
        assert!(every_shape_ok("  5m  ")); // trimmed
                                           // JSON objects with a numeric seconds/minutes.
        assert!(every_shape_ok(r#"{"seconds":90}"#));
        assert!(every_shape_ok(r#"{"minutes":15}"#));
        // Unrecognized shapes warn (but never block).
        assert!(!every_shape_ok("5x"));
        assert!(!every_shape_ok("abc"));
        assert!(!every_shape_ok("m"));
        assert!(!every_shape_ok("5m30")); // dangling bare number
        assert!(!every_shape_ok(r#"{"hours":2}"#)); // unknown key
        assert!(!every_shape_ok("{bad json"));
    }

    #[test]
    fn every_value_persists_typed_shape_and_round_trips_display() {
        // Bare integer → JSON number (minutes).
        assert_eq!(every_value("300"), Some(json!(300)));
        assert_eq!(every_display(&json!({ "every": 300 })), "300");
        // Duration string → verbatim string.
        assert_eq!(every_value("1h30m"), Some(json!("1h30m")));
        assert_eq!(every_display(&json!({ "every": "1h30m" })), "1h30m");
        // JSON object → parsed object.
        assert_eq!(
            every_value(r#"{"seconds":90}"#),
            Some(json!({ "seconds": 90 }))
        );
        assert_eq!(
            every_display(&json!({ "every": { "seconds": 90 } })),
            r#"{"seconds":90}"#
        );
        // Blank → None (clears the field); absent every displays empty.
        assert_eq!(every_value("   "), None);
        assert_eq!(every_display(&json!({ "kind": "collect_email" })), "");
        assert_eq!(every_display(&json!({ "every": null })), "");
        // Whitespace is trimmed before persisting.
        assert_eq!(every_value("  5m  "), Some(json!("5m")));
    }

    #[test]
    fn parse_fire_payload_handles_empty_null_and_json() {
        assert_eq!(parse_fire_payload("").unwrap(), None);
        assert_eq!(parse_fire_payload("  \n ").unwrap(), None);
        assert_eq!(parse_fire_payload("null").unwrap(), None);
        assert_eq!(
            parse_fire_payload(r#"{"row":3}"#).unwrap(),
            Some(json!({ "row": 3 }))
        );
        // A non-object JSON value is still a valid payload (carried verbatim).
        assert_eq!(parse_fire_payload(r#""hi""#).unwrap(), Some(json!("hi")));
        // Malformed JSON is a client-side error surfaced before send.
        assert!(parse_fire_payload("{not json").is_err());
    }
}
