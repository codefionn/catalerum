//! The Skills panel (SOUL §23, §12 — skills manager).
//!
//! A two-pane workbench panel: a searchable left list of the workspace's skills
//! and a right pane that **reads** before it **writes** — selecting a skill shows
//! a rendered card (its instructions as formatted Markdown, its tools as chips, an
//! attached code block with a language badge), and an "Edit" toggles into a guided
//! form. It is a thin client of the skills REST surface (`/skills`,
//! `/skills/{name}`) — every call carries the dev session token and is
//! workspace-scoped server-side (SOUL §18).
//!
//! A skill is keyed by its per-workspace-unique `name`, the path key for
//! update/delete (`PUT`/`DELETE /skills/{name}`). The name is therefore fixed once
//! a skill exists — the form disables it when editing (a rename would create a
//! second skill, not move this one); create a new skill to use a new name.
//!
//! The form reuses the shared workbench widgets so authoring a skill feels like
//! authoring an agent profile: the tool set is a checklist against the live tool
//! registry (`GET /tools`) instead of a comma-separated free-text box, and the
//! instructions use the same Markdown toolbar + live preview as Notes
//! ([`super::md_editor::MarkdownField`]). The optional code block (language +
//! source + entrypoint) maps to the core `Code`: a blank language clears it.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::dialogs::{use_dialogs, ConfirmSpec};
use super::markdown::markdown_html;
use super::md_editor::MarkdownField;
use super::widgets::{
    checklist, chip_input, list_drawer_scrim, list_drawer_toggle, with_out_of_catalog,
};
use crate::api::{Code, CreateSkill, Skill, ToolInfo, UpdateSkill};
use crate::auth;
use crate::rest;

