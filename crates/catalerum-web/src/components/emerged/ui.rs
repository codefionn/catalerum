//! The [`EmergedUi`] component — the inline mount point for one emerged UI.
//!
//! Given a `ui_id`, it loads the latest definition (via `GET /uis/{id}`), seeds a
//! fresh [`UiState`] from `initial_state`/`default_view`, and renders the active
//! view. Switching views is pure client state; nothing here round-trips.
//!
//! Each mount fetches fresh — there is deliberately no definition cache. A keyed
//! `<For>` mounts each chat line's child exactly once, and the call site keys its
//! mount `Memo` on `(ui_id, version)`, so a re-present/edit (same id, bumped
//! version) tears down this component and remounts it, re-fetching the new
//! definition instead of showing a stale one.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value as Json;

use super::handlers;
use super::model::{EventName, Handler, UiDefinition, UiNode};
use super::path::Scope;
use super::render::render_node;
use super::state::{UiState, MAX_APP_DEPTH};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

#[derive(Clone)]
enum Load {
    Loading,
    // Boxed: `UiDefinition` is far larger than the other variants, so an unboxed
    // `Ready` would bloat every `Load` (incl. the common `Loading`) — `clippy::large_enum_variant`.
    Ready(Box<UiDefinition>),
    Failed(String),
}

/// Render the emerged UI identified by `ui_id` inline (in a chat line, or later
/// the Apps panel).
#[component]
pub fn EmergedUi(
    /// The UI to load and render: its stable id (UUID string) or, from an
    /// `app_ref` targeting a name, its `present_ui` name slug.
    ui_id: String,
    /// Sink for `ai` handlers — runs the given text as a new chat turn. `None`
    /// (e.g. the future Apps panel) leaves `ai` handlers showing a notice.
    #[prop(optional)]
    ai_sink: Option<UnsyncCallback<String>>,
    /// The `app_ref` mount chain (ancestor ui ids) when this UI is embedded as
    /// a sub-app of a shell — the cycle/depth guard. Empty for a top mount.
    #[prop(optional)]
    chain: Vec<String>,
) -> impl IntoView {
    let load = RwSignal::new(Load::Loading);

    let fetch_ref = ui_id;
    spawn_local(async move {
        let token = auth::resolve_token();
        let result = if is_uuid_like(&fetch_ref) {
            rest::get_ui(token.as_deref(), &fetch_ref).await
        } else {
            rest::get_ui_by_name(token.as_deref(), &fetch_ref).await
        };
        match result {
            Ok(def) => load.set(Load::Ready(Box::new(def))),
            Err(e) => load.set(Load::Failed(e.to_string())),
        }
    });

    view! {
        <div class="eu-artifact">
            {move || match load.get() {
                Load::Loading => {
                    view! { <div class="eu-loading">"Loading UI…"</div> }.into_any()
                }
                Load::Failed(e) => {
                    view! { <div class="eu-load-error">"Could not load UI: " {e}</div> }.into_any()
                }
                Load::Ready(def) => {
                    // Cycle/depth guard on the RESOLVED id — a name-form
                    // `app_ref` can only be checked once the server told us
                    // which App it is.
                    if chain.iter().any(|a| a == &def.id) || chain.len() > MAX_APP_DEPTH {
                        return view! {
                            <div class="eu-load-error">
                                "This app embeds itself (or nests too deeply)."
                            </div>
                        }
                        .into_any();
                    }
                    render_definition(ai_sink, chain.clone(), *def)
                }
            }}
        </div>
    }
}

