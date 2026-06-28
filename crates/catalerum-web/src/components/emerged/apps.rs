//! The Apps panel — a standalone browser for the workspace's emerged UIs.
//!
//! Lists every emerged UI (`GET /uis`) in a sidebar and renders the selected one
//! via [`EmergedUi`] in the stage. Unlike the inline-in-chat mount, this surface
//! passes **no** `ai_sink`, so an `ai` handler shows a notice rather than starting
//! a chat turn (there is no conversation here). Tool/script handlers still work —
//! they round-trip to the server under the user's own grant.
//!
//! Each row carries a pin toggle feeding the nav's pinned-apps quick menu (see
//! [`super::pins`]) and a confirm-gated delete (`DELETE /uis/{id}`); the
//! selected app is deep-linkable at `/app/apps/<id>` and the nav quick menu
//! lands here through the one-shot `target` signal (the same pattern as the
//! History panel's resume-in-Chat handoff).

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use super::model::UiDefinition;
use super::pins::{self, PinnedApp};
use super::EmergedUi;
use crate::components::dialogs::{use_dialogs, ConfirmSpec};
use crate::components::icons::MdIcon;
use crate::components::widgets::{list_drawer_scrim, list_drawer_toggle, row_action};
use crate::{auth, rest};

/// The Apps panel's base frontend route; a selected app is deep-linkable at
/// `<APPS_ROUTE>/<ui-id>`.
const APPS_ROUTE: &str = "/app/apps";

/// The app id encoded in the current browser URL (`/app/apps/<id>`), if any.
/// Drives the initial selection so a deep link or reload lands on that app.
fn app_from_location() -> Option<String> {
    let path = web_sys::window()?.location().pathname().ok()?;
    let segment = path
        .trim_end_matches('/')
        .strip_prefix(APPS_ROUTE)?
        .trim_start_matches('/');
    (!segment.is_empty()).then(|| segment.to_string())
}

