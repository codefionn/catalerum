//! The **MCP Endpoints** panel (SOUL §30, §26) — a manager for the workspace's
//! user-authored, Boa-scripted MCP endpoints.
//!
//! A two-pane workbench panel mirroring [`super::skills::SkillsPanel`]: a
//! searchable left list of the workspace's scripted endpoints and a right pane
//! that **reads** before it **writes**. Selecting an endpoint shows a rendered
//! card — its scope pins, the authority it runs under, the ready-to-connect
//! `/mcp/e/{name}` URL, and its script — with an "Edit" toggle into a guided form
//! and a "Mint public share link" action (`/mcp/s/{token}`). It is a thin client
//! of the endpoints REST surface (`/mcp-endpoints`, `/mcp-endpoints/{id}`,
//! `/mcp-endpoints/{id}/token`); every call carries the dev session token and is
//! workspace-scoped server-side (SOUL §18).
//!
//! Unlike a skill (keyed by its fixed name), an endpoint is keyed by `id`, so its
//! name is editable — a rename `PUT`s the same record. Each endpoint is a
//! JavaScript program that declares its MCP tools (`tools/list`) and implements
//! their `tools/call`, reaching a single narrow host bridge —
//! `catalerum.callTool("search_semantic", …)` — pinned to the endpoint's
//! Bucket/Key-prefix scope so a script can never widen its own reach.
//!
//! To *connect* an external product (Claude Code, Codex, …) to a chosen endpoint
//! with copy-paste config, see the settings "MCP clients" section
//! ([`super::mcp_connect`]); this panel is where the endpoints themselves are
//! authored.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::dialogs::{use_dialogs, ConfirmSpec};
use super::widgets::{copy_button, copy_to_clipboard, list_drawer_scrim, list_drawer_toggle};
use crate::api::{Grant, McpEndpoint, McpEndpointBody, MintEndpointToken};
use crate::auth;
use crate::rest;

/// The starter program a new endpoint opens with: a minimal-but-working example
/// that advertises one `search` tool and implements it against the pinned scope.
const DEFAULT_SCRIPT: &str = r#"// A scripted MCP endpoint (SOUL §30). This program runs on every request;
// `input.method` says which phase you are in. Return the tool list on
// discovery, and run the tool on a call. The only host call available is
// `catalerum.callTool("search_semantic", { query })`, and its search is pinned
// to this endpoint's Bucket / Key prefix — a script can never widen its reach.

if (input.method === "tools/list") {
  // Advertise the tools this endpoint exposes.
  return [
    {
      name: "search",
      description: "Search this endpoint's documents.",
      inputSchema: {
        type: "object",
        properties: { query: { type: "string", description: "What to look for" } },
        required: ["query"],
      },
    },
  ];
}

if (input.method === "tools/call" && input.name === "search") {
  var query = (input.arguments && input.arguments.query) || "";
  return catalerum.callTool("search_semantic", { query: query });
}

return null;
"#;

