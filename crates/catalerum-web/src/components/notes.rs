//! The Notes panel (SOUL §21, §12 — M3 markdown notes editor).
//!
//! A two-pane workbench panel: a left list of the workspace's notes
//! (most-recently-edited first) and a right editor (title, tags, markdown body)
//! with create / save / delete. It is a thin client of the notes REST surface
//! (`/notes`, `/notes/{id}`) — every call carries the dev session token and is
//! workspace-scoped server-side (SOUL §18).
//!
//! The editor keeps Markdown as the durable source of truth while providing a
//! visual writing surface: formatting buttons mutate the selected Markdown, and
//! a live preview renders the note safely without pulling a parser into the WASM
//! bundle. The value is durable CRUD over the note source of truth
//! (SOUL §3.1/§21).

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use super::dialogs::{use_dialogs, ConfirmSpec};
use super::md_editor::MarkdownField;
use super::widgets::{list_drawer_scrim, list_drawer_toggle};
use crate::api::{CreateNote, Note, UpdateNote};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// The Notes panel's base frontend route. The open note is deep-linkable at
/// `<NOTES_ROUTE>/<id>`; the bare route is "nothing / a fresh draft".
const NOTES_ROUTE: &str = "/app/notes";

/// The note id encoded in the current browser URL (`/app/notes/<id>`), if the
/// path carries one. Seeds the open note from a deep link or reload; a bare
/// `/app/notes` yields `None`.
fn note_from_location() -> Option<String> {
    let path = web_sys::window()?.location().pathname().ok()?;
    let id = path
        .trim_end_matches('/')
        .strip_prefix(NOTES_ROUTE)?
        .trim_start_matches('/');
    (!id.is_empty()).then(|| id.to_string())
}

/// Reflect the open note in the browser URL: `/app/notes/<id>` for a saved note,
/// or the bare `/app/notes` when nothing (or an unsaved draft) is open. Uses
/// `replace_state` so selecting notes tracks the address bar without stacking
/// per-note history entries. No-op when already at the URL.
fn sync_location_to_note(id: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match id {
        Some(id) => format!("{NOTES_ROUTE}/{id}"),
        None => NOTES_ROUTE.to_string(),
    };
    if let Ok(current) = window.location().pathname() {
        if current.trim_end_matches('/') == target {
            return;
        }
    }
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&target));
    }
}