/// Whether a mount reference looks like a ui id (a hyphenated hex UUID) rather
/// than a `present_ui` name slug — picks `GET /uis/{id}` vs `/uis/by-name/{n}`.
fn is_uuid_like(s: &str) -> bool {
    let s = s.trim();
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Seed transient state (carrying the resolved ui id + the `ai_sink` for
/// handler round-trips) from the spec and render its active view + chrome.
fn render_definition(
    ai_sink: Option<UnsyncCallback<String>>,
    chain: Vec<String>,
    def: UiDefinition,
) -> AnyView {
    // Always the RESOLVED id (a name-form mount still posts events by id).
    let ui_id = def.id.clone();
    let version = def.version;
    let spec = def.definition;
    let mut st = UiState::seed(
        ui_id.clone(),
        ai_sink,
        Json::Object(spec.initial_state.clone()),
        spec.default_view.clone(),
    );
    // Spec-derived extras: live-compute flag, the views for `view_ref`
    // composition, and the `app_ref` mount chain (ancestors + self).
    st.has_computed = !spec.computed.is_empty();
    st.views.set_value(spec.views.clone());
    let mut full_chain = chain;
    full_chain.push(ui_id);
    st.app_chain.set_value(full_chain);
    let st = st;

    // Derive the initial `computed.*` values (SOUL §12, Option A); later refreshes
    // arrive as a `set computed` action on each handler response.
    if !spec.computed.is_empty() {
        let compute_id = st.ui_id.get_value();
        let initial = st.snapshot();
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(computed) =
                rest::post_ui_compute(token.as_deref(), &compute_id, &initial).await
            {
                st.set_computed(computed);
            }
        });
    }

    // Fire a view root's `load` handler when its view becomes active — once on
    // mount and again on each navigate-to (the App lifecycle seam: a root-level
    // `load` → tool/script handler pulls durable data, e.g. `app_data_list`,
    // into state so the App opens populated). The previous-value guard keeps a
    // re-set of the same view id from double-firing. The MOUNT fire (identical
    // seeded state on every copy of the same version) goes through the
    // deduplicating `dispatch_load`, so a replayed chat that remounts the same
    // presented UI N times shares one round trip; navigate-to fires — whose
    // state has diverged per mount — dispatch normally.
    {
        let views = spec.views.clone();
        Effect::new(move |prev: Option<String>| {
            let vid = st.view.get();
            if prev.as_deref() != Some(vid.as_str()) {
                let root = views
                    .iter()
                    .find(|v| v.id == vid)
                    .or_else(|| views.first())
                    .map(|v| &v.root);
                if let Some(root) = root {
                    if let Some((node_id, h)) = view_load_handler(root) {
                        if prev.is_none() {
                            handlers::dispatch_load(st, version, node_id, h);
                        } else {
                            handlers::dispatch(st, &Scope::default(), node_id, EventName::Load, h);
                        }
                    }
                }
            }
            vid
        });
    }

    let views = spec.views;
    let title = def.title;

    let active = move || {
        let vid = st.view.get();
        let root = views
            .iter()
            .find(|v| v.id == vid)
            .or_else(|| views.first())
            .map(|v| v.root.clone());
        match root {
            Some(root) => render_node(root, st, Scope::default(), 0),
            None => view! { <div class="eu-load-error">"This UI has no views."</div> }.into_any(),
        }
    };

    let notice = move || {
        st.notice.get().map(|msg| {
            view! {
                <div class="eu-notice">
                    <span class="eu-notice-msg">{msg}</span>
                    <button
                        class="eu-notice-x"
                        type="button"
                        on:click=move |_| st.notice.set(None)
                    >
                        <Icon icon=MdIcon::Close />
                    </button>
                </div>
            }
        })
    };

    view! {
        <div class="eu-app">
            <div class="eu-app-head">{title}</div>
            {notice}
            <div class="eu-app-body">{active}</div>
        </div>
    }
    .into_any()
}

/// Resolve the lifecycle handler for a view.
///
/// New specs are validated to keep `load` on the view root. Older staged Apps
/// could nevertheless persist a `load` on the one shell stack inserted directly
/// below that root because the original validator checked only the node kind.
/// Honor that single, unambiguous legacy shape so those Apps populate after an
/// upgrade. We deliberately do not walk deeper or choose among multiple child
/// handlers: those shapes have no well-defined view lifecycle or loop scope.
fn view_load_handler(root: &UiNode) -> Option<(&str, &Handler)> {
    if let Some(handler) = root.events.get(&EventName::Load) {
        return Some((&root.id, handler));
    }

    let mut found = None;
    for child in &root.children {
        if let Some(handler) = child.events.get(&EventName::Load) {
            if found.is_some() {
                return None;
            }
            found = Some((child.id.as_str(), handler));
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Map;

    use super::{is_uuid_like, view_load_handler};
    use crate::components::emerged::model::{EventName, Handler, NodeKind, UiNode};

    fn node(id: &str) -> UiNode {
        UiNode {
            id: id.to_string(),
            kind: NodeKind::Stack,
            props: Map::new(),
            children: Vec::new(),
            bind: None,
            show_if: None,
            for_each: None,
            events: BTreeMap::new(),
            validate: Vec::new(),
        }
    }

    fn add_load(node: &mut UiNode) {
        node.events.insert(
            EventName::Load,
            Handler::Tool {
                tool: "sql_query".to_string(),
                args: Map::new(),
                result_path: Some("rows".to_string()),
                then: Vec::new(),
            },
        );
    }

    #[test]
    fn uuid_like_vs_name_slug() {
        assert!(is_uuid_like("11111111-1111-1111-1111-111111111111"));
        assert!(is_uuid_like("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"));
        // Name slugs (and near-misses) route to the by-name fetch.
        for name in [
            "recipes-editor",
            "recipes",
            "11111111-1111-1111-1111-11111111111",  // 35 chars
            "g1111111-1111-1111-1111-111111111111", // non-hex
            "11111111x1111-1111-1111-111111111111", // wrong separator
            "",
        ] {
            assert!(!is_uuid_like(name), "{name:?} must not look like a uuid");
        }
    }

    #[test]
    fn root_load_wins_and_legacy_single_child_load_is_supported() {
        let mut root = node("root");
        let mut shell = node("shell");
        add_load(&mut shell);
        root.children.push(shell);

        assert_eq!(view_load_handler(&root).map(|(id, _)| id), Some("shell"));

        add_load(&mut root);
        assert_eq!(view_load_handler(&root).map(|(id, _)| id), Some("root"));
    }

    #[test]
    fn ambiguous_legacy_child_loads_do_not_fire() {
        let mut root = node("root");
        for id in ["one", "two"] {
            let mut child = node(id);
            add_load(&mut child);
            root.children.push(child);
        }

        assert!(view_load_handler(&root).is_none());
    }
}
