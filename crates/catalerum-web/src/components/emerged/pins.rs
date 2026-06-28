//! Pinned apps — the localStorage-backed pin set behind the nav's quick menu.
//!
//! A pin is a tiny `{id, title, workspace}` record cached in `localStorage`
//! (like the theme choice — a per-browser convenience, no server state). The
//! workbench nav renders the active workspace's pins as a flyout beside the
//! Apps entry; the Apps panel toggles them per row. Storage keeps every
//! workspace's pins side by side; [`reconcile_workspace`] projects out the
//! current workspace's slice, refreshes stale titles, and drops pins whose app
//! has been deleted — other workspaces' pins are never touched (their apps are
//! simply absent from this workspace's `GET /uis`).

use leptos::prelude::*;
use serde::{Deserialize, Serialize};

use super::model::UiDefinition;

/// `localStorage` key under which the pin list is cached (a JSON
/// `Vec<PinnedApp>`, all workspaces mixed).
const PINS_STORAGE_KEY: &str = "catalerum-pinned-apps";

/// One pinned app: just enough to render a quick-menu row without a fetch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedApp {
    /// The emerged UI's id.
    pub id: String,
    /// Display title as of pin time (refreshed by [`reconcile_workspace`]).
    pub title: String,
    /// Owning workspace — scopes the nav quick menu to the active workspace.
    pub workspace: String,
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

/// Every stored pin, across workspaces (empty on absence or parse failure).
#[must_use]
pub fn load_all() -> Vec<PinnedApp> {
    storage()
        .and_then(|s| s.get_item(PINS_STORAGE_KEY).ok().flatten())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Persist the full pin list (best-effort, like the theme cache).
fn save_all(pins: &[PinnedApp]) {
    if let (Some(storage), Ok(json)) = (storage(), serde_json::to_string(pins)) {
        let _ = storage.set_item(PINS_STORAGE_KEY, &json);
    }
}

/// Pin `app` if unpinned, else unpin it — updating both the shared signal (so
/// the nav re-renders) and `localStorage` (where other workspaces' pins are
/// preserved; the signal only ever holds the current workspace's slice).
pub fn toggle(pins: RwSignal<Vec<PinnedApp>>, app: &UiDefinition) {
    let mut all = load_all();
    if all.iter().any(|p| p.id == app.id) {
        all.retain(|p| p.id != app.id);
        pins.update(|v| v.retain(|p| p.id != app.id));
    } else {
        let pin = PinnedApp {
            id: app.id.clone(),
            title: app.display_title(),
            workspace: app.workspace_id.clone(),
        };
        all.push(pin.clone());
        pins.update(|v| {
            if !v.iter().any(|p| p.id == pin.id) {
                v.push(pin);
            }
        });
    }
    save_all(&all);
}

/// Drop any pin for app `id` from both the shared signal and `localStorage` —
/// the Apps panel calls this right after deleting an app, so the nav quick
/// menu never dangles until the next [`reconcile_workspace`] (which cannot
/// drop the pin at all once the workspace's app list is empty). A no-op when
/// the app was never pinned.
pub fn remove(pins: RwSignal<Vec<PinnedApp>>, id: &str) {
    let mut all = load_all();
    if all.iter().any(|p| p.id == id) {
        all.retain(|p| p.id != id);
        save_all(&all);
    }
    pins.update(|v| v.retain(|p| p.id != id));
}

/// Reconcile stored pins against a workspace's full app list: refresh titles,
/// drop pins whose app no longer exists, leave other workspaces' pins alone.
/// Returns the current workspace's pins in pin order — the value for the
/// shared nav signal.
#[must_use]
pub fn reconcile_workspace(apps: &[UiDefinition]) -> Vec<PinnedApp> {
    let (all, mine, changed) = reconcile_in(load_all(), apps);
    if changed {
        save_all(&all);
    }
    mine
}

/// Pure core of [`reconcile_workspace`]: `(updated full list, this workspace's
/// slice, whether the full list changed)`. An empty `apps` list cannot identify
/// the workspace, so it shows nothing and drops nothing.
fn reconcile_in(
    mut all: Vec<PinnedApp>,
    apps: &[UiDefinition],
) -> (Vec<PinnedApp>, Vec<PinnedApp>, bool) {
    let Some(ws) = apps.first().map(|a| a.workspace_id.clone()) else {
        return (all, Vec::new(), false);
    };
    let mut changed = false;
    all.retain_mut(|pin| {
        if pin.workspace != ws {
            return true;
        }
        match apps.iter().find(|a| a.id == pin.id) {
            Some(app) => {
                let title = app.display_title();
                if pin.title != title {
                    pin.title = title;
                    changed = true;
                }
                true
            }
            None => {
                changed = true;
                false
            }
        }
    });
    let mine = all.iter().filter(|p| p.workspace == ws).cloned().collect();
    (all, mine, changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app(id: &str, ws: &str, title: &str) -> UiDefinition {
        serde_json::from_value(json!({
            "id": id,
            "workspace_id": ws,
            "author": { "kind": "user", "id": "u" },
            "title": title,
            "definition": {
                "default_view": "v",
                "views": [{ "id": "v", "title": "V", "root": { "id": "r", "kind": "text" } }]
            }
        }))
        .expect("test app decodes")
    }

    fn pin(id: &str, ws: &str, title: &str) -> PinnedApp {
        PinnedApp {
            id: id.into(),
            title: title.into(),
            workspace: ws.into(),
        }
    }

    #[test]
    fn reconcile_refreshes_titles_drops_deleted_keeps_other_workspaces() {
        let all = vec![
            pin("a", "ws1", "Old title"), // renamed since pinning
            pin("b", "ws1", "Gone"),      // app deleted
            pin("c", "ws2", "Elsewhere"), // another workspace — untouched
        ];
        let apps = vec![app("a", "ws1", "New title")];
        let (all, mine, changed) = reconcile_in(all, &apps);
        assert!(changed);
        assert_eq!(mine, vec![pin("a", "ws1", "New title")]);
        assert_eq!(
            all,
            vec![pin("a", "ws1", "New title"), pin("c", "ws2", "Elsewhere")]
        );
    }

    #[test]
    fn reconcile_no_change_reports_unchanged() {
        let all = vec![pin("a", "ws1", "Same")];
        let apps = vec![app("a", "ws1", "Same"), app("x", "ws1", "Unpinned")];
        let (_, mine, changed) = reconcile_in(all, &apps);
        assert!(!changed);
        assert_eq!(mine.len(), 1);
    }

    #[test]
    fn reconcile_empty_list_shows_nothing_drops_nothing() {
        let all = vec![pin("a", "ws1", "Kept")];
        let (all, mine, changed) = reconcile_in(all, &[]);
        assert!(!changed);
        assert!(mine.is_empty());
        assert_eq!(all.len(), 1);
    }
}
