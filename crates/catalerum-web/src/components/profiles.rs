//! The Agent Profiles panel (SOUL §19/§25 — scoped-agent manager).
//!
//! A two-pane workbench panel: a left list of the workspace's agent profiles and
//! a right editor with create / save / delete. It is a thin client of the
//! agent-profile REST surface (`/agent-profiles`, `/agent-profiles/{name}`) —
//! every call carries the dev session token and is workspace-scoped server-side
//! (SOUL §18).
//!
//! A profile is the durable form of the §19 agent: it bundles a model, a tool /
//! skill set, the **subagents** it may delegate to (a subagent runs ⊆ its parent's
//! authority), the **channels** it listens on (an inbound message routes to it),
//! and the §19 **grant** that is its authority. The name is the per-workspace key
//! and the path key for update/delete, so the editor disables it once a profile
//! exists (a rename would create a second profile). Managing profiles is admin-only
//! server-side (like grants), so a non-admin sees a load error.
//!
//! The editor is **picker-first**: the model is a dropdown of the gateway's chat
//! models (`/llm-models`), tools and skills are checklists (`/tools`, `/skills`),
//! subagents a checklist of the *other* profiles, channels a chip input, and the
//! grant a name dropdown (`/grants`). Each catalog **degrades gracefully** — if its
//! fetch fails (e.g. the gateway is down, or a non-admin can't read grants) the
//! field falls back to a plain text input so the panel never breaks.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::widgets::{
    checklist, chip_input, list_drawer_scrim, list_drawer_toggle, model_autocomplete,
    model_options, with_out_of_catalog,
};
use crate::api::{
    AgentProfile, CreateAgentProfile, GuardFail, ModelInfo, ObjectLabelPolicy, Skill, ToolGuard,
    ToolGuardLlm, ToolInfo, UpdateAgentProfile,
};
use crate::auth;
use crate::rest;

