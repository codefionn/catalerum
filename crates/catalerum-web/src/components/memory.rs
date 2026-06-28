//! The Memory panel (SOUL §22, §12 — memories + profile).
//!
//! A two-pane workbench panel: a left column of the workspace's visible memories
//! (with create / inline-edit / delete) and a right **profile** editor (the
//! per-user structured record merged into the chat system prompt each turn). It
//! is a thin client of the memory REST surface (`/memories`, `/memories/{id}`,
//! `/profile`) — every call carries the dev session token and is workspace-scoped
//! and capability-gated server-side (SOUL §18/§19). Listed memories are
//! visibility-filtered by the server: workspace-shared ones plus the caller's own
//! private ones, never another member's private memory (§22).

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::widgets::{list_drawer_scrim, list_drawer_toggle};
use crate::api::{CreateMemory, Memory, UpdateMemory};
use crate::auth;
use crate::rest;

/// The Memory panel component.
#[component]
pub fn MemoryPanel() -> impl IntoView {
    let memories = RwSignal::new(Vec::<Memory>::new());
    let loading = RwSignal::new(true);
    let error = RwSignal::new(Option::<String>::None);
    let busy = RwSignal::new(false);

    // New-memory form.
    let new_text = RwSignal::new(String::new());
    let new_scope = RwSignal::new("user".to_string());

    // Inline edit of one memory.
    let editing_id = RwSignal::new(Option::<String>::None);
    let edit_text = RwSignal::new(String::new());

    // Profile editor.
    let profile_text = RwSignal::new(String::new());
    let profile_busy = RwSignal::new(false);
    let profile_error = RwSignal::new(Option::<String>::None);

    let load_memories = move || {
        loading.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_memories(token.as_deref()).await {
                Ok(list) => {
                    memories.set(list);
                    error.set(None);
                }
                Err(e) => {
                    memories.set(Vec::new());
                    error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    let load_profile = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::get_profile(token.as_deref()).await {
                Ok(p) => profile_text.set(pretty_fields(&p.fields)),
                Err(e) => profile_error.set(Some(e.to_string())),
            }
        });
    };

    load_memories();
    load_profile();

    // Create a memory from the new-memory form.
    let create = move || {
        if busy.get_untracked() {
            return;
        }
        let text = new_text.get_untracked().trim().to_string();
        error.set(None);
        if text.is_empty() {
            error.set(Some("Enter some memory text.".to_string()));
            return;
        }
        let scope = new_scope.get_untracked();
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::create_memory(token.as_deref(), &CreateMemory { scope, text }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    new_text.set(String::new());
                    load_memories();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Save an in-progress inline edit.
    let save_edit = move || {
        let Some(id) = editing_id.get_untracked() else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        let text = edit_text.get_untracked().trim().to_string();
        if text.is_empty() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::update_memory(token.as_deref(), &id, &UpdateMemory { text }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    editing_id.set(None);
                    load_memories();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Delete a memory.
    let delete = move |id: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_memory(token.as_deref(), &id).await {
                Ok(()) => {
                    busy.set(false);
                    if editing_id.get_untracked().as_deref() == Some(id.as_str()) {
                        editing_id.set(None);
                    }
                    load_memories();
                }
                Err(e) => {
                    busy.set(false);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Save the profile (merge the edited fields).
    let save_profile = move || {
        if profile_busy.get_untracked() {
            return;
        }
        profile_error.set(None);
        let fields = match parse_fields(&profile_text.get_untracked()) {
            Ok(v) => v,
            Err(e) => {
                profile_error.set(Some(e));
                return;
            }
        };
        profile_busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::update_profile(token.as_deref(), &fields).await {
                Ok(p) => {
                    profile_text.set(pretty_fields(&p.fields));
                    profile_error.set(None);
                }
                Err(e) => profile_error.set(Some(e.to_string())),
            }
            profile_busy.set(false);
        });
    };

    let on_new_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        create();
    };

    // Whether the memories list is open as a mobile drawer (SOUL §12); inert on
    // desktop. The Profile editor is the always-visible detail pane.
    let list_open = RwSignal::new(false);

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list mem-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Memories"</h2>
                </header>

                <form class="mem-new" on:submit=on_new_submit>
                    <textarea
                        class="mem-new-text"
                        placeholder="Remember that…"
                        disabled=move || busy.get()
                        prop:value=move || new_text.get()
                        on:input=move |ev| new_text.set(event_target_value(&ev))
                    ></textarea>
                    <div class="mem-new-actions">
                        <select
                            class="mem-select"
                            disabled=move || busy.get()
                            on:change=move |ev| new_scope.set(event_target_value(&ev))
                        >
                            <option value="user">"Private"</option>
                            <option value="workspace">"Shared"</option>
                        </select>
                        <button class="mem-btn mem-btn-primary" type="submit" disabled=move || busy.get()>
                            "Add"
                        </button>
                    </div>
                </form>

                <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                    <div class="pane-list-status pane-list-error">
                        {move || error.get().unwrap_or_default()}
                    </div>
                </Show>

                <div class="pane-list-body">
                    <Show when=move || loading.get() fallback=|| ().into_view()>
                        <div class="pane-list-status">"Loading…"</div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && error.with(Option::is_none)
                                && memories.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No memories yet. Add one above."</div>
                    </Show>

                    <ul class="mem-items">
                        <For
                            each=move || memories.get()
                            key=|m| (m.id.clone(), m.text.clone())
                            children=move |m: Memory| {
                                let id = m.id.clone();
                                let text = m.text.clone();
                                let scope = m.scope.clone();
                                let when = fmt_ts(&m.created_at);
                                let is_editing = {
                                    let id = id.clone();
                                    move || editing_id.get().as_deref() == Some(id.as_str())
                                };
                                let id_edit = id.clone();
                                let text_for_edit = text.clone();
                                let id_del = id.clone();
                                view! {
                                    <li class="mem-item">
                                        <div class="mem-item-head">
                                            <span class=format!(
                                                "mem-scope mem-scope-{}",
                                                scope_token(&scope),
                                            )>{scope_label(&scope)}</span>
                                            <span class="mem-when">{when}</span>
                                        </div>
                                        <Show
                                            when=is_editing.clone()
                                            fallback={
                                                let text = text.clone();
                                                move || {
                                                    view! {
                                                        <div class="mem-item-text">{text.clone()}</div>
                                                    }
                                                }
                                            }
                                        >
                                            <textarea
                                                class="mem-edit-text"
                                                disabled=move || busy.get()
                                                prop:value=move || edit_text.get()
                                                on:input=move |ev| {
                                                    edit_text.set(event_target_value(&ev))
                                                }
                                            ></textarea>
                                        </Show>
                                        <div class="mem-item-actions">
                                            <Show
                                                when=is_editing.clone()
                                                fallback={
                                                    let id_edit = id_edit.clone();
                                                    let text_for_edit = text_for_edit.clone();
                                                    let id_del = id_del.clone();
                                                    move || {
                                                        let id_edit = id_edit.clone();
                                                        let text_for_edit = text_for_edit.clone();
                                                        let id_del = id_del.clone();
                                                        view! {
                                                            <button
                                                                class="mem-btn"
                                                                disabled=move || busy.get()
                                                                on:click=move |_| {
                                                                    edit_text.set(text_for_edit.clone());
                                                                    editing_id.set(Some(id_edit.clone()));
                                                                }
                                                            >
                                                                "Edit"
                                                            </button>
                                                            <button
                                                                class="mem-btn mem-btn-danger"
                                                                disabled=move || busy.get()
                                                                on:click=move |_| delete(id_del.clone())
                                                            >
                                                                "Delete"
                                                            </button>
                                                        }
                                                    }
                                                }
                                            >
                                                <button
                                                    class="mem-btn mem-btn-primary"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| save_edit()
                                                >
                                                    "Save"
                                                </button>
                                                <button
                                                    class="mem-btn"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| editing_id.set(None)
                                                >
                                                    "Cancel"
                                                </button>
                                            </Show>
                                        </div>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            {list_drawer_toggle("Memories", list_open)}
            <div class="mem-profile">
                <header class="mem-profile-head">
                    <h2 class="pane-list-title">"Profile"</h2>
                    <span class="mem-profile-hint">
                        "Merged into your chat context each turn. Saving merges keys (it can't remove them)."
                    </span>
                </header>
                <textarea
                    class="mem-profile-text"
                    placeholder="{}"
                    disabled=move || profile_busy.get()
                    prop:value=move || profile_text.get()
                    on:input=move |ev| profile_text.set(event_target_value(&ev))
                ></textarea>
                <Show
                    when=move || profile_error.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="pane-list-status pane-list-error">
                        {move || profile_error.get().unwrap_or_default()}
                    </div>
                </Show>
                <div class="mem-profile-actions">
                    <button
                        class="mem-btn mem-btn-primary"
                        disabled=move || profile_busy.get()
                        on:click=move |_| save_profile()
                    >
                        {move || if profile_busy.get() { "Saving…" } else { "Save profile" }}
                    </button>
                </div>
            </div>
        </section>
    }
}

/// A display label for a memory scope.
fn scope_label(scope: &str) -> &'static str {
    match scope {
        "workspace" => "Shared",
        _ => "Private",
    }
}

/// The CSS modifier suffix for a scope badge.
fn scope_token(scope: &str) -> &'static str {
    match scope {
        "workspace" => "shared",
        _ => "private",
    }
}

/// Pretty-print profile fields for the editor. An empty / non-object value
/// renders as `{}`, so the textarea always shows valid JSON to start from.
fn pretty_fields(value: &serde_json::Value) -> String {
    match value.as_object() {
        Some(map) if !map.is_empty() => serde_json::to_string_pretty(value).unwrap_or_default(),
        _ => "{}".to_string(),
    }
}

/// Parse the profile editor into a JSON object. Empty → `{}`; a non-object or
/// malformed input is a client-side error.
fn parse_fields(input: &str) -> Result<serde_json::Value, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON ({e})"))?;
    if !value.is_object() {
        return Err("expected a JSON object".to_string());
    }
    Ok(value)
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scope_label_and_token() {
        assert_eq!(scope_label("workspace"), "Shared");
        assert_eq!(scope_label("user"), "Private");
        assert_eq!(scope_label("weird"), "Private");
        assert_eq!(scope_token("workspace"), "shared");
        assert_eq!(scope_token("user"), "private");
    }

    #[test]
    fn pretty_fields_empty_is_braces() {
        assert_eq!(pretty_fields(&json!({})), "{}");
        assert_eq!(pretty_fields(&serde_json::Value::Null), "{}");
        assert!(pretty_fields(&json!({"tz":"UTC"})).contains("tz"));
    }

    #[test]
    fn parse_fields_object_only() {
        assert_eq!(parse_fields("  ").unwrap(), json!({}));
        assert_eq!(
            parse_fields(r#"{"tz":"UTC"}"#).unwrap(),
            json!({"tz":"UTC"})
        );
        assert!(parse_fields("[1,2]").is_err());
        assert!(parse_fields("{bad").is_err());
    }

    #[test]
    fn fmt_ts_trims_to_minute() {
        assert_eq!(fmt_ts("2026-06-18T09:00:00Z"), "2026-06-18 09:00");
        assert_eq!(fmt_ts("nope"), "nope");
    }
}