/// The Notes panel component.
#[component]
pub fn NotesPanel() -> impl IntoView {
    // The shared, theme-aware dialog prevents an accidental destructive click.
    let dialogs = use_dialogs();
    // Loaded list + load state.
    let notes = RwSignal::new(Vec::<Note>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);
    // Active tag filter: `Some(tag)` shows only notes carrying it, `None` = all.
    let tag_filter = RwSignal::new(Option::<String>::None);
    // Free-text search over title/body/tags; composes with `tag_filter`.
    let note_query = RwSignal::new(String::new());

    // Editor state. `selected_id` is the note being edited (None + `is_new` =
    // an unsaved draft; None + !`is_new` = nothing open).
    let selected_id = RwSignal::new(Option::<String>::None);
    // A one-shot `/app/notes/<id>` deep link to open once the list loads (mount
    // only): preferred over the auto-select-first default and cleared when the
    // load consumes it, so later refreshes keep their normal behavior.
    let pending_url_id = StoredValue::new(note_from_location());
    let is_new = RwSignal::new(false);
    let edit_title = RwSignal::new(String::new());
    let edit_tags = RwSignal::new(String::new());
    let edit_markdown = RwSignal::new(String::new());
    let saving = RwSignal::new(false);
    let save_error = RwSignal::new(Option::<String>::None);

    // Whether the notes list is open as a mobile drawer (SOUL §12) — the same
    // collapsible "second sidebar" as the chat sessions list. Inert on desktop,
    // where the list is a static column; the editor is the always-visible detail
    // pane.
    let list_open = RwSignal::new(false);

    // Load a note's fields into the editor signals.
    let load_into_editor = move |note: &Note| {
        selected_id.set(Some(note.id.clone()));
        is_new.set(false);
        edit_title.set(note.title.clone());
        edit_tags.set(join_tags(&note.tags));
        edit_markdown.set(note.markdown.clone());
        save_error.set(None);
    };

    // Fetch the notes list. When `auto_select` and nothing is being edited, open
    // the first note so the editor isn't empty on first paint.
    let refresh = move |auto_select: bool| {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_notes(token.as_deref()).await {
                Ok(list) => {
                    if auto_select
                        && !is_new.get_untracked()
                        && selected_id.get_untracked().is_none()
                    {
                        // Prefer a `/app/notes/<id>` deep link (one-shot, mount
                        // only); otherwise open the first note so the editor
                        // isn't empty on first paint. A stale/unknown deep-link
                        // id falls back to the first note.
                        let picked = pending_url_id
                            .get_value()
                            .and_then(|id| list.iter().position(|n| n.id == id))
                            .or_else(|| (!list.is_empty()).then_some(0));
                        pending_url_id.set_value(None);
                        if let Some(idx) = picked {
                            load_into_editor(&list[idx]);
                        }
                    }
                    notes.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    notes.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Initial load.
    refresh(true);

    // Mirror the open note into the URL as `/app/notes/<id>` so it's
    // deep-linkable and survives reload. Held back until the pending deep link
    // (if any) has been consumed by the load above, so the async open doesn't
    // race a URL wipe; thereafter it tracks every select/new/delete via the
    // single `selected_id` signal. See `sync_location_to_note`.
    Effect::new(move |_| {
        let id = selected_id.get();
        if pending_url_id.get_value().is_some() {
            return;
        }
        sync_location_to_note(id.as_deref());
    });

    // Begin a new, unsaved note (clears the editor).
    let start_new = move || {
        selected_id.set(None);
        is_new.set(true);
        edit_title.set(String::new());
        edit_tags.set(String::new());
        edit_markdown.set(String::new());
        save_error.set(None);
    };

    // Save the editor: create a new note or update the open one.
    let save = move || {
        if saving.get_untracked() {
            return;
        }
        let title = edit_title.get_untracked().trim().to_string();
        save_error.set(None);
        if title.is_empty() {
            save_error.set(Some("Give the note a title.".to_string()));
            return;
        }
        let tags = parse_tags(&edit_tags.get_untracked());
        let markdown = edit_markdown.get_untracked();
        let editing_id = selected_id.get_untracked();

        saving.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let tok = token.as_deref();
            let result: Result<Note, rest::RestError> = match editing_id {
                Some(id) => {
                    rest::update_note(
                        tok,
                        &id,
                        &UpdateNote {
                            title,
                            markdown,
                            tags,
                        },
                    )
                    .await
                }
                None => {
                    rest::create_note(
                        tok,
                        &CreateNote {
                            title,
                            markdown,
                            tags,
                        },
                    )
                    .await
                }
            };
            // The editor inputs + the New / list-row buttons are all disabled
            // while `saving`, so nothing the user did during the await can be
            // clobbered here: re-loading the server's echoed note is always safe.
            saving.set(false);
            match result {
                Ok(note) => {
                    load_into_editor(&note);
                    refresh(false);
                }
                Err(e) => save_error.set(Some(e.to_string())),
            }
        });
    };

    // Delete the open note only after an explicit confirmation. Capture both
    // the id and display title now so the dialog describes exactly which note
    // its deferred action will remove.
    let delete = move || {
        let Some(id) = selected_id.get_untracked() else {
            return;
        };
        if saving.get_untracked() {
            return;
        }
        let title = match edit_title.get_untracked().trim() {
            "" => "Untitled note".to_string(),
            title => title.to_string(),
        };
        dialogs.confirm(
            ConfirmSpec::danger(
                "Delete note?",
                format!("Delete “{title}”? This cannot be undone."),
                "Delete",
            ),
            move || {
                let id = id.clone();
                saving.set(true);
                save_error.set(None);
                spawn_local(async move {
                    let token = auth::resolve_token();
                    match rest::delete_note(token.as_deref(), &id).await {
                        Ok(()) => {
                            saving.set(false);
                            // Clear the editor and let the refresh re-open the next note.
                            selected_id.set(None);
                            is_new.set(false);
                            edit_title.set(String::new());
                            edit_tags.set(String::new());
                            edit_markdown.set(String::new());
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

    // Whether the editor pane is showing (a note open or a new draft).
    let editor_open = move || selected_id.get().is_some() || is_new.get();
    let on_save_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        save();
    };

    // The notes shown after the active tag filter, and the distinct tags across
    // all notes (for the filter bar). Both read `notes`/`tag_filter` reactively.
    // Notes after BOTH the tag filter and the free-text query (composed).
    let visible_notes = move || {
        notes.with(|ns| {
            let by_tag = tag_filter.with(|t| filter_notes_by_tag(ns, t.as_deref()));
            note_query.with(|q| {
                let q = q.trim();
                if q.is_empty() {
                    by_tag
                } else {
                    by_tag
                        .into_iter()
                        .filter(|n| note_matches_query(n, q))
                        .collect()
                }
            })
        })
    };
    let all_tags = move || notes.with(|ns| distinct_tags(ns));

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Notes"</h2>
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
                        when=move || !notes.with(Vec::is_empty)
                        fallback=|| ().into_view()
                    >
                        <input
                            class="pane-search"
                            placeholder="Search notes…"
                            prop:value=move || note_query.get()
                            on:input=move |ev| note_query.set(event_target_value(&ev))
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
                                    "Could not load notes: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && notes.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No notes yet. Create one →"</div>
                    </Show>

                    <Show
                        when=move || !all_tags().is_empty()
                        fallback=|| ().into_view()
                    >
                        <div class="notes-tagbar">
                            <button
                                class=move || {
                                    if tag_filter.with(Option::is_none) {
                                        "notes-tag-chip notes-tag-chip-active"
                                    } else {
                                        "notes-tag-chip"
                                    }
                                }
                                on:click=move |_| tag_filter.set(None)
                            >
                                "All"
                            </button>
                            <For
                                each=move || all_tags()
                                key=|t| t.clone()
                                children=move |t: String| {
                                    let t_click = t.clone();
                                    let t_active = t.clone();
                                    let active =
                                        move || tag_filter.with(|f| f.as_deref() == Some(t_active.as_str()));
                                    view! {
                                        <button
                                            class=move || {
                                                if active() {
                                                    "notes-tag-chip notes-tag-chip-active"
                                                } else {
                                                    "notes-tag-chip"
                                                }
                                            }
                                            on:click=move |_| {
                                                // Toggle: clicking the active tag clears the filter.
                                                if tag_filter.with(|f| f.as_deref() == Some(t_click.as_str()))
                                                {
                                                    tag_filter.set(None);
                                                } else {
                                                    tag_filter.set(Some(t_click.clone()));
                                                }
                                            }
                                        >
                                            {format!("#{t}")}
                                        </button>
                                    }
                                }
                            />
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !notes.with(Vec::is_empty) && visible_notes().is_empty()
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No notes match."</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || visible_notes()
                            key=|n| (n.id.clone(), n.updated_at.clone())
                            children=move |n: Note| {
                                let id = n.id.clone();
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
                                let title = if n.title.trim().is_empty() {
                                    "(untitled)".to_string()
                                } else {
                                    n.title.clone()
                                };
                                // Agent-authored notes (e.g. written by an
                                // automation) are flagged so they're distinguishable
                                // from a user's own notes (SOUL §5/§21).
                                let is_agent = n.author.kind == "agent";
                                let preview = note_preview(&n.markdown);
                                let tags = n.tags.clone();
                                let note_for_click = n.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            disabled=move || saving.get()
                                            on:click=move |_| {
                                                load_into_editor(&note_for_click);
                                                list_open.set(false);
                                            }
                                        >
                                            <span class="pane-item-title">
                                                {title}
                                                <Show
                                                    when={
                                                        let a = is_agent;
                                                        move || a
                                                    }
                                                    fallback=|| ().into_view()
                                                >
                                                    <span
                                                        class="notes-item-agent"
                                                        title="Created by an agent"
                                                    >
                                                        "agent"
                                                    </span>
                                                </Show>
                                            </span>
                                            <Show
                                                when={
                                                    let has = !preview.is_empty();
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="pane-item-preview">
                                                    {preview.clone()}
                                                </span>
                                            </Show>
                                            {(!tags.is_empty()).then(|| view! {
                                                <span class="notes-item-tags">
                                                    {tags
                                                        .iter()
                                                        .map(|t| view! {
                                                            <span class="notes-item-tag">{format!("#{t}")}</span>
                                                        })
                                                        .collect::<Vec<_>>()}
                                                </span>
                                            })}
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            {list_drawer_toggle("Notes", list_open)}
            <div class="pane-detail">
                <Show
                    when=editor_open
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select a note, or create a new one."</p>
                            </div>
                        }
                    }
                >
                    <form class="notes-form" on:submit=on_save_submit>
                        <input
                            class="notes-input notes-input-title"
                            placeholder="Title"
                            disabled=move || saving.get()
                            prop:value=move || edit_title.get()
                            on:input=move |ev| edit_title.set(event_target_value(&ev))
                        />
                        <input
                            class="notes-input notes-input-tags"
                            placeholder="Tags (comma-separated)"
                            disabled=move || saving.get()
                            prop:value=move || edit_tags.get()
                            on:input=move |ev| edit_tags.set(event_target_value(&ev))
                        />

                        <MarkdownField
                            markdown=edit_markdown
                            disabled=saving
                            placeholder="Write your note in markdown…"
                        />

                        <Show
                            when=move || save_error.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <div class="notes-form-error">
                                {move || save_error.get().unwrap_or_default()}
                            </div>
                        </Show>

                        <div class="notes-form-actions">
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
                                    <Icon icon=MdIcon::Delete />
                                    <span>"Delete"</span>
                                </button>
                            </Show>
                        </div>
                    </form>
                </Show>
            </div>
        </section>
    }
}

/// Parse a comma-separated tag string into a clean tag list: trim each, drop
/// empties, de-duplicate while preserving order. Mirrors the API's `clean_tags`
/// so the client sends what the server would have stored anyway.
fn parse_tags(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in input.split(',') {
        let trimmed = tag.trim();
        if !trimmed.is_empty() && !out.iter().any(|t| t == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

/// Render a tag list back into the comma-separated form the editor shows.
fn join_tags(tags: &[String]) -> String {
    tags.join(", ")
}

/// The distinct tags across all notes, sorted, for the filter bar.
fn distinct_tags(notes: &[Note]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    for note in notes {
        for tag in &note.tags {
            seen.insert(tag.clone());
        }
    }
    seen.into_iter().collect()
}

/// Whether a note matches a free-text `query` — a case-insensitive substring of
/// its title, markdown body, or any tag. `query` is assumed already trimmed.
fn note_matches_query(note: &Note, query: &str) -> bool {
    let q = query.to_lowercase();
    note.title.to_lowercase().contains(&q)
        || note.markdown.to_lowercase().contains(&q)
        || note.tags.iter().any(|t| t.to_lowercase().contains(&q))
}

/// Filter `notes` to those carrying `tag`; `None` returns them all (unfiltered).
fn filter_notes_by_tag(notes: &[Note], tag: Option<&str>) -> Vec<Note> {
    match tag {
        Some(tag) => notes
            .iter()
            .filter(|n| n.tags.iter().any(|t| t == tag))
            .cloned()
            .collect(),
        None => notes.to_vec(),
    }
}

/// A short one-line preview of a note body for the list: the first non-empty
/// line, trimmed of markdown heading/bullet markers, capped at 80 chars.
fn note_preview(markdown: &str) -> String {
    let line = markdown
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let stripped = line.trim_start_matches(['#', '-', '*', '>', ' ']).trim();
    if stripped.chars().count() > 80 {
        let truncated: String = stripped.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        stripped.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::NoteAuthor;

    fn note(id: &str, tags: &[&str]) -> Note {
        Note {
            id: id.to_string(),
            workspace_id: String::new(),
            author: NoteAuthor {
                kind: "user".to_string(),
                id: String::new(),
            },
            title: id.to_string(),
            markdown: String::new(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn distinct_tags_are_sorted_and_deduped() {
        let notes = [
            note("a", &["work", "ideas"]),
            note("b", &["ideas", "urgent"]),
        ];
        assert_eq!(distinct_tags(&notes), ["ideas", "urgent", "work"]);
        assert!(distinct_tags(&[note("c", &[])]).is_empty());
    }

    #[test]
    fn note_matches_query_searches_title_body_and_tags() {
        let mut n = note("x", &["urgent"]);
        n.title = "Quarterly plan".into();
        n.markdown = "Ship the **widget** by Friday".into();
        // Case-insensitive substring across title, body, and tags.
        assert!(note_matches_query(&n, "quarterly"));
        assert!(note_matches_query(&n, "WIDGET"));
        assert!(note_matches_query(&n, "urgent"));
        // No match anywhere.
        assert!(!note_matches_query(&n, "zzz"));
    }

    #[test]
    fn filter_notes_by_tag_matches_membership() {
        let notes = [
            note("a", &["work"]),
            note("b", &["work", "ideas"]),
            note("c", &["ideas"]),
        ];
        // None → everything.
        assert_eq!(filter_notes_by_tag(&notes, None).len(), 3);
        // A tag keeps only notes that carry it.
        let work: Vec<_> = filter_notes_by_tag(&notes, Some("work"))
            .into_iter()
            .map(|n| n.id)
            .collect();
        assert_eq!(work, ["a", "b"]);
        // An unknown tag yields nothing.
        assert!(filter_notes_by_tag(&notes, Some("nope")).is_empty());
    }

    #[test]
    fn parse_tags_trims_dedups_and_drops_empties() {
        assert_eq!(
            parse_tags("  work , work, ,ideas,"),
            vec!["work".to_string(), "ideas".to_string()]
        );
        assert!(parse_tags("   ").is_empty());
        assert!(parse_tags("").is_empty());
    }

    #[test]
    fn join_then_parse_round_trips() {
        let tags = vec!["a".to_string(), "b c".to_string()];
        assert_eq!(parse_tags(&join_tags(&tags)), tags);
    }

    #[test]
    fn preview_takes_first_line_stripped() {
        assert_eq!(note_preview("# Heading\nbody"), "Heading");
        assert_eq!(note_preview("\n\n- milk\n- eggs"), "milk");
        assert_eq!(note_preview(""), "");
    }

    #[test]
    fn preview_caps_length() {
        let long = "x".repeat(200);
        let preview = note_preview(&long);
        // 80 chars + the ellipsis.
        assert_eq!(preview.chars().count(), 81);
        assert!(preview.ends_with('…'));
    }
}