/// The Skills panel component.
#[component]
pub fn SkillsPanel() -> impl IntoView {
    // The shared confirm dialog (replaces the native discard-changes confirm).
    let dialogs = use_dialogs();
    // Loaded list + load state.
    let skills = RwSignal::new(Vec::<Skill>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);
    // Free-text search over the list (name / description / instructions / tools).
    let skill_query = RwSignal::new(String::new());

    // The tool registry that backs the tools checklist (`GET /tools`). If it fails
    // to load, the form falls back to a free-text chip input so authoring still
    // works offline.
    let tools_catalog = RwSignal::new(Vec::<ToolInfo>::new());
    let tools_failed = RwSignal::new(false);

    // Right-pane state. `selected_name` is the open skill (None + `is_new` = an
    // unsaved draft; None + !`is_new` = nothing open). `editing` distinguishes the
    // rendered read view (false) from the authoring form (true). `current_skill`
    // holds the loaded skill so the read view renders it and Cancel can revert.
    let selected_name = RwSignal::new(Option::<String>::None);
    let is_new = RwSignal::new(false);
    let editing = RwSignal::new(false);
    let current_skill = RwSignal::new(Option::<Skill>::None);

    let edit_name = RwSignal::new(String::new());
    let edit_description = RwSignal::new(String::new());
    let edit_tools = RwSignal::new(Vec::<String>::new());
    let tool_draft = RwSignal::new(String::new());
    let edit_instructions = RwSignal::new(String::new());
    let edit_advertised = RwSignal::new(true);
    let edit_code_lang = RwSignal::new(String::new());
    let edit_code_source = RwSignal::new(String::new());
    let edit_code_entrypoint = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // Blank every editor field — a fresh draft or a cleared editor.
    let clear_editor = move || {
        edit_name.set(String::new());
        edit_description.set(String::new());
        edit_tools.set(Vec::new());
        tool_draft.set(String::new());
        edit_instructions.set(String::new());
        edit_advertised.set(true);
        edit_code_lang.set(String::new());
        edit_code_source.set(String::new());
        edit_code_entrypoint.set(String::new());
    };

    // Load a skill's fields into the editor signals (the form's starting point and
    // what Cancel reverts to).
    let populate_editor = move |skill: &Skill| {
        edit_name.set(skill.name.clone());
        edit_description.set(skill.description.clone());
        edit_tools.set(skill.tools.clone());
        tool_draft.set(String::new());
        edit_instructions.set(skill.instructions_md.clone());
        edit_advertised.set(skill.advertised);
        match &skill.code {
            Some(c) => {
                edit_code_lang.set(c.language.clone());
                edit_code_source.set(c.source.clone());
                edit_code_entrypoint.set(c.entrypoint.clone().unwrap_or_default());
            }
            None => {
                edit_code_lang.set(String::new());
                edit_code_source.set(String::new());
                edit_code_entrypoint.set(String::new());
            }
        }
    };

    // Open a skill in the rendered read view.
    let load_into_view = move |skill: &Skill| {
        selected_name.set(Some(skill.name.clone()));
        is_new.set(false);
        editing.set(false);
        current_skill.set(Some(skill.clone()));
        populate_editor(skill);
        save_error.set(None);
    };

    // Fetch the skills list. When `auto_select` and nothing is open, open the first
    // skill so the pane isn't empty on first paint.
    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_skills(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_name.get_untracked().is_none()
                    {
                        if let Some(first) = list.first() {
                            load_into_view(first);
                        }
                    }
                    skills.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    skills.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Initial loads: the list, and the tool registry for the checklist.
    refresh(true);
    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::list_tools(token.as_deref()).await {
            Ok(t) => {
                tools_catalog.set(t);
                tools_failed.set(false);
            }
            Err(_) => tools_failed.set(true),
        }
    });

    // Whether the form differs from the open skill (or, for a new draft, from
    // empty) — i.e. there is unsaved work that a reset would throw away.
    let editor_is_dirty = move || {
        let (b_name, b_desc, b_tools, b_instr, b_advertised, b_lang, b_source, b_entry) =
            match current_skill.get_untracked() {
                Some(s) => {
                    let (lang, source, entry) = match s.code {
                        Some(c) => (c.language, c.source, c.entrypoint.unwrap_or_default()),
                        None => (String::new(), String::new(), String::new()),
                    };
                    (
                        s.name,
                        s.description,
                        s.tools,
                        s.instructions_md,
                        s.advertised,
                        lang,
                        source,
                        entry,
                    )
                }
                None => (
                    String::new(),
                    String::new(),
                    Vec::new(),
                    String::new(),
                    true,
                    String::new(),
                    String::new(),
                    String::new(),
                ),
            };
        // Compare the raw code fields rather than build_code's result: a typed
        // source with a blank language is still unsaved work (save() refuses to
        // silently drop it), so navigating away must prompt.
        edit_name.get_untracked().trim() != b_name
            || edit_description.get_untracked().trim() != b_desc
            || clean_tools(edit_tools.get_untracked()) != b_tools
            || edit_instructions.get_untracked() != b_instr
            || edit_advertised.get_untracked() != b_advertised
            || edit_code_lang.get_untracked() != b_lang
            || edit_code_source.get_untracked() != b_source
            || edit_code_entrypoint.get_untracked() != b_entry
    };

    // Gate a destructive editor reset (selecting another skill, or "New") behind a
    // discard confirmation. `proceed` runs immediately when it's safe (nothing to
    // lose), or after the user confirms the discard. Cancel and Save have their
    // own intent, so they don't go through this.
    let guard_discard = move |proceed: Box<dyn Fn()>| {
        if !editing.get_untracked() || !editor_is_dirty() {
            proceed();
        } else {
            dialogs.confirm(
                ConfirmSpec::danger(
                    "Discard changes?",
                    "Discard unsaved changes to this skill?",
                    "Discard",
                ),
                proceed,
            );
        }
    };

    // Begin a new, unsaved skill (a blank form).
    let start_new = move || {
        guard_discard(Box::new(move || {
            selected_name.set(None);
            is_new.set(true);
            editing.set(true);
            current_skill.set(None);
            clear_editor();
            save_error.set(None);
        }));
    };

    // Switch the open skill from read view into the authoring form.
    let start_edit = move || {
        editing.set(true);
        save_error.set(None);
    };

    // Leave the form: discard a new draft, or revert an existing skill's fields.
    let cancel_edit = move || {
        if is_new.get_untracked() {
            selected_name.set(None);
            is_new.set(false);
            editing.set(false);
            current_skill.set(None);
            clear_editor();
        } else {
            if let Some(skill) = current_skill.get_untracked() {
                populate_editor(&skill);
            }
            editing.set(false);
        }
        save_error.set(None);
    };

    // Save the form: create a new skill or replace the open one.
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        save_error.set(None);
        let new = is_new.get_untracked();
        // The name is the key: from the form when creating, else the open one.
        let name = if new {
            edit_name.get_untracked().trim().to_string()
        } else {
            selected_name.get_untracked().unwrap_or_default()
        };
        if name.is_empty() {
            save_error.set(Some("Give the skill a name.".to_string()));
            return;
        }
        let description = edit_description.get_untracked().trim().to_string();
        let instructions_md = edit_instructions.get_untracked();
        let tools = clean_tools(edit_tools.get_untracked());
        let advertised = edit_advertised.get_untracked();
        let code = build_code(
            &edit_code_lang.get_untracked(),
            &edit_code_source.get_untracked(),
            &edit_code_entrypoint.get_untracked(),
        );
        // A blank language drops the code (the API's clear-semantics). Don't do
        // that silently when the user has typed a source/entrypoint — losing
        // code is costly. Ask for a language instead of discarding their work.
        if code.is_none()
            && (!edit_code_source.get_untracked().trim().is_empty()
                || !edit_code_entrypoint.get_untracked().trim().is_empty())
        {
            save_error.set(Some(
                "Pick a language for the attached code, or clear the source to remove it."
                    .to_string(),
            ));
            return;
        }

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            let result: Result<Skill, rest::RestError> = if new {
                rest::create_skill(
                    tok,
                    &CreateSkill {
                        name,
                        description,
                        instructions_md,
                        tools,
                        code,
                        advertised,
                    },
                )
                .await
            } else {
                rest::update_skill(
                    tok,
                    &name,
                    &UpdateSkill {
                        description,
                        instructions_md,
                        tools,
                        code,
                        advertised,
                    },
                )
                .await
            };
            saving.set(false);
            match result {
                Ok(skill) => {
                    load_into_view(&skill);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    // Delete the open skill (by name).
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
            match rest::delete_skill(token.as_deref(), &name).await {
                Ok(()) => {
                    saving.set(false);
                    selected_name.set(None);
                    is_new.set(false);
                    editing.set(false);
                    current_skill.set(None);
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

    // Something is open on the right (a skill in view/edit, or a new draft).
    let editor_open = move || selected_name.get().is_some() || is_new.get();
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // The list after the free-text filter.
    let visible_skills = move || {
        skills.with(|list| {
            skill_query.with(|q| {
                let q = q.trim();
                if q.is_empty() {
                    list.clone()
                } else {
                    list.iter()
                        .filter(|s| skill_matches_query(s, q))
                        .cloned()
                        .collect()
                }
            })
        })
    };

    // Read-view projections off the open skill (reactive: re-render on selection).
    let view_name =
        move || current_skill.with(|s| s.as_ref().map(|s| s.name.clone()).unwrap_or_default());
    let view_description = move || {
        current_skill.with(|s| {
            s.as_ref()
                .map(|s| s.description.clone())
                .unwrap_or_default()
        })
    };
    let view_tools =
        move || current_skill.with(|s| s.as_ref().map(|s| s.tools.clone()).unwrap_or_default());
    let view_instructions = move || {
        current_skill.with(|s| {
            s.as_ref()
                .map(|s| s.instructions_md.clone())
                .unwrap_or_default()
        })
    };
    let view_code = move || current_skill.with(|s| s.as_ref().and_then(|s| s.code.clone()));
    let view_advertised =
        move || current_skill.with(|s| s.as_ref().map(|s| s.advertised).unwrap_or(true));

    // Whether the list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Skills"</h2>
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
                    <Show
                        when=move || !skills.with(Vec::is_empty)
                        fallback=|| ().into_view()
                    >
                        <input
                            class="pane-search"
                            placeholder="Search skills…"
                            prop:value=move || skill_query.get()
                            on:input=move |ev| skill_query.set(event_target_value(&ev))
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
                                    "Could not load skills: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && skills.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No skills yet. Create one →"</div>
                    </Show>

                    <Show
                        when=move || !skills.with(Vec::is_empty) && visible_skills().is_empty()
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No skills match."</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || visible_skills()
                            // A tuple key (injective, unlike a delimiter-joined
                            // string) that still folds in the row's displayed
                            // fields so a save refreshes the row's text/badges.
                            key=|s| {
                                (
                                    s.name.clone(),
                                    s.description.clone(),
                                    s.tools.len(),
                                    s.code.is_some(),
                                )
                            }
                            children=move |s: Skill| {
                                let name = s.name.clone();
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
                                let label = s.name.clone();
                                let desc = s.description.clone();
                                let has_code = s.code.is_some();
                                let tool_count = s.tools.len();
                                let name_for_click = s.name.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            // Resolve the freshest copy from the list at
                                            // click time rather than a clone captured at
                                            // render — the row key may not have rotated on
                                            // an instructions/code-only edit, so the
                                            // captured value could be stale.
                                            on:click=move |_| {
                                                let name_for_click = name_for_click.clone();
                                                guard_discard(Box::new(move || {
                                                    if let Some(sk) = skills
                                                        .get_untracked()
                                                        .into_iter()
                                                        .find(|x| x.name == name_for_click)
                                                    {
                                                        load_into_view(&sk);
                                                        list_open.set(false);
                                                    }
                                                }));
                                            }
                                        >
                                            <span class="pane-item-title">
                                                {label}
                                                <Show
                                                    when=move || has_code
                                                    fallback=|| ().into_view()
                                                >
                                                    <span class="skills-tag">"code"</span>
                                                </Show>
                                            </span>
                                            <Show
                                                when={
                                                    let has = !desc.is_empty();
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="pane-item-preview">
                                                    {desc.clone()}
                                                </span>
                                            </Show>
                                            <Show
                                                when={
                                                    let has = tool_count > 0;
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="pane-item-meta">
                                                    {format!(
                                                        "{tool_count} tool{}",
                                                        if tool_count == 1 { "" } else { "s" },
                                                    )}
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

            {list_drawer_toggle("Skills", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select a skill, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <Show
                        when=move || editing.get()
                        fallback=move || {
                            view! {
                                <div class="skills-view">
                                    <header class="skills-view-header">
                                        <h2 class="skills-view-name">{view_name}</h2>
                                        <div class="skills-form-actions">
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
                                        <p class="skills-view-desc">{view_description}</p>
                                    </Show>

                                    <Show when=move || !view_advertised() fallback=|| ().into_view()>
                                        <p class="skills-view-muted">
                                            "Hidden from the agent — not advertised in the chat system prompt (still invokable by name)."
                                        </p>
                                    </Show>

                                    <section class="skills-section">
                                        <div class="skills-section-label">"Tools"</div>
                                        {move || {
                                            let tools = view_tools();
                                            if tools.is_empty() {
                                                view! {
                                                    <p class="skills-view-muted">
                                                        "All tools (unrestricted)."
                                                    </p>
                                                }
                                                    .into_any()
                                            } else {
                                                view! {
                                                    <div class="skills-view-chips">
                                                        {tools
                                                            .into_iter()
                                                            .map(|t| {
                                                                view! { <span class="skills-chip">{t}</span> }
                                                            })
                                                            .collect::<Vec<_>>()}
                                                    </div>
                                                }
                                                    .into_any()
                                            }
                                        }}
                                    </section>

                                    <section class="skills-section">
                                        <div class="skills-section-label">"Instructions"</div>
                                        {move || {
                                            let md = view_instructions();
                                            if md.trim().is_empty() {
                                                view! {
                                                    <p class="skills-view-muted">
                                                        "No instructions yet."
                                                    </p>
                                                }
                                                    .into_any()
                                            } else {
                                                view! {
                                                    <div
                                                        class="notes-preview skills-view-md"
                                                        inner_html=markdown_html(&md)
                                                    ></div>
                                                }
                                                    .into_any()
                                            }
                                        }}
                                    </section>

                                    {move || {
                                        match view_code() {
                                            None => ().into_any(),
                                            Some(code) => {
                                                let entry = code.entrypoint.clone();
                                                view! {
                                                    <section class="skills-section">
                                                        <div class="skills-section-label">
                                                            "Code"
                                                            <span class="skills-code-badge">
                                                                {code.language.clone()}
                                                            </span>
                                                            {entry
                                                                .map(|e| {
                                                                    view! {
                                                                        <span class="skills-code-entry">
                                                                            {format!("→ {e}")}
                                                                        </span>
                                                                    }
                                                                })}
                                                        </div>
                                                        <pre class="skills-codeblock"><code>{code.source.clone()}</code></pre>
                                                    </section>
                                                }
                                                    .into_any()
                                            }
                                        }
                                    }}
                                </div>
                            }
                        }
                    >
                        <form class="skills-form" on:submit=on_save_submit>
                            <div class="pf-group-title">"Basics"</div>
                            <div class="pf-field">
                                <span class="pf-label">"Name"</span>
                                <span class="pf-help">
                                    "How the skill is invoked. Fixed once the skill exists."
                                </span>
                                <input
                                    class="skills-input skills-input-name"
                                    placeholder="e.g. triage-inbox"
                                    // The name is the key: editable only when creating.
                                    disabled=move || saving.get() || !is_new.get()
                                    prop:value=move || edit_name.get()
                                    on:input=move |ev| edit_name.set(event_target_value(&ev))
                                />
                            </div>
                            <div class="pf-field">
                                <span class="pf-label">"Description"</span>
                                <span class="pf-help">
                                    "A one-line summary shown in the list and pickers."
                                </span>
                                <input
                                    class="skills-input"
                                    placeholder="One-line description"
                                    disabled=move || saving.get()
                                    prop:value=move || edit_description.get()
                                    on:input=move |ev| {
                                        edit_description.set(event_target_value(&ev))
                                    }
                                />
                            </div>
                            <div class="pf-field">
                                <label class="skills-check">
                                    <input
                                        type="checkbox"
                                        disabled=move || saving.get()
                                        prop:checked=move || edit_advertised.get()
                                        on:change=move |ev| {
                                            edit_advertised.set(event_target_checked(&ev))
                                        }
                                    />
                                    "Visible to agent — advertise the name + description in the chat system prompt"
                                </label>
                            </div>

                            <div class="pf-group-title">"Capabilities"</div>
                            <div class="pf-field">
                                <span class="pf-label">"Tools"</span>
                                <span class="pf-help">
                                    "The tools this skill restricts itself to. Tick none to leave it unrestricted."
                                </span>
                                {move || {
                                    // Re-render the picker whenever a different skill is opened (or
                                    // a new draft begins), so the out-of-catalog rows below always
                                    // reflect the open skill — even if Leptos keeps this form
                                    // mounted across edits. `edit_tools` is read untracked so a
                                    // checkbox toggle updates only its own box, not the whole list.
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
                                        // Out-of-catalog selected tools (e.g. a renamed tool) are
                                        // appended so editing never silently drops them.
                                        let catalog: Vec<(String, String, Option<String>)> =
                                            tools_catalog
                                                .get()
                                                .into_iter()
                                                .map(|t| {
                                                    let hint =
                                                        (!t.description.is_empty()).then_some(t.description);
                                                    (t.name.clone(), t.name, hint)
                                                })
                                                .collect();
                                        let items = with_out_of_catalog(
                                            catalog,
                                            &edit_tools.get_untracked(),
                                            "not in catalog",
                                        );
                                        if items.is_empty() {
                                            view! {
                                                <div class="pf-empty">"No tools available."</div>
                                            }
                                                .into_any()
                                        } else {
                                            checklist(items, edit_tools, saving).into_any()
                                        }
                                    }
                                }}
                            </div>

                            <div class="pf-group-title">"Instructions"</div>
                            <div class="pf-field">
                                <span class="pf-help">
                                    "The markdown runbook the agent follows when it uses this skill."
                                </span>
                                <MarkdownField
                                    markdown=edit_instructions
                                    disabled=saving
                                    placeholder="Describe the runbook in markdown…"
                                />
                            </div>

                            <div class="pf-group-title">"Code (optional)"</div>
                            <div class="pf-field">
                                <span class="pf-help">
                                    "Attach code to run via the executor. Leave the language blank to remove it."
                                </span>
                                <div class="skills-code">
                                    <input
                                        class="skills-input"
                                        list="skills-code-langs"
                                        placeholder="Language (e.g. python)"
                                        disabled=move || saving.get()
                                        prop:value=move || edit_code_lang.get()
                                        on:input=move |ev| {
                                            edit_code_lang.set(event_target_value(&ev))
                                        }
                                    />
                                    <datalist id="skills-code-langs">
                                        {["python", "javascript", "typescript", "bash", "sh", "ruby", "go", "rust"]
                                            .into_iter()
                                            .map(|l| view! { <option value=l></option> })
                                            .collect::<Vec<_>>()}
                                    </datalist>
                                    <textarea
                                        class="skills-textarea skills-textarea-code"
                                        placeholder="Source…"
                                        disabled=move || saving.get()
                                        prop:value=move || edit_code_source.get()
                                        on:input=move |ev| {
                                            edit_code_source.set(event_target_value(&ev))
                                        }
                                    ></textarea>
                                    <input
                                        class="skills-input"
                                        placeholder="Entrypoint (optional, e.g. main)"
                                        disabled=move || saving.get()
                                        prop:value=move || edit_code_entrypoint.get()
                                        on:input=move |ev| {
                                            edit_code_entrypoint.set(event_target_value(&ev))
                                        }
                                    />
                                </div>
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

/// Normalize a tool-name list: trim each, drop empties, de-duplicate while
/// preserving order — keeps a skill's stored tool set clean. Mirrors the API's
/// `clean_tools`.
fn clean_tools(tools: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tools.len());
    for tool in tools {
        let t = tool.trim();
        if !t.is_empty() && !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Whether a skill matches a free-text `query` — a case-insensitive substring of
/// its name, description, instructions, or any tool. `query` is assumed trimmed.
fn skill_matches_query(skill: &Skill, query: &str) -> bool {
    let q = query.to_lowercase();
    skill.name.to_lowercase().contains(&q)
        || skill.description.to_lowercase().contains(&q)
        || skill.instructions_md.to_lowercase().contains(&q)
        || skill.tools.iter().any(|t| t.to_lowercase().contains(&q))
}

/// Build the optional [`Code`] from the editor's language + source + entrypoint
/// fields. A blank language means "no code" (source/entrypoint ignored), matching
/// the API's "clear the code" semantics when the field is absent. A blank
/// entrypoint is `None` (so it round-trips losslessly — editing a skill that has
/// an entrypoint preserves it instead of silently dropping it).
fn build_code(language: &str, source: &str, entrypoint: &str) -> Option<Code> {
    let lang = language.trim();
    if lang.is_empty() {
        return None;
    }
    let entry = entrypoint.trim();
    Some(Code {
        language: lang.to_string(),
        source: source.to_string(),
        entrypoint: (!entry.is_empty()).then(|| entry.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: &str, tools: &[&str]) -> Skill {
        Skill {
            id: String::new(),
            workspace_id: String::new(),
            name: name.to_string(),
            description: description.to_string(),
            instructions_md: String::new(),
            tools: tools.iter().map(|t| t.to_string()).collect(),
            code: None,
            advertised: true,
        }
    }

    #[test]
    fn clean_tools_trims_dedups_drops_empty() {
        let cleaned = clean_tools(vec![
            "  read_note ".into(),
            "read_note".into(),
            String::new(),
            "  ".into(),
            "kanban_create_task".into(),
        ]);
        assert_eq!(
            cleaned,
            vec!["read_note".to_string(), "kanban_create_task".to_string()]
        );
    }

    #[test]
    fn skill_matches_query_searches_name_description_and_tools() {
        let mut s = skill(
            "triage-inbox",
            "Turn notes into tasks",
            &["kanban_create_task"],
        );
        s.instructions_md = "List recent notes and create tasks".into();
        // Case-insensitive substring across name, description, instructions, tools.
        assert!(skill_matches_query(&s, "triage"));
        assert!(skill_matches_query(&s, "TASKS"));
        assert!(skill_matches_query(&s, "kanban"));
        assert!(skill_matches_query(&s, "recent notes"));
        assert!(!skill_matches_query(&s, "zzz"));
    }

    #[test]
    fn build_code_blank_language_is_none() {
        assert_eq!(build_code("  ", "print(1)", ""), None);
        assert_eq!(build_code("", "", "main"), None);
    }

    #[test]
    fn build_code_trims_language_and_keeps_source() {
        // A blank entrypoint stays `None`.
        let code = build_code("  python ", "print(1)", "  ").unwrap();
        assert_eq!(code.language, "python");
        assert_eq!(code.source, "print(1)");
        assert!(code.entrypoint.is_none());
    }

    #[test]
    fn build_code_preserves_entrypoint() {
        let code = build_code("python", "print(1)", "  main ").unwrap();
        assert_eq!(code.entrypoint.as_deref(), Some("main"));
    }
}