/// The MCP Endpoints panel component.
#[component]
pub fn McpEndpointsPanel() -> impl IntoView {
    // The shared confirm dialog (discard-changes + delete confirmation).
    let dialogs = use_dialogs();
    // The API origin the connect/share URLs point at — fixed per mount.
    let base = StoredValue::new(crate::api::api_base());

    // Loaded list + load state.
    let endpoints = RwSignal::new(Vec::<McpEndpoint>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);
    // Free-text search over the list (name / description / script).
    let query = RwSignal::new(String::new());

    // The workspace's §19 grants, backing the authority picker (best-effort:
    // without them the picker still offers the default read-only authority).
    let grants = RwSignal::new(Vec::<Grant>::new());

    // Right-pane state. `selected_id` is the open endpoint (None + `is_new` = an
    // unsaved draft; None + !`is_new` = nothing open). `editing` distinguishes the
    // rendered read view (false) from the authoring form (true). `current` holds
    // the loaded endpoint so the read view renders it and Cancel can revert.
    let selected_id = RwSignal::new(Option::<String>::None);
    let is_new = RwSignal::new(false);
    let editing = RwSignal::new(false);
    let current = RwSignal::new(Option::<McpEndpoint>::None);

    let edit_name = RwSignal::new(String::new());
    let edit_description = RwSignal::new(String::new());
    let edit_enabled = RwSignal::new(true);
    let edit_bucket = RwSignal::new(String::new());
    let edit_prefix = RwSignal::new(String::new());
    let edit_grant = RwSignal::new(String::new());
    let edit_script = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // A minted public share URL for the open endpoint (`/mcp/s/{token}`), reset
    // whenever the selection changes (it is per-endpoint).
    let share = RwSignal::new(Option::<crate::api::MintedEndpointToken>::None);
    let sharing = RwSignal::new(false);
    let share_error = RwSignal::new(Option::<String>::None);

    // Blank every editor field — a cleared editor.
    let clear_editor = move || {
        edit_name.set(String::new());
        edit_description.set(String::new());
        edit_enabled.set(true);
        edit_bucket.set(String::new());
        edit_prefix.set(String::new());
        edit_grant.set(String::new());
        edit_script.set(String::new());
    };

    // Load an endpoint's fields into the editor signals (the form's starting
    // point and what Cancel reverts to).
    let populate_editor = move |ep: &McpEndpoint| {
        edit_name.set(ep.name.clone());
        edit_description.set(ep.description.clone());
        edit_enabled.set(ep.enabled);
        edit_bucket.set(ep.bucket_name.clone().unwrap_or_default());
        edit_prefix.set(ep.key_prefix.clone().unwrap_or_default());
        edit_grant.set(ep.grant_id.clone().unwrap_or_default());
        edit_script.set(ep.script.clone());
    };

    // Open an endpoint in the rendered read view.
    let load_into_view = move |ep: &McpEndpoint| {
        selected_id.set(Some(ep.id.clone()));
        is_new.set(false);
        editing.set(false);
        current.set(Some(ep.clone()));
        populate_editor(ep);
        save_error.set(None);
        share.set(None);
        share_error.set(None);
    };

    // Fetch the endpoints list. When `auto_select` and nothing is open, open the
    // first endpoint so the pane isn't empty on first paint.
    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_mcp_endpoints(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_id.get_untracked().is_none()
                    {
                        if let Some(first) = list.first() {
                            load_into_view(first);
                        }
                    }
                    endpoints.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    endpoints.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Initial loads: the list, and the grants for the authority picker.
    refresh(true);
    spawn_local(async move {
        let token = auth::resolve_token();
        if let Ok(list) = rest::list_grants(token.as_deref()).await {
            grants.set(list);
        }
    });

    // Whether the form differs from the open endpoint (or, for a new draft, from
    // the starting template) — i.e. unsaved work a reset would throw away.
    let editor_is_dirty = move || {
        let (b_name, b_desc, b_enabled, b_bucket, b_prefix, b_grant, b_script) =
            match current.get_untracked() {
                Some(ep) => (
                    ep.name,
                    ep.description,
                    ep.enabled,
                    ep.bucket_name.unwrap_or_default(),
                    ep.key_prefix.unwrap_or_default(),
                    ep.grant_id.unwrap_or_default(),
                    ep.script,
                ),
                None => (
                    String::new(),
                    String::new(),
                    true,
                    String::new(),
                    String::new(),
                    String::new(),
                    DEFAULT_SCRIPT.to_string(),
                ),
            };
        edit_name.get_untracked().trim() != b_name
            || edit_description.get_untracked().trim() != b_desc
            || edit_enabled.get_untracked() != b_enabled
            || edit_bucket.get_untracked().trim() != b_bucket
            || edit_prefix.get_untracked().trim() != b_prefix
            || edit_grant.get_untracked() != b_grant
            || edit_script.get_untracked() != b_script
    };

    // Gate a destructive editor reset (selecting another endpoint, or "New")
    // behind a discard confirmation. Cancel and Save have their own intent, so
    // they don't go through this.
    let guard_discard = move |proceed: Box<dyn Fn()>| {
        if !editing.get_untracked() || !editor_is_dirty() {
            proceed();
        } else {
            dialogs.confirm(
                ConfirmSpec::danger(
                    "Discard changes?",
                    "Discard unsaved changes to this endpoint?",
                    "Discard",
                ),
                proceed,
            );
        }
    };

    // Begin a new, unsaved endpoint (a blank form with the starter script).
    let start_new = move || {
        guard_discard(Box::new(move || {
            selected_id.set(None);
            is_new.set(true);
            editing.set(true);
            current.set(None);
            clear_editor();
            edit_script.set(DEFAULT_SCRIPT.to_string());
            save_error.set(None);
            share.set(None);
            share_error.set(None);
        }));
    };

    // Switch the open endpoint from read view into the authoring form.
    let start_edit = move || {
        editing.set(true);
        save_error.set(None);
    };

    // Leave the form: discard a new draft, or revert an existing endpoint.
    let cancel_edit = move || {
        if is_new.get_untracked() {
            selected_id.set(None);
            is_new.set(false);
            editing.set(false);
            current.set(None);
            clear_editor();
        } else {
            if let Some(ep) = current.get_untracked() {
                populate_editor(&ep);
            }
            editing.set(false);
        }
        save_error.set(None);
    };

    // Save the form: create a new endpoint or replace the open one.
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        let name = edit_name.get_untracked().trim().to_string();
        if name.is_empty() {
            save_error.set(Some("Give the endpoint a name.".to_string()));
            return;
        }
        let body = McpEndpointBody {
            name,
            description: edit_description.get_untracked().trim().to_string(),
            script: edit_script.get_untracked(),
            bucket_name: blank_to_none(&edit_bucket.get_untracked()),
            key_prefix: blank_to_none(&edit_prefix.get_untracked()),
            grant_id: blank_to_none(&edit_grant.get_untracked()),
            enabled: edit_enabled.get_untracked(),
        };
        let new = is_new.get_untracked();
        let id = selected_id.get_untracked().unwrap_or_default();

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            let result = if new {
                rest::create_mcp_endpoint(tok, &body).await
            } else {
                rest::update_mcp_endpoint(tok, &id, &body).await
            };
            saving.set(false);
            match result {
                Ok(ep) => {
                    load_into_view(&ep);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    // Delete the open endpoint (by id), behind a confirmation.
    let delete = move || {
        let Some(id) = selected_id.get_untracked() else {
            return;
        };
        if saving.get_untracked() {
            return;
        }
        let name = current
            .get_untracked()
            .map(|e| e.name)
            .unwrap_or_else(|| "this endpoint".to_string());
        dialogs.confirm(
            ConfirmSpec::danger(
                "Delete endpoint?",
                format!("Delete “{name}”? Any share links stop working immediately."),
                "Delete",
            ),
            move || {
                let id = id.clone();
                saving.set(true);
                save_error.set(None);
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::delete_mcp_endpoint(token.as_deref(), &id).await {
                        Ok(()) => {
                            saving.set(false);
                            selected_id.set(None);
                            is_new.set(false);
                            editing.set(false);
                            current.set(None);
                            clear_editor();
                            share.set(None);
                            refresh(true);
                        }
                        Err(e) => {
                            saving.set(false);
                            save_error.set(Some(e.to_string()));
                        }
                    }
                });
            },
        );
    };

    // Mint a signed public share URL (server default TTL) for the open endpoint —
    // the credential rides in the `/mcp/s/{token}` path, no bearer header needed.
    let mint_share = move || {
        let Some(ep) = current.get_untracked() else {
            return;
        };
        if ep.id.is_empty() || sharing.get_untracked() {
            return;
        }
        sharing.set(true);
        share_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let body = MintEndpointToken { ttl_days: None };
            match rest::mint_mcp_endpoint_token(token.as_deref(), &ep.id, &body).await {
                Ok(minted) => share.set(Some(minted)),
                Err(e) => share_error.set(Some(e.to_string())),
            }
            sharing.set(false);
        });
    };

    // Something is open on the right (an endpoint in view/edit, or a new draft).
    let editor_open = move || selected_id.get().is_some() || is_new.get();
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // The list after the free-text filter.
    let visible = move || {
        endpoints.with(|list| {
            query.with(|q| {
                let q = q.trim();
                if q.is_empty() {
                    list.clone()
                } else {
                    list.iter()
                        .filter(|e| endpoint_matches_query(e, q))
                        .cloned()
                        .collect()
                }
            })
        })
    };

    // Read-view projections off the open endpoint (reactive: re-render on select).
    let view_name =
        move || current.with(|e| e.as_ref().map(|e| e.name.clone()).unwrap_or_default());
    let view_description = move || {
        current.with(|e| {
            e.as_ref()
                .map(|e| e.description.clone())
                .unwrap_or_default()
        })
    };
    let view_enabled = move || current.with(|e| e.as_ref().map(|e| e.enabled).unwrap_or(true));
    let view_bucket = move || {
        current.with(|e| {
            e.as_ref()
                .and_then(|e| e.bucket_name.clone())
                .unwrap_or_default()
        })
    };
    let view_prefix = move || {
        current.with(|e| {
            e.as_ref()
                .and_then(|e| e.key_prefix.clone())
                .unwrap_or_default()
        })
    };
    let view_grant = move || {
        current.with(|e| {
            e.as_ref()
                .and_then(|e| e.grant_id.clone())
                .unwrap_or_default()
        })
    };
    let view_script =
        move || current.with(|e| e.as_ref().map(|e| e.script.clone()).unwrap_or_default());
    // The bearer-authenticated serve URL for the open endpoint.
    let serve_url = move || mcpe_serve_url(&base.get_value(), &view_name());
    // The grant's display name (falls back to the raw id when it isn't in the
    // loaded list, e.g. a since-deleted grant).
    let grant_label = move || {
        let id = view_grant();
        if id.is_empty() {
            return String::new();
        }
        grants
            .with(|list| list.iter().find(|g| g.id == id).map(|g| g.name.clone()))
            .unwrap_or_else(|| format!("{id} (unknown grant)"))
    };

    // The authority-picker options: the default read-only authority, every
    // workspace grant, and — when the open endpoint points at a grant no longer
    // in the list — a synthetic row so a save can't silently drop it.
    let grant_options = move || {
        let current_grant = edit_grant.get();
        let list = grants.get();
        let mut seen = current_grant.is_empty();
        let mut opts = vec![
            view! { <option value="">"None — read-only search (default)"</option> }.into_any(),
        ];
        for g in &list {
            if g.id == current_grant {
                seen = true;
            }
            let id = g.id.clone();
            opts.push(view! { <option value=id>{g.name.clone()}</option> }.into_any());
        }
        if !seen {
            let id = current_grant.clone();
            opts.push(
                view! { <option value=id.clone()>{format!("{id} (unknown grant)")}</option> }
                    .into_any(),
            );
        }
        opts
    };

    // Whether the list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list mcpe-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"MCP Endpoints"</h2>
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
                    <Show when=move || !endpoints.with(Vec::is_empty) fallback=|| ().into_view()>
                        <input
                            class="pane-search"
                            placeholder="Search endpoints…"
                            prop:value=move || query.get()
                            on:input=move |ev| query.set(event_target_value(&ev))
                        />
                    </Show>

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
                                    "Could not load endpoints: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get() && load_error.with(Option::is_none)
                                && endpoints.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No endpoints yet. Create one →"</div>
                    </Show>

                    <Show
                        when=move || !endpoints.with(Vec::is_empty) && visible().is_empty()
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No endpoints match."</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || visible()
                            key=|e| {
                                (e.id.clone(), e.name.clone(), e.description.clone(), e.enabled)
                            }
                            children=move |e: McpEndpoint| {
                                let id = e.id.clone();
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
                                let label = e.name.clone();
                                let desc = e.description.clone();
                                let disabled_badge = !e.enabled;
                                let id_for_click = e.id.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            on:click=move |_| {
                                                let id_for_click = id_for_click.clone();
                                                guard_discard(Box::new(move || {
                                                    if let Some(ep) = endpoints
                                                        .get_untracked()
                                                        .into_iter()
                                                        .find(|x| x.id == id_for_click)
                                                    {
                                                        load_into_view(&ep);
                                                        list_open.set(false);
                                                    }
                                                }));
                                            }
                                        >
                                            <span class="pane-item-title">
                                                {label}
                                                <Show
                                                    when=move || disabled_badge
                                                    fallback=|| ().into_view()
                                                >
                                                    <span class="mcpe-tag">"off"</span>
                                                </Show>
                                            </span>
                                            <Show
                                                when={
                                                    let has = !desc.is_empty();
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="pane-item-preview">{desc.clone()}</span>
                                            </Show>
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            {list_drawer_toggle("Endpoints", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select an endpoint, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <Show
                        when=move || editing.get()
                        fallback=move || {
                            view! {
                                <div class="mcpe-view">
                                    <header class="mcpe-view-header">
                                        <h2 class="mcpe-view-name">
                                            {view_name}
                                            {move || {
                                                if view_enabled() {
                                                    view! {
                                                        <span class="mcpe-status-badge mcpe-status-on">
                                                            "enabled"
                                                        </span>
                                                    }
                                                        .into_any()
                                                } else {
                                                    view! {
                                                        <span class="mcpe-status-badge mcpe-status-off">
                                                            "disabled"
                                                        </span>
                                                    }
                                                        .into_any()
                                                }
                                            }}
                                        </h2>
                                        <div class="mcpe-form-actions">
                                            <button
                                                class="pane-btn pane-btn-primary"
                                                type="button"
                                                disabled=move || saving.get()
                                                on:click=move |_| start_edit()
                                            >
                                                "Edit"
                                            </button>
                                            <button
                                                class="pane-btn pane-btn-danger"
                                                type="button"
                                                disabled=move || saving.get()
                                                on:click=move |_| delete()
                                            >
                                                "Delete"
                                            </button>
                                        </div>
                                    </header>

                                    <Show
                                        when=move || !view_description().is_empty()
                                        fallback=|| ().into_view()
                                    >
                                        <p class="mcpe-view-desc">{view_description}</p>
                                    </Show>

                                    <section class="mcpe-section">
                                        <div class="mcpe-section-label">"Connect URL"</div>
                                        <div class="mcp-url-row">
                                            <span class="mcp-url">{serve_url}</span>
                                            {copy_button(serve_url, "Copy", "Copied ✓", "pane-btn")}
                                        </div>
                                        <p class="mcpe-muted">
                                            "A bearer token authenticates this URL. For ready-to-paste "
                                            "client config, see Settings → MCP clients."
                                        </p>
                                        <div class="mcpe-form-actions">
                                            <button
                                                class="pane-btn"
                                                type="button"
                                                disabled=move || sharing.get()
                                                on:click=move |_| mint_share()
                                            >
                                                {move || {
                                                    if sharing.get() {
                                                        "Minting…"
                                                    } else {
                                                        "Mint public share link"
                                                    }
                                                }}
                                            </button>
                                        </div>
                                        <Show
                                            when=move || share_error.with(Option::is_some)
                                            fallback=|| ().into_view()
                                        >
                                            <div class="mcpe-form-error">
                                                {move || share_error.get().unwrap_or_default()}
                                            </div>
                                        </Show>
                                        {move || {
                                            share
                                                .get()
                                                .map(|s| {
                                                    let full = format!(
                                                        "{}{}",
                                                        base.get_value().trim_end_matches('/'),
                                                        s.path,
                                                    );
                                                    let days = days_until(
                                                        s.expires_at,
                                                        js_sys::Date::now(),
                                                    );
                                                    let for_copy = full.clone();
                                                    view! {
                                                        <div class="mcpe-notice">
                                                            <div class="mcp-url-row">
                                                                <span class="mcp-url">{full}</span>
                                                                <button
                                                                    class="pane-btn"
                                                                    type="button"
                                                                    on:click=move |_| {
                                                                        copy_to_clipboard(&for_copy)
                                                                    }
                                                                >
                                                                    "Copy"
                                                                </button>
                                                            </div>
                                                            {format!(
                                                                "Public link — the credential is in the URL, no header \
                                                                 needed. Anyone with it can call this endpoint for {days} \
                                                                 days.",
                                                            )}
                                                        </div>
                                                    }
                                                })
                                        }}
                                    </section>

                                    <section class="mcpe-section">
                                        <div class="mcpe-section-label">"Scope"</div>
                                        {move || {
                                            let bucket = view_bucket();
                                            let prefix = view_prefix();
                                            if bucket.is_empty() && prefix.is_empty() {
                                                view! {
                                                    <p class="mcpe-muted">
                                                        "Any bucket, no prefix (unrestricted search)."
                                                    </p>
                                                }
                                                    .into_any()
                                            } else {
                                                view! {
                                                    <div class="mcpe-chips">
                                                        <Show
                                                            when={
                                                                let has = !bucket.is_empty();
                                                                move || has
                                                            }
                                                            fallback=|| ().into_view()
                                                        >
                                                            <span class="mcpe-chip">
                                                                {format!("bucket: {bucket}")}
                                                            </span>
                                                        </Show>
                                                        <Show
                                                            when={
                                                                let has = !prefix.is_empty();
                                                                move || has
                                                            }
                                                            fallback=|| ().into_view()
                                                        >
                                                            <span class="mcpe-chip">
                                                                {format!("prefix: {prefix}")}
                                                            </span>
                                                        </Show>
                                                    </div>
                                                }
                                                    .into_any()
                                            }
                                        }}
                                    </section>

                                    <section class="mcpe-section">
                                        <div class="mcpe-section-label">"Authority"</div>
                                        {move || {
                                            let g = grant_label();
                                            if g.is_empty() {
                                                view! {
                                                    <p class="mcpe-muted">
                                                        "Default — read-only semantic search."
                                                    </p>
                                                }
                                                    .into_any()
                                            } else {
                                                view! {
                                                    <div class="mcpe-chips">
                                                        <span class="mcpe-chip">{format!("grant: {g}")}</span>
                                                    </div>
                                                }
                                                    .into_any()
                                            }
                                        }}
                                    </section>

                                    <section class="mcpe-section">
                                        <div class="mcpe-section-label">"Script"</div>
                                        <pre class="mcpe-codeblock"><code>{view_script}</code></pre>
                                    </section>
                                </div>
                            }
                        }
                    >
                        <form class="mcpe-form" on:submit=on_save_submit>
                            <div class="pf-group-title">"Basics"</div>
                            <div class="pf-field">
                                <span class="pf-label">"Name"</span>
                                <span class="pf-help">
                                    "URL-safe slug — the /mcp/e/{name} path segment. Renaming is allowed."
                                </span>
                                <input
                                    class="mcpe-input mcpe-input-name"
                                    placeholder="e.g. wiki-docs"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_name.get()
                                    on:input=move |ev| edit_name.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="pf-field">
                                <span class="pf-label">"Description"</span>
                                <span class="pf-help">
                                    "A one-line summary shown in the list and the connect picker."
                                </span>
                                <input
                                    class="mcpe-input"
                                    placeholder="One-line description"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_description.get()
                                    on:input=move |ev| edit_description.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="pf-field">
                                <label class="mcpe-check">
                                    <input
                                        type="checkbox"
                                        disabled=move || saving.get()
                                        prop:checked=move || edit_enabled.get()
                                        on:change=move |ev| edit_enabled.set(event_target_checked(&ev))
                                    />
                                    "Enabled — serve this endpoint (disabled 404s, stays editable)"
                                </label>
                            </div>

                            <div class="pf-group-title">"Scope"</div>
                            <div class="pf-field">
                                <span class="pf-help">
                                    "Pin the script's search to one bucket / key prefix. The host injects "
                                    "these into every search, so a script can never widen its reach. Leave "
                                    "blank for an unrestricted search."
                                </span>
                                <input
                                    class="mcpe-input"
                                    placeholder="Bucket name (optional)"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_bucket.get()
                                    on:input=move |ev| edit_bucket.set(event_target_value(&ev))
                                />
                                <input
                                    class="mcpe-input"
                                    placeholder="Key prefix, e.g. acme/docs/ (optional)"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_prefix.get()
                                    on:input=move |ev| edit_prefix.set(event_target_value(&ev))
                                />
                            </div>

                            <div class="pf-group-title">"Authority"</div>
                            <div class="pf-field">
                                <span class="pf-help">
                                    "The §19 grant whose capabilities the script runs under. The default is "
                                    "a minimal read-only search authority."
                                </span>
                                <select
                                    class="mcpe-input"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_grant.get()
                                    on:change=move |ev| edit_grant.set(event_target_value(&ev))
                                >
                                    {grant_options}
                                </select>
                            </div>

                            <div class="pf-group-title">"Script"</div>
                            <div class="pf-field">
                                <span class="pf-help">
                                    "A JavaScript program. Return the tool list for input.method === \"tools/list\", "
                                    "and run the tool for \"tools/call\" via catalerum.callTool(\"search_semantic\", …)."
                                </span>
                                <textarea
                                    class="mcpe-textarea"
                                    placeholder="// endpoint script…"
                                    spellcheck="false"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_script.get()
                                    on:input=move |ev| edit_script.set(event_target_value(&ev))
                                ></textarea>
                            </div>

                            <Show
                                when=move || save_error.with(Option::is_some)
                                fallback=|| ().into_view()
                            >
                                <div class="mcpe-form-error">
                                    {move || save_error.get().unwrap_or_default()}
                                </div>
                            </Show>

                            <div class="mcpe-form-actions">
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
                                <button
                                    class="pane-btn"
                                    type="button"
                                    disabled=move || saving.get()
                                    on:click=move |_| cancel_edit()
                                >
                                    "Cancel"
                                </button>
                            </div>
                        </form>
                    </Show>
                </Show>
            </div>
        </section>
    }
}

/// The bearer-authenticated serve URL for an endpoint: `{base}/mcp/e/{name}`.
fn mcpe_serve_url(base: &str, name: &str) -> String {
    format!("{}/mcp/e/{name}", base.trim_end_matches('/'))
}

/// Trim `s`; an empty result is `None` (so a blank scope pin / grant is omitted
/// from the request rather than sent as `""`).
fn blank_to_none(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Whether an endpoint matches a free-text `query` — a case-insensitive substring
/// of its name, description, or script. `query` is assumed trimmed.
fn endpoint_matches_query(ep: &McpEndpoint, query: &str) -> bool {
    let q = query.to_lowercase();
    ep.name.to_lowercase().contains(&q)
        || ep.description.to_lowercase().contains(&q)
        || ep.script.to_lowercase().contains(&q)
}

/// Days until a Unix-seconds expiry, floored at zero — for the share-URL note.
fn days_until(expires_at: i64, now_ms: f64) -> i64 {
    let ms_left = (expires_at as f64) * 1000.0 - now_ms;
    (ms_left / 86_400_000.0).ceil().max(0.0) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(name: &str, description: &str, script: &str) -> McpEndpoint {
        McpEndpoint {
            id: String::new(),
            name: name.to_string(),
            description: description.to_string(),
            script: script.to_string(),
            bucket_name: None,
            key_prefix: None,
            grant_id: None,
            enabled: true,
        }
    }

    #[test]
    fn serve_url_joins_and_trims_trailing_slash() {
        assert_eq!(
            mcpe_serve_url("https://api.example.com/", "wiki"),
            "https://api.example.com/mcp/e/wiki"
        );
        assert_eq!(
            mcpe_serve_url("https://api.example.com", "wiki"),
            "https://api.example.com/mcp/e/wiki"
        );
    }

    #[test]
    fn blank_to_none_maps_empty_and_trims() {
        assert_eq!(blank_to_none("  "), None);
        assert_eq!(blank_to_none(""), None);
        assert_eq!(blank_to_none("  acme/ "), Some("acme/".to_string()));
    }

    #[test]
    fn matches_query_searches_name_description_and_script() {
        let e = ep(
            "wiki-docs",
            "Search the wiki",
            "catalerum.callTool(\"search_semantic\", { query })",
        );
        assert!(endpoint_matches_query(&e, "wiki"));
        assert!(endpoint_matches_query(&e, "SEARCH"));
        assert!(endpoint_matches_query(&e, "search_semantic"));
        assert!(!endpoint_matches_query(&e, "zzz"));
    }

    #[test]
    fn share_expiry_days_floor_at_zero() {
        assert_eq!(days_until(864_000, 0.0), 10);
        assert_eq!(days_until(0, 864_000_000.0), 0);
    }
}