/// The Agent Profiles panel component.
#[component]
pub fn ProfilesPanel() -> impl IntoView {
    // Loaded list + load state.
    let profiles = RwSignal::new(Vec::<AgentProfile>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Picker catalogs, each loaded once and independently. A `*_failed` flag flips
    // the matching field to a plain-text fallback when its fetch errors.
    let models = RwSignal::new(Vec::<ModelInfo>::new());
    let default_model = RwSignal::new(String::new());
    let tools_catalog = RwSignal::new(Vec::<ToolInfo>::new());
    let tools_failed = RwSignal::new(false);
    let skills_catalog = RwSignal::new(Vec::<Skill>::new());
    // Grants as (name, id): the dropdown shows the name, stores the id.
    let grants_catalog = RwSignal::new(Vec::<(String, String)>::new());
    let grants_failed = RwSignal::new(false);

    // Editor state. `selected_name` is the profile being edited (None + `is_new` =
    // an unsaved draft; None + !`is_new` = nothing open).
    let selected_name = RwSignal::new(Option::<String>::None);
    let is_new = RwSignal::new(false);
    let edit_name = RwSignal::new(String::new());
    let edit_model = RwSignal::new(String::new());
    let edit_system = RwSignal::new(String::new());
    // The multi-value fields are now selection sets (not comma strings).
    let edit_tools = RwSignal::new(Vec::<String>::new());
    let edit_skills = RwSignal::new(Vec::<String>::new());
    let edit_subagents = RwSignal::new(Vec::<String>::new());
    let edit_channels = RwSignal::new(Vec::<String>::new());
    let edit_grant = RwSignal::new(String::new());
    // Tool guard (SOUL §19): the JS classifier, the declarative LLM classifier, and
    // the fail-mode. Blank fields collapse to "no guard" on save (server-normalized).
    let edit_guard_script = RwSignal::new(String::new());
    let edit_guard_instruction = RwSignal::new(String::new());
    let edit_guard_model = RwSignal::new(String::new());
    let edit_guard_on_error = RwSignal::new("deny".to_string());
    // Object-label allow/deny policy (SOUL §9): require-any + deny label sets.
    let edit_guard_require_labels = RwSignal::new(Vec::<String>::new());
    let edit_guard_deny_labels = RwSignal::new(Vec::<String>::new());
    let require_label_draft = RwSignal::new(String::new());
    let deny_label_draft = RwSignal::new(String::new());
    // Draft inputs for the chip/free-text fields.
    let channel_draft = RwSignal::new(String::new());
    let tool_draft = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // Clear every editor field (shared by "new" and post-delete).
    let clear_editor = move || {
        edit_name.set(String::new());
        edit_model.set(String::new());
        edit_system.set(String::new());
        edit_tools.set(Vec::new());
        edit_skills.set(Vec::new());
        edit_subagents.set(Vec::new());
        edit_channels.set(Vec::new());
        edit_grant.set(String::new());
        edit_guard_script.set(String::new());
        edit_guard_instruction.set(String::new());
        edit_guard_model.set(String::new());
        edit_guard_on_error.set("deny".to_string());
        edit_guard_require_labels.set(Vec::new());
        edit_guard_deny_labels.set(Vec::new());
        require_label_draft.set(String::new());
        deny_label_draft.set(String::new());
        channel_draft.set(String::new());
        tool_draft.set(String::new());
    };

    // Load a profile's fields into the editor signals.
    let load_into_editor = move |p: &AgentProfile| {
        selected_name.set(Some(p.name.clone()));
        is_new.set(false);
        edit_name.set(p.name.clone());
        edit_model.set(p.model.clone().unwrap_or_default());
        edit_system.set(p.system_prompt.clone().unwrap_or_default());
        edit_tools.set(p.tools.clone());
        edit_skills.set(p.skills.clone());
        edit_subagents.set(p.subagents.clone());
        edit_channels.set(p.channels.clone());
        edit_grant.set(p.grant_id.clone().unwrap_or_default());
        // Guard fields (empty when the profile carries no guard).
        let g = p.guard.clone().unwrap_or_default();
        edit_guard_script.set(g.script.unwrap_or_default());
        let llm = g.llm.unwrap_or_default();
        edit_guard_instruction.set(llm.instruction);
        edit_guard_model.set(llm.model.unwrap_or_default());
        let labels = g.object_labels.unwrap_or_default();
        edit_guard_require_labels.set(labels.require_any);
        edit_guard_deny_labels.set(labels.deny);
        edit_guard_on_error.set(guard_fail_str(g.on_error).to_string());
        require_label_draft.set(String::new());
        deny_label_draft.set(String::new());
        channel_draft.set(String::new());
        tool_draft.set(String::new());
        save_error.set(None);
    };

    // Fetch the profile list. When `auto_select` and nothing is being edited, open
    // the first profile so the editor isn't empty on first paint.
    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_agent_profiles(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_name.get_untracked().is_none()
                    {
                        if let Some(first) = list.first() {
                            load_into_editor(first);
                        }
                    }
                    profiles.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    profiles.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Load the picker catalogs (model / tools / skills / grants + the default-model
    // label). Each step is best-effort; a failure flips the field to its fallback.
    let load_catalogs = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            if let Ok(s) = rest::get_status(tok).await {
                default_model.set(s.llm.default_model);
            }
            // Best-effort: an empty or failed catalog just leaves the model
            // autocomplete with no suggestions; it still takes a typed id.
            if let Ok(m) = rest::list_llm_models(tok, "llm").await {
                models.set(m);
            }
            match rest::list_tools(tok).await {
                Ok(t) => {
                    tools_catalog.set(t);
                    tools_failed.set(false);
                }
                Err(_) => tools_failed.set(true),
            }
            if let Ok(sk) = rest::list_skills(tok).await {
                skills_catalog.set(sk);
            }
            match rest::list_grants(tok).await {
                Ok(g) => {
                    grants_catalog.set(g.into_iter().map(|x| (x.name, x.id)).collect());
                    grants_failed.set(false);
                }
                Err(_) => grants_failed.set(true),
            }
        });
    };

    // Initial load.
    refresh(true);
    load_catalogs();

    // Begin a new, unsaved profile (clears the editor).
    let start_new = move || {
        selected_name.set(None);
        is_new.set(true);
        clear_editor();
        save_error.set(None);
    };

    // Save the editor: create a new profile or replace the open one.
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        // The name is the key: from the editor when creating, else the open one.
        let new = is_new.get_untracked();
        let name = if new {
            edit_name.get_untracked().trim().to_string()
        } else {
            selected_name.get_untracked().unwrap_or_default()
        };
        if name.is_empty() {
            save_error.set(Some("Give the profile a name.".to_string()));
            return;
        }
        let model = opt(&edit_model.get_untracked());
        let system_prompt = opt(&edit_system.get_untracked());
        // The pickers keep these clean (deduped, trimmed) by construction.
        let tools = edit_tools.get_untracked();
        let skills = edit_skills.get_untracked();
        let subagents = edit_subagents.get_untracked();
        let channels = edit_channels.get_untracked();
        let grant_id = opt(&edit_grant.get_untracked());
        let guard = build_guard(
            edit_guard_script.get_untracked(),
            edit_guard_instruction.get_untracked(),
            edit_guard_model.get_untracked(),
            edit_guard_require_labels.get_untracked(),
            edit_guard_deny_labels.get_untracked(),
            edit_guard_on_error.get_untracked(),
        );

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            let result: Result<AgentProfile, rest::RestError> = if new {
                rest::create_agent_profile(
                    tok,
                    &CreateAgentProfile {
                        name,
                        model,
                        system_prompt,
                        tools,
                        skills,
                        subagents,
                        channels,
                        grant_id,
                        guard,
                    },
                )
                .await
            } else {
                rest::update_agent_profile(
                    tok,
                    &name,
                    &UpdateAgentProfile {
                        model,
                        system_prompt,
                        tools,
                        skills,
                        subagents,
                        channels,
                        grant_id,
                        guard,
                    },
                )
                .await
            };
            saving.set(false);
            match result {
                Ok(p) => {
                    load_into_editor(&p);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    // Delete the open profile (by name).
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
            match rest::delete_agent_profile(token.as_deref(), &name).await {
                Ok(()) => {
                    saving.set(false);
                    selected_name.set(None);
                    is_new.set(false);
                    clear_editor();
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
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // Whether the list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Profiles"</h2>
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
                                    "Could not load profiles: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && profiles.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No profiles yet. Create one →"</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || profiles.get()
                            key=|p| p.name.clone()
                            children=move |p: AgentProfile| {
                                let name = p.name.clone();
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
                                let label = p.name.clone();
                                let model = p.model.clone().unwrap_or_default();
                                let has_channels = !p.channels.is_empty();
                                let profile_for_click = p.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            on:click=move |_| {
                                                load_into_editor(&profile_for_click);
                                                list_open.set(false);
                                            }
                                        >
                                            <span class="pane-item-title">
                                                {label}
                                                <Show
                                                    when=move || has_channels
                                                    fallback=|| ().into_view()
                                                >
                                                    <span class="skills-tag">"channel"</span>
                                                </Show>
                                            </span>
                                            <Show
                                                when={
                                                    let has = !model.is_empty();
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="pane-item-preview">
                                                    {model.clone()}
                                                </span>
                                            </Show>
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            {list_drawer_toggle("Profiles", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select a profile, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <form class="skills-form" on:submit=on_save_submit>
                        <div class="pf-group-title">"Identity"</div>
                        <div class="pf-field">
                            <span class="pf-label">"Name"</span>
                            <span class="pf-help">
                                "The profile's unique name. Locked once it exists."
                            </span>
                            <input
                                class="skills-input skills-input-name"
                                placeholder="e.g. calendar-bot"
                                disabled=move || saving.get() || !is_new.get()
                                prop:value=move || edit_name.get()
                                on:input=move |ev| edit_name.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"Model"</span>
                            <span class="pf-help">"Which model this agent thinks with."</span>
                            {model_autocomplete(
                                Signal::derive(move || edit_model.get()),
                                move |v| edit_model.set(v),
                                model_options(models, false),
                                Signal::derive(move || {
                                    let d = default_model.get();
                                    if d.is_empty() {
                                        "Workspace default".to_string()
                                    } else {
                                        format!("Workspace default ({d})")
                                    }
                                }),
                                Signal::derive(move || saving.get()),
                                "skills-input",
                            )}
                        </div>

                        <div class="pf-group-title">"Behaviour"</div>
                        <div class="pf-field">
                            <span class="pf-label">"System prompt"</span>
                            <span class="pf-help">
                                "Standing instructions. Blank uses the default agent prompt."
                            </span>
                            <textarea
                                class="skills-textarea"
                                placeholder="You are a helpful assistant that…"
                                disabled=move || saving.get()
                                prop:value=move || edit_system.get()
                                on:input=move |ev| edit_system.set(event_target_value(&ev))
                            ></textarea>
                        </div>

                        <div class="pf-group-title">"Capabilities"</div>
                        <div class="pf-field">
                            <span class="pf-label">"Tools"</span>
                            <span class="pf-help">
                                "What the agent can do. Tick none to allow every tool."
                            </span>
                            {move || {
                                // Re-render on profile switch so the out-of-catalog rows
                                // reflect the open profile (the catalog itself is stable).
                                let _ = (selected_name.get(), is_new.get());
                                if tools_failed.get() {
                                    chip_input(
                                            edit_tools,
                                            tool_draft,
                                            "Tool name, then press Enter",
                                            saving,
                                        )
                                        .into_any()
                                } else {
                                    // Fold in any selected tool no longer in the registry (a
                                    // renamed/removed tool) so it stays visible and removable.
                                    let cat: Vec<(String, String, Option<String>)> = tools_catalog
                                        .get()
                                        .into_iter()
                                        .map(|t| {
                                            let hint = (!t.description.is_empty()).then_some(t.description);
                                            (t.name.clone(), t.name, hint)
                                        })
                                        .collect();
                                    let items = with_out_of_catalog(
                                        cat,
                                        &edit_tools.get_untracked(),
                                        "not in catalog",
                                    );
                                    if items.is_empty() {
                                        view! { <div class="pf-empty">"No tools available."</div> }
                                            .into_any()
                                    } else {
                                        checklist(items, edit_tools, saving).into_any()
                                    }
                                }
                            }}
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"Skills"</span>
                            <span class="pf-help">
                                "Reusable runbooks mixed into the agent's prompt."
                            </span>
                            {move || {
                                let _ = (selected_name.get(), is_new.get());
                                // Fold in any selected skill that no longer exists so a
                                // profile referencing a deleted skill still shows it.
                                let cat: Vec<(String, String, Option<String>)> = skills_catalog
                                    .get()
                                    .into_iter()
                                    .map(|s| (s.name.clone(), s.name, None))
                                    .collect();
                                let items = with_out_of_catalog(
                                    cat,
                                    &edit_skills.get_untracked(),
                                    "not in catalog",
                                );
                                if items.is_empty() {
                                    view! {
                                        <div class="pf-empty">
                                            "No skills yet — create some in the Skills panel."
                                        </div>
                                    }
                                        .into_any()
                                } else {
                                    checklist(items, edit_skills, saving).into_any()
                                }
                            }}
                        </div>
                        <div class="pf-group-title">"Delegation & routing"</div>
                        <div class="pf-field">
                            <span class="pf-label">"Subagents"</span>
                            <span class="pf-help">
                                "Other profiles this one may hand work to (each runs within this profile's authority)."
                            </span>
                            {move || {
                                let current = edit_name.get();
                                let others: Vec<(String, String, Option<String>)> = profiles
                                    .get()
                                    .into_iter()
                                    .map(|p| p.name)
                                    .filter(|n| n != &current)
                                    .map(|n| (n.clone(), n, None))
                                    .collect();
                                // Fold in any selected subagent that is no longer a profile
                                // (deleted, or renamed) so it stays visible and removable.
                                let items = with_out_of_catalog(
                                    others,
                                    &edit_subagents.get_untracked(),
                                    "not in catalog",
                                );
                                if items.is_empty() {
                                    view! {
                                        <div class="pf-empty">"No other profiles to delegate to."</div>
                                    }
                                        .into_any()
                                } else {
                                    checklist(items, edit_subagents, saving).into_any()
                                }
                            }}
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"Channels"</span>
                            <span class="pf-help">
                                "Inbound channels this agent listens on. Type a name and press Enter."
                            </span>
                            {chip_input(
                                edit_channels,
                                channel_draft,
                                "Channel name, then press Enter",
                                saving,
                            )}
                        </div>

                        <div class="pf-group-title">"Authority"</div>
                        <div class="pf-field">
                            <span class="pf-label">"Grant"</span>
                            <span class="pf-help">
                                "The capability grant that bounds what this agent may touch."
                            </span>
                            {move || {
                                if grants_failed.get() {
                                    view! {
                                        <input
                                            class="skills-input"
                                            placeholder="Grant id (optional UUID = its authority)"
                                            disabled=move || saving.get()
                                            prop:value=move || edit_grant.get()
                                            on:input=move |ev| edit_grant.set(event_target_value(&ev))
                                        />
                                    }
                                        .into_any()
                                } else {
                                    let gs = grants_catalog.get();
                                    view! {
                                        <select
                                            class="skills-input pf-select"
                                            disabled=move || saving.get()
                                            prop:value=move || edit_grant.get()
                                            on:change=move |ev| {
                                                edit_grant.set(event_target_value(&ev))
                                            }
                                        >
                                            <option value="">"None (base Member capabilities)"</option>
                                            {gs
                                                .into_iter()
                                                .map(|(name, id)| {
                                                    view! { <option value=id>{name}</option> }
                                                })
                                                .collect::<Vec<_>>()}
                                        </select>
                                    }
                                        .into_any()
                                }
                            }}
                        </div>

                        <div class="pf-group-title">"Tool guard"</div>
                        <div class="pf-field">
                            <span class="pf-label">"Classifier (JavaScript)"</span>
                            <span class="pf-help">
                                "Optional. A function body that classifies every tool call on top of the grant. \
                                 It receives `input` = { phase, tool:{name,description}, capability:{domain,action,read_only}, \
                                 mcp:{server}|null, args, output } and returns 'allow' | 'deny' | 'ask' (or { decision, reason }). \
                                 May call catalerum.callTool(name,args) and catalerum.classifyWithLlm({instruction}). \
                                 e.g. limit an MCP server to read-only, or require approval for writes."
                            </span>
                            <textarea
                                class="skills-textarea"
                                style="font-family: var(--mono, monospace);"
                                placeholder="if (input.mcp && !input.capability.read_only) return 'deny'; return 'allow';"
                                disabled=move || saving.get()
                                prop:value=move || edit_guard_script.get()
                                on:input=move |ev| edit_guard_script.set(event_target_value(&ev))
                            ></textarea>
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"LLM classifier — instruction"</span>
                            <span class="pf-help">
                                "Optional. When set (and no script decides), an LLM judges each call by this policy \
                                 and returns allow / deny / require-user-feedback. Also the default for classifyWithLlm."
                            </span>
                            <textarea
                                class="skills-textarea"
                                placeholder="Deny any write to a production resource; allow reads."
                                disabled=move || saving.get()
                                prop:value=move || edit_guard_instruction.get()
                                on:input=move |ev| edit_guard_instruction.set(event_target_value(&ev))
                            ></textarea>
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"LLM classifier — model"</span>
                            <span class="pf-help">
                                "Model the LLM classifier judges with. Blank uses the profile's model."
                            </span>
                            <input
                                class="skills-input"
                                placeholder="e.g. anthropic/claude-haiku-4-5 (optional)"
                                disabled=move || saving.get()
                                prop:value=move || edit_guard_model.get()
                                on:input=move |ev| edit_guard_model.set(event_target_value(&ev))
                            />
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"Object labels — require any"</span>
                            <span class="pf-help">
                                "Optional. Only allow a tool call touching a file (SOUL §9) if that file carries at least one of these labels — unlabelled files are denied too. Type a label and press Enter."
                            </span>
                            {chip_input(
                                edit_guard_require_labels,
                                require_label_draft,
                                "Label, then press Enter",
                                saving,
                            )}
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"Object labels — deny"</span>
                            <span class="pf-help">
                                "Optional. Block any tool call touching a file that carries one of these labels (this wins over require-any). Type a label and press Enter."
                            </span>
                            {chip_input(
                                edit_guard_deny_labels,
                                deny_label_draft,
                                "Label, then press Enter",
                                saving,
                            )}
                        </div>
                        <div class="pf-field">
                            <span class="pf-label">"On classifier error"</span>
                            <span class="pf-help">
                                "What to do when the classifier errors or can't decide. Default: deny (fail closed)."
                            </span>
                            <select
                                class="skills-input pf-select"
                                disabled=move || saving.get()
                                prop:value=move || edit_guard_on_error.get()
                                on:change=move |ev| edit_guard_on_error.set(event_target_value(&ev))
                            >
                                <option value="deny">"Deny (fail closed)"</option>
                                <option value="allow">"Allow (fail open)"</option>
                                <option value="ask">"Ask the user"</option>
                            </select>
                        </div>

                        <Show
                            when=move || save_error.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <div class="skills-form-error">
                                {move || save_error.get().unwrap_or_default()}
                            </div>
                        </Show>

                        <div class="skills-form-actions">
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
                    </form>
                </Show>
            </div>
        </section>
    }
}

/// A trimmed optional field: blank → `None` (omitted from the request, read as
/// "unset" server-side), matching the API's `clean_opt`.
fn opt(raw: &str) -> Option<String> {
    let t = raw.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Assemble a [`ToolGuard`] from the editor fields (SOUL §19): a blank script and a
/// blank instruction each drop out, and a guard left with neither collapses to
/// `None` (no guard). The server re-normalizes + validates on save.
#[allow(clippy::too_many_arguments)]
fn build_guard(
    script: String,
    instruction: String,
    model: String,
    require_labels: Vec<String>,
    deny_labels: Vec<String>,
    on_error: String,
) -> Option<ToolGuard> {
    let script = opt(&script);
    let instruction = instruction.trim().to_string();
    let llm = (!instruction.is_empty()).then(|| ToolGuardLlm {
        model: opt(&model),
        instruction,
    });
    // The chip inputs keep these clean (trimmed, deduped); drop an all-empty policy.
    let object_labels = if require_labels.is_empty() && deny_labels.is_empty() {
        None
    } else {
        Some(ObjectLabelPolicy {
            require_any: require_labels,
            deny: deny_labels,
        })
    };
    if script.is_none() && llm.is_none() && object_labels.is_none() {
        return None;
    }
    let on_error = match on_error.as_str() {
        "allow" => GuardFail::Allow,
        "ask" => GuardFail::Ask,
        _ => GuardFail::Deny,
    };
    Some(ToolGuard {
        script,
        llm,
        object_labels,
        on_error,
    })
}

/// The lowercase token for a [`GuardFail`] (for the editor's select value).
fn guard_fail_str(f: GuardFail) -> &'static str {
    match f {
        GuardFail::Deny => "deny",
        GuardFail::Allow => "allow",
        GuardFail::Ask => "ask",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_blanks_to_none() {
        assert_eq!(opt("   "), None);
        assert_eq!(opt(""), None);
        assert_eq!(
            opt("  anthropic/claude "),
            Some("anthropic/claude".to_string())
        );
    }
}
