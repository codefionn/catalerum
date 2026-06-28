//! Small shared widgets (SOUL §12) reused across the panels.
//!
//! [`checklist`] picks a set of values from a catalog (tools, skills, subagents);
//! [`chip_input`] is a free-text removable-chip set for fields with no catalog (or
//! as the catalog's offline fallback). Profiles pioneered both; Skills reuses them
//! so the tool-picking experience is identical across the workbench.
//!
//! [`row_action`] is the shared edit (`✎`) / delete (`✕`) icon button for list
//! rows — one look and one danger treatment across the chat, calendar and notes
//! surfaces (each of which used to hand-roll its own button + CSS).
//!
//! [`copy_button`] (over [`copy_to_clipboard`]) is the shared copy-to-clipboard
//! control with transient "copied" feedback — used by the chat message bar and
//! the Apps list.
//!
//! [`list_drawer_scrim`] + [`list_drawer_toggle`] turn a master-detail panel's
//! list pane into an off-canvas drawer on narrow viewports — the ☰ "second
//! hamburger" (after the workbench nav's) on Apps, Skills, Profiles, Grants,
//! History, Memory, Calendars, Notes and Automations, mirroring the chat
//! sessions sidebar.

use std::collections::HashSet;
use std::time::Duration;

use leptos::portal::Portal;
use leptos::prelude::*;

use crate::api::{Attachment, ModelInfo, VoiceInfo};
use crate::components::icons::{Icon, MdIcon};
use crate::{auth, rest};

/// Append any `selected` values not already present in `items` as extra rows
/// (labeled with `extra_hint`), preserving order. A bare [`checklist`] renders
/// only its catalog rows and reads `selected` merely to tick them — so a selected
/// value absent from the catalog (e.g. a renamed/removed tool, or a deleted skill
/// a profile still references) would be invisible and un-untickable while still
/// being saved. Folding such values back in keeps the picker honest.
pub fn with_out_of_catalog(
    mut items: Vec<(String, String, Option<String>)>,
    selected: &[String],
    extra_hint: &str,
) -> Vec<(String, String, Option<String>)> {
    // An empty catalog is ambiguous — it may simply not have loaded yet — so only
    // attach the "missing" hint when the catalog is populated (hence known-loaded).
    // Otherwise a real, still-loading entry would be mislabeled as out-of-catalog.
    let hint = (!items.is_empty()).then(|| extra_hint.to_string());
    // Collect the missing values first so the immutable borrow of `items` (via
    // `known`) is released before we push to it.
    let missing: Vec<String> = {
        let known: HashSet<&str> = items.iter().map(|(value, _, _)| value.as_str()).collect();
        selected
            .iter()
            .filter(|s| !known.contains(s.as_str()))
            .cloned()
            .collect()
    };
    for value in missing {
        items.push((value.clone(), value, hint.clone()));
    }
    items
}