/// Reflect the selected app in the URL as `/app/apps/<id>` — or the bare panel
/// route when nothing is selected (e.g. after deleting the last app) — via
/// `replace_state` (selection switches stay out of the history stack, like the
/// calendar's views). No-op when the URL already matches.
fn sync_location_to_app(id: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match id {
        Some(id) => format!("{APPS_ROUTE}/{id}"),
        None => APPS_ROUTE.to_string(),
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

/// The Apps workbench panel (SOUL §12).
#[component]
pub fn AppsPanel(
    /// The current workspace's pinned apps, shared with the nav quick menu.
    pins: RwSignal<Vec<PinnedApp>>,
    /// One-shot "open this app" request from the nav quick menu; consumed here.
    target: RwSignal<Option<String>>,
) -> impl IntoView {
    let apps = RwSignal::new(Vec::<UiDefinition>::new());
    let selected = RwSignal::new(app_from_location());
    let error = RwSignal::new(Option::<String>::None);
    let loading = RwSignal::new(true);
    let dialogs = use_dialogs();
    // Whether the apps list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    // Consume nav quick-menu requests — fires on mount and again on later
    // clicks while the panel stays mounted.
    Effect::new(move |_| {
        if let Some(id) = target.get() {
            selected.set(Some(id));
            target.set(None);
        }
    });

    // Keep the URL tracking the selected app (deep-linkable, reload-safe).
    Effect::new(move |_| {
        selected.with(|sel| sync_location_to_app(sel.as_deref()));
    });

    spawn_local(async move {
        let token = auth::resolve_token();
        match rest::list_uis(token.as_deref()).await {
            Ok(list) => {
                // Sub-apps of a shell suite (spec carries `parent_app`) render
                // inside their shell's `app_ref` — hide them from the list.
                let list: Vec<UiDefinition> = list
                    .into_iter()
                    .filter(|app| app.definition.parent_app.is_none())
                    .collect();
                // The live list is known: refresh pin titles and drop pins
                // whose app has been deleted.
                pins.set(pins::reconcile_workspace(&list));
                // Keep a URL/quick-menu preselection when it resolves; else
                // open the most-recently-edited app.
                let preselected = selected.with_untracked(|sel| {
                    sel.as_deref()
                        .is_some_and(|id| list.iter().any(|app| app.id == id))
                });
                if !preselected {
                    selected.set(list.first().map(|app| app.id.clone()));
                }
                apps.set(list);
            }
            Err(e) => error.set(Some(e.to_string())),
        }
        loading.set(false);
    });

    let rows = move || {
        let pinned_ids: Vec<String> = pins.with(|p| p.iter().map(|p| p.id.clone()).collect());
        apps.get()
            .into_iter()
            .map(|app| {
                let id = app.id.clone();
                let row_id = app.id.clone();
                let is_active = move || selected.get().as_deref() == Some(row_id.as_str());
                let class = move || {
                    if is_active() {
                        "apps-item apps-item-active"
                    } else {
                        "apps-item"
                    }
                };
                let title = app.display_title();
                let is_pinned = pinned_ids.contains(&app.id);
                let pin_class = if is_pinned {
                    "apps-pin apps-pin-on"
                } else {
                    "apps-pin"
                };
                let pin_hint = if is_pinned {
                    "Unpin from the nav quick menu"
                } else {
                    "Pin to the nav quick menu"
                };
                let pin_glyph = if is_pinned { "★" } else { "☆" };
                // Deleting is confirm-gated (the shared dialog, never
                // `window.confirm`); on success the row, its pin, and a
                // now-dangling selection are pruned locally — no refetch.
                let del_id = app.id.clone();
                let del_title = title.clone();
                let on_delete = move || {
                    let del_id = del_id.clone();
                    dialogs.confirm(
                        ConfirmSpec::danger(
                            "Delete app?",
                            format!(
                                "Delete the app “{del_title}” for everyone in this \
                                 workspace? This cannot be undone."
                            ),
                            "Delete",
                        ),
                        move || {
                            let del_id = del_id.clone();
                            spawn_local(async move {
                                let token = auth::resolve_token();
                                match rest::delete_ui(token.as_deref(), &del_id).await {
                                    Ok(()) => {
                                        apps.update(|list| list.retain(|a| a.id != del_id));
                                        pins::remove(pins, &del_id);
                                        if selected.get_untracked().as_deref()
                                            == Some(del_id.as_str())
                                        {
                                            selected.set(apps.with_untracked(|list| {
                                                list.first().map(|a| a.id.clone())
                                            }));
                                        }
                                        error.set(None);
                                    }
                                    Err(e) => error.set(Some(e.to_string())),
                                }
                            });
                        },
                    );
                };
                view! {
                    <li class="apps-row">
                        <button
                            class=class
                            on:click=move |_| {
                                selected.set(Some(id.clone()));
                                list_open.set(false);
                            }
                        >
                            {title}
                        </button>
                        <button
                            class=pin_class
                            title=pin_hint
                            aria-pressed=is_pinned.to_string()
                            on:click=move |_| pins::toggle(pins, &app)
                        >
                            {pin_glyph}
                        </button>
                        <span class="row-acts row-acts-reveal">
                            {row_action(MdIcon::Delete, "Delete this app", true, on_delete)}
                        </span>
                    </li>
                }
            })
            .collect::<Vec<_>>()
    };

    let empty = move || !loading.get() && apps.with(Vec::is_empty) && error.with(Option::is_none);

    view! {
        <div class="pane-split apps-panel">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list apps-sidebar list-drawer" class:list-drawer-open=move || list_open.get()>
                <div class="apps-sidebar-head">"Apps"</div>
                {move || {
                    error
                        .get()
                        .map(|e| view! { <div class="apps-error">{e}</div> }.into_any())
                }}
                <Show when=empty fallback=|| ().into_view()>
                    <div class="apps-empty-list">
                        "No apps yet — ask the assistant to build one."
                    </div>
                </Show>
                <ul class="apps-list">{rows}</ul>
            </aside>
            {list_drawer_toggle("Apps", list_open)}
            <section class="apps-stage">
                {move || match selected.get() {
                    Some(id) => view! { <EmergedUi ui_id=id /> }.into_any(),
                    None => {
                        view! { <div class="apps-placeholder">"Select an app."</div> }.into_any()
                    }
                }}
            </section>
        </div>
    }
}