/// A checkbox list over a catalog of `(value, label, optional hint)` rows that
/// toggles membership of `selected`. Ticking a row adds its `value` to the set;
/// unticking removes it — so the set stays deduped by construction.
pub fn checklist(
    items: Vec<(String, String, Option<String>)>,
    selected: RwSignal<Vec<String>>,
    disabled: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <div class="pf-checklist">
            {items
                .into_iter()
                .map(|(value, label, hint)| {
                    let v_checked = value.clone();
                    let is_checked = move || selected.get().iter().any(|s| s == &v_checked);
                    let v_toggle = value.clone();
                    let toggle = move |_| {
                        selected
                            .update(|sel| {
                                if let Some(pos) = sel.iter().position(|s| s == &v_toggle) {
                                    sel.remove(pos);
                                } else {
                                    sel.push(v_toggle.clone());
                                }
                            });
                    };
                    view! {
                        <label class="pf-check">
                            <input
                                type="checkbox"
                                class="pf-check-box"
                                prop:checked=is_checked
                                disabled=move || disabled.get()
                                on:change=toggle
                            />
                            <span class="pf-check-text">
                                <span class="pf-check-name">{label}</span>
                                {hint
                                    .map(|h| view! { <span class="pf-check-hint">{h}</span> })}
                            </span>
                        </label>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
    }
}

/// A free-text chip input bound to `selected`: each entry renders as a removable
/// chip; typing a value and pressing Enter appends it (deduped, trimmed). Used for
/// fields with no catalog (channels) and as the tools fallback.
pub fn chip_input(
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

/// The tap-away backdrop behind a master-detail list pane's mobile drawer (SOUL
/// §12). Inert on desktop, where the list is a static column; on narrow viewports
/// it dims the detail pane and closes the drawer on click. Place it as the first
/// child of the master-detail container, before the list `<aside>` — which
/// carries the `list-drawer` class plus `class:list-drawer-open=move || open.get()`.
/// Pairs with [`list_drawer_toggle`]; both mirror the chat sessions sidebar.
pub fn list_drawer_scrim(open: RwSignal<bool>) -> impl IntoView {
    view! {
        <button
            class="list-drawer-scrim"
            class:list-drawer-scrim-open=move || open.get()
            aria-label="Close list"
            tabindex="-1"
            on:click=move |_| open.set(false)
        ></button>
    }
}

/// The ☰ button that opens the master-detail list drawer paired with a
/// [`list_drawer_scrim`] (SOUL §12) — the same affordance as the chat sessions
/// sidebar's "☰ Chats". Hidden on desktop (the list is a static column) and shown
/// on narrow viewports; place it at the top of the detail pane so it stays
/// reachable whether or not an item is selected. `label` names the list (e.g.
/// "Skills"), rendering "☰ Skills".
pub fn list_drawer_toggle(label: &'static str, open: RwSignal<bool>) -> impl IntoView {
    view! {
        <button
            class="list-drawer-toggle"
            type="button"
            title="Show list"
            on:click=move |_| open.update(|o| *o = !*o)
        >
            <Icon icon=MdIcon::Menu />
            <span>{label}</span>
        </button>
    }
}

/// A small icon action button for list-row controls — the shared edit (`✎`) /
/// delete (`✕`) affordance for the chat, calendar and notes rows (SOUL §12).
/// Before this, each surface hand-rolled its own `<button>` + CSS class set
/// (`chat-session-act`, `cal-event-edit`, `cal-cal-del`, `cal-source-del`, …);
/// routing them all through here gives one look, one hover treatment, and one
/// error-red danger tint.
///
/// `icon` is the Material icon; `title` the hover/aria tooltip; `danger` tints
/// it error-red on hover (set it for deletes); `on_click` fires on click.
/// Positioning and any hover-reveal stay with the caller's row wrapper (the
/// `.row-acts` / `.row-acts-reveal` classes), so this is purely the button.
pub fn row_action(
    icon: MdIcon,
    title: impl Into<String>,
    danger: bool,
    on_click: impl Fn() + 'static,
) -> impl IntoView {
    let title = title.into();
    view! {
        <button
            type="button"
            class="row-act"
            class:row-act-danger=danger
            title=title
            on:click=move |_| on_click()
        >
            <Icon icon />
        </button>
    }
}

/// Copy `text` to the system clipboard (fire-and-forget; the async clipboard API
/// resolves on its own and fails only silently — the usual failure is a
/// non-secure context or a denied permission, neither actionable here). The
/// web_sys `Navigator`/`Clipboard` features are enabled in the crate manifest.
pub fn copy_to_clipboard(text: &str) {
    if let Some(win) = web_sys::window() {
        let _ = win.navigator().clipboard().write_text(text);
    }
}

/// A copy-to-clipboard button with transient "copied" feedback (SOUL §12) —
/// shared by the chat message bar and the Apps list. `text` is read lazily at
/// click time so it always copies the current value (e.g. a streaming message's
/// latest text). `idle`/`copied` are the resting and just-copied captions (use
/// glyphs for icon bars, words for prose); after ~1.2s it reverts to `idle` and
/// carries the shared `.copy-btn-done` class while flashing. `extra_class` is the
/// caller's positioning/appearance class so the button matches its neighbours.
pub fn copy_button(
    text: impl Fn() -> String + 'static,
    idle: &'static str,
    copied: &'static str,
    extra_class: &'static str,
) -> impl IntoView {
    let done = RwSignal::new(false);
    view! {
        <button
            type="button"
            class=format!("copy-btn {extra_class}")
            class:copy-btn-done=move || done.get()
            title="Copy to clipboard"
            on:click=move |_| {
                copy_to_clipboard(&text());
                done.set(true);
                // Revert the flash after a beat; a rapid re-click just restarts it.
                set_timeout(move || done.set(false), Duration::from_millis(1200));
            }
        >
            {move || if done.get() { copied } else { idle }}
        </button>
    }
}

/// Map a model catalog into `(id, label)` pairs for [`model_autocomplete`],
/// falling back to the id when a model has no display name. When `with_ctx` is
/// set, a model that advertises a context length gets a "· Nk ctx" suffix (used
/// by the workspace settings panel; the leaner pickers pass `false`).
pub fn model_options(
    models: RwSignal<Vec<ModelInfo>>,
    with_ctx: bool,
) -> Signal<Vec<(String, String)>> {
    Signal::derive(move || {
        models
            .get()
            .into_iter()
            .map(|m| {
                let name = if m.name.is_empty() {
                    m.id.clone()
                } else {
                    m.name.clone()
                };
                let label = match m.context_length {
                    Some(c) if with_ctx && c >= 1000 => format!("{name} · {}k ctx", c / 1000),
                    _ => name,
                };
                (m.id, label)
            })
            .collect()
    })
}

/// Map a voice catalog into `(id, label)` pairs for [`model_autocomplete`],
/// falling back to the id when a voice has no display name.
pub fn voice_options(voices: RwSignal<Vec<VoiceInfo>>) -> Signal<Vec<(String, String)>> {
    Signal::derive(move || {
        voices
            .get()
            .into_iter()
            .map(|v| {
                let label = v.name.clone().unwrap_or_else(|| v.id.clone());
                (v.id, label)
            })
            .collect()
    })
}

/// A free-text input with autocomplete search over a catalog — the shared model /
/// voice picker (SOUL §12). It replaces the old `<select>` dropdowns and native
/// `<datalist>`s with one consistent combobox: the user types, the suggestion list
/// filters by substring (over both id and display label), and a row can be chosen
/// by click or keyboard (↑/↓ to move, Enter to pick, Esc to cancel). Because it is
/// just an input under the hood it still accepts a hand-typed id that isn't in the
/// catalog (e.g. a model the gateway doesn't enumerate, or while the catalog is
/// still loading / failed to load) — so it degrades gracefully with no separate
/// fallback path.
///
/// `value` is the committed selection (read to seed and re-sync the field, and to
/// mark the matching row as current); `on_commit` fires when the user settles on a
/// value — by picking a row, pressing Enter, or blurring after editing — never on
/// every keystroke, so callers that persist over the network (e.g. the per-chat
/// model pin) don't get a request per character. `options` is the reactive
/// `(id, label)` catalog (see [`model_options`] / [`voice_options`]); the empty
/// string is a valid committed value meaning "use the default" (conveyed by the
/// `placeholder`). `input_class` is the caller's existing input class so the field
/// matches its surrounding form.
pub fn model_autocomplete(
    value: Signal<String>,
    on_commit: impl Fn(String) + Copy + Send + Sync + 'static,
    options: Signal<Vec<(String, String)>>,
    placeholder: Signal<String>,
    disabled: Signal<bool>,
    input_class: &'static str,
) -> impl IntoView {
    // `draft` is what the box shows while editing — kept distinct from the
    // committed `value` so typing can filter without committing. `dirty` gates
    // filter-vs-show-all (a freshly focused box lists the whole catalog; once the
    // user types it narrows). `active` is the highlighted row for keyboard pick
    // (None until the user navigates/types, so a bare Enter doesn't grab row 0).
    let draft = RwSignal::new(value.get_untracked());
    let open = RwSignal::new(false);
    let dirty = RwSignal::new(false);
    let active = RwSignal::new(Option::<usize>::None);
    let focused = RwSignal::new(false);

    // The suggestion list is rendered into <body> (a `Portal`) with `position:
    // fixed` rather than floating inside `.ac-wrap`. Every panel that hosts this
    // picker (SOUL §12: chat sidebar, settings modal, profiles, onboarding, flow
    // inspector) is a scroll container — `overflow: auto/hidden` — and a CSS
    // scroll container *cannot* stop clipping its other axis while staying
    // scrollable, so an in-flow absolute dropdown gets cut off at the panel edge
    // (most visibly when the field sits near the bottom). A fixed, body-level node
    // escapes all of them (and any transformed ancestor, e.g. the flow canvas).
    // The trade-off: fixed coords don't follow layout on their own, so we anchor
    // the list to the input by measuring its on-screen box whenever the box can
    // move — open, refilter (`measure` is called from the handlers below) and
    // window resize. `(left, top, width)` in viewport coords; top already includes
    // the small gap the old `top: calc(100% + 3px)` provided.
    let input_ref: NodeRef<leptos::html::Input> = NodeRef::new();
    let menu_pos = RwSignal::new((0.0_f64, 0.0_f64, 0.0_f64));
    let measure = move || {
        if let Some(el) = input_ref.get_untracked() {
            let r = el.get_bounding_client_rect();
            menu_pos.set((r.left(), r.bottom() + 3.0, r.width()));
        }
    };
    // Keep the list pinned to the input when the viewport reflows under it.
    let resize_handle = window_event_listener(leptos::ev::resize, move |_| {
        if open.get_untracked() {
            measure();
        }
    });
    on_cleanup(move || resize_handle.remove());

    // Re-seed the draft when the committed value changes from the outside (a loaded
    // record, a switched conversation) — but never while focused, so an in-flight
    // edit isn't clobbered by the value our own commit just produced.
    Effect::new(move |_| {
        let v = value.get();
        if !focused.get_untracked() {
            draft.set(v);
        }
    });

    let filtered = Signal::derive(move || {
        let opts = options.get();
        if !dirty.get() {
            return opts;
        }
        let q = draft.get().trim().to_lowercase();
        if q.is_empty() {
            return opts;
        }
        opts.into_iter()
            .filter(|(id, label)| {
                id.to_lowercase().contains(&q) || label.to_lowercase().contains(&q)
            })
            .collect()
    });

    let commit = move |id: String| {
        draft.set(id.clone());
        on_commit(id);
        open.set(false);
        dirty.set(false);
        active.set(None);
    };

    view! {
        <div class="ac-wrap">
            <input
                node_ref=input_ref
                class=input_class
                type="text"
                autocomplete="off"
                placeholder=move || placeholder.get()
                disabled=move || disabled.get()
                prop:value=move || draft.get()
                on:focus=move |_| {
                    focused.set(true);
                    open.set(true);
                    dirty.set(false);
                    active.set(None);
                    measure();
                }
                on:blur=move |_| {
                    focused.set(false);
                    open.set(false);
                    let d = draft.get_untracked();
                    if d != value.get_untracked() {
                        on_commit(d);
                    }
                    dirty.set(false);
                    active.set(None);
                }
                on:input=move |ev| {
                    draft.set(event_target_value(&ev));
                    open.set(true);
                    dirty.set(true);
                    active.set(Some(0));
                    measure();
                }
                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                    match ev.key().as_str() {
                        "ArrowDown" => {
                            ev.prevent_default();
                            if !open.get_untracked() {
                                measure();
                            }
                            open.set(true);
                            let n = filtered.get_untracked().len();
                            if n > 0 {
                                active
                                    .update(|a| {
                                        *a = Some(match *a {
                                            Some(i) => (i + 1).min(n - 1),
                                            None => 0,
                                        });
                                    });
                            }
                        }
                        "ArrowUp" => {
                            ev.prevent_default();
                            active
                                .update(|a| {
                                    *a = match *a {
                                        Some(0) | None => None,
                                        Some(i) => Some(i - 1),
                                    };
                                });
                        }
                        "Enter" if open.get_untracked() => {
                            let opts = filtered.get_untracked();
                            if let Some((id, _)) = active.get_untracked().and_then(|i| opts.get(i)) {
                                ev.prevent_default();
                                commit(id.clone());
                            }
                        }
                        "Escape" => {
                            ev.prevent_default();
                            draft.set(value.get_untracked());
                            open.set(false);
                            dirty.set(false);
                            active.set(None);
                        }
                        _ => {}
                    }
                }
            />
            <Show
                when=move || open.get() && !filtered.get().is_empty()
                fallback=|| ().into_view()
            >
                <Portal>
                <ul
                    class="ac-list"
                    style=move || {
                        let (l, t, w) = menu_pos.get();
                        format!("left:{l}px;top:{t}px;width:{w}px;")
                    }
                >
                    {move || {
                        let act = active.get();
                        let cur = value.get();
                        filtered
                            .get()
                            .into_iter()
                            .enumerate()
                            .map(|(i, (id, label))| {
                                let id_click = id.clone();
                                let is_active = act == Some(i);
                                let is_current = id == cur;
                                view! {
                                    <li
                                        class="ac-item"
                                        class:ac-item-active=is_active
                                        class:ac-item-current=is_current
                                        // Keep focus on the input so its blur doesn't
                                        // fire (and close the list) before the click.
                                        on:mousedown=move |ev: leptos::ev::MouseEvent| {
                                            ev.prevent_default()
                                        }
                                        on:click=move |_| commit(id_click.clone())
                                    >
                                        {label}
                                    </li>
                                }
                            })
                            .collect::<Vec<_>>()
                    }}
                </ul>
                </Portal>
            </Show>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Attachment references (SOUL §9) — shared across calendar events and chat
// messages. Both surfaces carry the same [`Attachment`] shape (an uploaded
// object in the files store, or an external/synced link) and must resolve it to
// a fetchable href the same safe way; these helpers are the single source of
// that logic (notably the `is_safe_href` XSS gate and the token plumbing), so no
// panel hand-rolls its own.

/// The last path segment of a URL (its filename), query/fragment stripped.
pub fn url_basename(url: &str) -> String {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    no_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// A display name for an attachment: its filename, else the URL's basename, else
/// the raw URL.
pub fn attachment_label(att: &Attachment) -> String {
    att.filename
        .clone()
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| {
            let base = url_basename(&att.url);
            if base.is_empty() {
                att.url.clone()
            } else {
                base
            }
        })
}

/// Whether an attachment should render as an inline image: an `image/*` content
/// type, or (when the type is unknown) an image-looking filename/URL extension.
pub fn attachment_is_image(att: &Attachment) -> bool {
    if att
        .content_type
        .as_deref()
        .is_some_and(|t| t.to_lowercase().starts_with("image/"))
    {
        return true;
    }
    let name = attachment_label(att).to_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Resolve an attachment's `url` to a fetchable href. An uploaded file is stored
/// as a relative `/storage/objects/{key}` path; rebuild it through
/// [`rest::download_url`] so the bearer token rides along (a plain `<img>`/anchor
/// can't set an `Authorization` header). Absolute URLs pass through unchanged.
pub fn attachment_href(att: &Attachment) -> String {
    const STORAGE_PREFIX: &str = "/storage/objects/";
    if let Some(key) = att.url.strip_prefix(STORAGE_PREFIX) {
        let token = auth::resolve_token();
        rest::download_url(token.as_deref(), key, None)
    } else {
        att.url.clone()
    }
}

/// Whether an attachment href is safe to place in an `href` / `src`. Allows only
/// our own storage paths (relative `/…`), absolute `http(s)`, and inline
/// `data:image/*` thumbnails — blocking `javascript:`, `data:text/html`,
/// `vbscript:`, and other schemes that could execute script when a pasted
/// attachment link is clicked (the URL is user-supplied, so this is the XSS gate).
pub fn is_safe_href(href: &str) -> bool {
    let h = href.trim();
    let lower = h.to_ascii_lowercase();
    h.starts_with('/')
        || lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("data:image/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_out_of_catalog_appends_only_missing_in_order() {
        let catalog = vec![
            ("a".to_string(), "a".to_string(), None),
            ("b".to_string(), "b".to_string(), Some("hint".to_string())),
        ];
        // "b" is already in the catalog (not re-added); "z" then "y" are missing,
        // appended in selection order with the extra hint.
        let out = with_out_of_catalog(
            catalog,
            &["b".to_string(), "z".to_string(), "y".to_string()],
            "gone",
        );
        let values: Vec<&str> = out.iter().map(|(v, _, _)| v.as_str()).collect();
        assert_eq!(values, ["a", "b", "z", "y"]);
        assert_eq!(out[2].2.as_deref(), Some("gone"));
        assert_eq!(out[3].2.as_deref(), Some("gone"));
        // The catalog's own hint is untouched.
        assert_eq!(out[1].2.as_deref(), Some("hint"));
    }

    #[test]
    fn with_out_of_catalog_noop_when_all_known() {
        let catalog = vec![("a".to_string(), "a".to_string(), None)];
        let out = with_out_of_catalog(catalog, &["a".to_string()], "gone");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn with_out_of_catalog_no_hint_when_catalog_empty() {
        // An empty catalog may merely be unloaded — selected entries are still
        // shown but NOT labeled "missing" (so a real entry isn't mislabeled while
        // its catalog is loading).
        let out = with_out_of_catalog(Vec::new(), &["x".to_string(), "y".to_string()], "gone");
        let values: Vec<&str> = out.iter().map(|(v, _, _)| v.as_str()).collect();
        assert_eq!(values, ["x", "y"]);
        assert!(out.iter().all(|(_, _, h)| h.is_none()));
    }

    fn attach(url: &str, content_type: Option<&str>) -> Attachment {
        Attachment {
            url: url.to_string(),
            filename: None,
            content_type: content_type.map(str::to_string),
            size: None,
        }
    }

    #[test]
    fn attachment_image_detection_by_type_or_extension() {
        assert!(attachment_is_image(&attach("/x/a.bin", Some("image/png"))));
        assert!(attachment_is_image(&attach("https://x/pic.JPG", None))); // by extension, case-insensitive
        assert!(!attachment_is_image(&attach(
            "https://x/doc.pdf",
            Some("application/pdf")
        )));
    }

    #[test]
    fn url_basename_and_label_fallbacks() {
        assert_eq!(
            url_basename("https://x/y/file.pdf?token=1#frag"),
            "file.pdf"
        );
        assert_eq!(url_basename("https://x/dir/"), "dir");
        let mut named = attach("https://x/floor.png", None);
        named.filename = Some("Report.pdf".to_string());
        assert_eq!(attachment_label(&named), "Report.pdf");
        assert_eq!(
            attachment_label(&attach("https://x/floor.png", None)),
            "floor.png"
        );
    }

    #[test]
    fn is_safe_href_blocks_script_schemes() {
        assert!(is_safe_href("/storage/objects/events/x.png?token=abc"));
        assert!(is_safe_href("https://example.com/a.png"));
        assert!(is_safe_href("http://example.com/a.pdf"));
        assert!(is_safe_href("data:image/png;base64,AAAA"));
        assert!(!is_safe_href("javascript:alert(1)"));
        assert!(!is_safe_href("  JavaScript:alert(1)"));
        assert!(!is_safe_href("data:text/html,<script>alert(1)</script>"));
        assert!(!is_safe_href("vbscript:msgbox(1)"));
    }
}
