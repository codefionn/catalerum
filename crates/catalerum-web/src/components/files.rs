//! The Files panel (SOUL §9, §12 — M3 object-storage browser).
//!
//! A single-pane workbench panel browsing the selected storage backend's **actual
//! filesystem** (`GET /storage/objects`) as an expandable directory **tree** — so a
//! *browse* store pointed at an existing directory (e.g. `~/Documents`) shows the
//! files already on disk, not just what catalerum uploaded. It offers a key-prefix
//! filter, a store switcher, **upload**, a per-object **download** link, and
//! **delete** — full CRUD over the §9 storage surface. It is a thin client of the
//! storage REST routes; every call carries the dev session token and is
//! workspace-scoped server-side (SOUL §18).
//!
//! Each backend file is enriched from the *catalogue* (`GET /storage/catalogue`,
//! Postgres truth) by matching keys: where a catalogue row exists, the file shows
//! its "Indexed ✓" badge and opens its §10 extracted text. Downloads go to the
//! blob backend (`GET /storage/objects/{key}`); the token rides as a `?token=`
//! query parameter since a browser can't set an `Authorization` header on a plain
//! anchor navigation (see [`crate::rest::download_url`]). Uploads read the picked
//! `File`'s bytes and `PUT` them under the current prefix (which doubles as the
//! destination folder); the server catalogues + ingests (§10) + fires the
//! `StorageObject` trigger (§11) on arrival. The backend listing is bounded at
//! `DEFAULT_OBJECT_LIMIT` (1000) objects — a truncated tree says so.

use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::JsCast;

use super::dialogs::{use_dialogs, PromptSpec};
use crate::api::{BackendObject, FileLabel, NewFileLabel, ObjectHit, ObjectText, StorageStore};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// The server-side cap on a backend listing (`DEFAULT_OBJECT_LIMIT`). When a fetch
/// returns exactly this many objects the tree is (probably) truncated, and the
/// panel says so rather than silently implying it showed everything.
const BACKEND_LIST_LIMIT: usize = 1000;

/// The Files panel component.
#[component]
pub fn FilesPanel() -> impl IntoView {
    // The shared prompt dialog (replaces the native "add label" window.prompt).
    let dialogs = use_dialogs();
    // The directory tree (dirs + files, key-sorted) built from the selected store's
    // backend listing, the set of expanded directory paths, whether the listing was
    // truncated at the server cap, and the load state.
    let rows = RwSignal::new(Vec::<TreeRow>::new());
    let expanded = RwSignal::new(HashSet::<String>::new());
    let truncated = RwSignal::new(false);
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // The active key-prefix filter (applied server-side; doubles as the upload
    // destination folder).
    let prefix = RwSignal::new(String::new());

    // A mutating action (delete) in flight, and its error.
    let busy = RwSignal::new(false);
    let action_error = RwSignal::new(Option::<String>::None);

    // An upload in flight, and its error.
    let uploading = RwSignal::new(false);
    let upload_error = RwSignal::new(Option::<String>::None);

    // The workspace's storage backends + the picked store (the `?store=` value).
    // It drives BOTH which backend the tree browses and where an upload lands.
    // `selected_store` defaults to the default store; with one store the picker is
    // hidden — there's nothing to switch between (SOUL §9).
    let stores = RwSignal::new(Vec::<StorageStore>::new());
    let selected_store = RwSignal::new(String::new());

    // Fetch the selected store's backend listing for the current prefix, enrich each
    // file with its catalogue row (for the Indexed badge + text view), build the
    // tree, and flag truncation. The store switcher + filter both call this.
    let refresh = move || {
        loading.set(true);
        load_error.set(None);
        let pfx = prefix.get_untracked();
        let store = selected_store.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            // The backend filesystem is the source of truth for *what's there*; the
            // catalogue (best-effort, same prefix) layers in the §10 badge by key.
            let cat = rest::list_catalogue(token.as_deref(), &pfx)
                .await
                .unwrap_or_default();
            let mut by_key: HashMap<String, (String, bool)> = HashMap::new();
            for o in cat {
                if store.is_empty() || o.store == store || o.store.is_empty() {
                    by_key.insert(o.key.clone(), (o.id.clone(), o.is_ingested()));
                }
            }
            match rest::list_objects(token.as_deref(), &store, &pfx).await {
                Ok(list) => {
                    truncated.set(list.len() >= BACKEND_LIST_LIMIT);
                    rows.set(build_rows(list, &by_key));
                    load_error.set(None);
                }
                Err(e) => {
                    rows.set(Vec::new());
                    truncated.set(false);
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    let load_stores = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_stores(token.as_deref()).await {
                // Keep the picked store valid: default it (to the default store, else
                // the first) when unset or when it no longer exists (e.g. the
                // selected store was just deleted), then browse it.
                let cur = selected_store.get_untracked();
                let still_there = list.iter().any(|s| s.name == cur);
                if cur.is_empty() || !still_there {
                    let pick = list
                        .iter()
                        .find(|s| s.is_default)
                        .or_else(|| list.first())
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    selected_store.set(pick);
                }
                stores.set(list);
                refresh();
            }
        });
    };
    load_stores();

    // --- Labels on files & directories (SOUL §9) -----------------------------
    // The selected store's labels (every labelled path), reloaded whenever the
    // store changes or a label is added/removed; each tree row filters this by its
    // own path to render its chips.
    let labels = RwSignal::new(Vec::<FileLabel>::new());
    let label_error = RwSignal::new(Option::<String>::None);

    let reload_labels = move || {
        let store = selected_store.get_untracked();
        spawn_local(async move {
            let token = auth::resolve_token();
            let list = rest::list_labels(token.as_deref(), &store, "")
                .await
                .unwrap_or_default();
            labels.set(list);
        });
    };
    // Reload labels whenever the picked store changes (and on first render).
    Effect::new(move |_| {
        let _ = selected_store.get();
        reload_labels();
    });

    // Add a label (to a file or a directory) via the shared prompt dialog, then
    // reload. The dialog hands back the trimmed, non-empty value.
    let add_label_prompt = move |path: String, is_dir: bool| {
        dialogs.prompt(
            PromptSpec::new("Add label", "Enter a label for this item.").placeholder("Label"),
            move |text| {
                let path = path.clone();
                let store = selected_store.get_untracked();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    let body = NewFileLabel {
                        store,
                        path,
                        is_dir,
                        label: text,
                    };
                    match rest::add_label(token.as_deref(), &body).await {
                        Ok(_) => {
                            label_error.set(None);
                            reload_labels();
                        }
                        Err(e) => label_error.set(Some(e.to_string())),
                    }
                });
            },
        );
    };

    // Remove a label by id, then reload.
    let remove_label = move |id: String| {
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_label(token.as_deref(), &id).await {
                Ok(()) => {
                    label_error.set(None);
                    reload_labels();
                }
                Err(e) => label_error.set(Some(e.to_string())),
            }
        });
    };

    // A path's label chips + an "add" button, shared by directory and file rows.
    // Clicks stop propagation so tagging a directory never toggles its expand.
    let label_strip = move |path: String, is_dir: bool| {
        let p_filter = path.clone();
        let p_add = path;
        view! {
            <span class="files-labels" on:click=move |ev| ev.stop_propagation()>
                {move || {
                    let p = p_filter.clone();
                    labels
                        .get()
                        .into_iter()
                        .filter(move |l| l.path == p)
                        .map(|l| {
                            let id = l.id.clone();
                            view! {
                                <span class="files-label-chip">
                                    <span class="files-label-text">{l.label.clone()}</span>
                                    <button
                                        class="files-label-x"
                                        type="button"
                                        title="Remove label"
                                        on:click=move |ev| {
                                            ev.stop_propagation();
                                            remove_label(id.clone());
                                        }
                                    >
                                        <Icon icon=MdIcon::Close />
                                    </button>
                                </span>
                            }
                        })
                        .collect_view()
                }}
                <button
                    class="files-label-add"
                    type="button"
                    title="Add a label"
                    on:click=move |ev| {
                        ev.stop_propagation();
                        add_label_prompt(p_add.clone(), is_dir);
                    }
                >
                    "+"
                </button>
            </span>
        }
    };

    // --- Storage manager (add / remove runtime backends, SOUL §9) ------------
    let manage_open = RwSignal::new(false);
    let manage_busy = RwSignal::new(false);
    let manage_error = RwSignal::new(Option::<String>::None);
    // The add-backend form: a name, a kind, and the per-kind fields.
    let new_name = RwSignal::new(String::new());
    let new_kind = RwSignal::new("local".to_string());
    let f_local_path = RwSignal::new(String::new());
    let f_s3_endpoint = RwSignal::new(String::new());
    let f_s3_region = RwSignal::new(String::new());
    let f_s3_access = RwSignal::new(String::new());
    let f_s3_secret = RwSignal::new(String::new());
    let f_s3_bucket = RwSignal::new(String::new());
    let f_s3_path_style = RwSignal::new(true);
    let f_dav_url = RwSignal::new(String::new());
    let f_dav_user = RwSignal::new(String::new());
    let f_dav_pass = RwSignal::new(String::new());
    // Browse mode for a local backend: expose its raw root (no per-workspace
    // namespacing) so an existing directory's files show up (SOUL §9/§18).
    let f_local_browse = RwSignal::new(false);
    // Watch mode (any backend): keep the §10 index in sync as files change
    // (real-time for local, periodic for remote, SOUL §9/§10).
    let f_watch = RwSignal::new(false);

    // Add a runtime backend from the form, then reload the store list + reset.
    let add_backend = move || {
        if manage_busy.get_untracked() {
            return;
        }
        let name = new_name.get_untracked().trim().to_string();
        if name.is_empty() {
            manage_error.set(Some("A backend name is required.".into()));
            return;
        }
        let kind = new_kind.get_untracked();
        let config = match kind.as_str() {
            "local" => serde_json::json!({
                "local_path": f_local_path.get_untracked(),
                "browse": f_local_browse.get_untracked(),
            }),
            "s3" => serde_json::json!({
                "endpoint": f_s3_endpoint.get_untracked(),
                "region": f_s3_region.get_untracked(),
                "access_key": f_s3_access.get_untracked(),
                "secret_key": f_s3_secret.get_untracked(),
                "bucket": f_s3_bucket.get_untracked(),
                "path_style": f_s3_path_style.get_untracked(),
            }),
            "webdav" => serde_json::json!({
                "url": f_dav_url.get_untracked(),
                "username": f_dav_user.get_untracked(),
                "password": f_dav_pass.get_untracked(),
            }),
            _ => serde_json::json!({}),
        };
        // Watch applies to any backend kind — fold it into the config uniformly.
        let mut config = config;
        if let Some(obj) = config.as_object_mut() {
            obj.insert("watch".into(), serde_json::json!(f_watch.get_untracked()));
        }
        let body = crate::api::NewStorageStore { name, kind, config };
        manage_busy.set(true);
        manage_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::create_store(token.as_deref(), &body).await {
                Ok(_) => {
                    // Clear the form and refresh the store list.
                    new_name.set(String::new());
                    f_local_path.set(String::new());
                    f_local_browse.set(false);
                    f_watch.set(false);
                    f_s3_endpoint.set(String::new());
                    f_s3_region.set(String::new());
                    f_s3_access.set(String::new());
                    f_s3_secret.set(String::new());
                    f_s3_bucket.set(String::new());
                    f_dav_url.set(String::new());
                    f_dav_user.set(String::new());
                    f_dav_pass.set(String::new());
                    load_stores();
                }
                Err(e) => manage_error.set(Some(e.to_string())),
            }
            manage_busy.set(false);
        });
    };

    // Delete a runtime backend (config backends can't be deleted), then reload.
    let delete_backend = move |name: String| {
        if manage_busy.get_untracked() {
            return;
        }
        manage_busy.set(true);
        manage_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::delete_store(token.as_deref(), &name).await {
                Ok(()) => load_stores(),
                Err(e) => manage_error.set(Some(e.to_string())),
            }
            manage_busy.set(false);
        });
    };

    // Initial load (also re-run by `load_stores` once the default store is known).
    refresh();

    // Delete an object (blob + catalogue) from its own store, then reload.
    let delete = move |key: String, store: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        action_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            let store = (!store.is_empty()).then_some(store.as_str());
            match rest::delete_object(token.as_deref(), &key, store).await {
                Ok(()) => {
                    busy.set(false);
                    refresh();
                }
                Err(e) => {
                    busy.set(false);
                    action_error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Upload a picked file: read its bytes, then PUT under the current prefix
    // (the prefix box doubles as the destination "folder"). Reload on success.
    let do_upload = move |file: web_sys::File| {
        if uploading.get_untracked() {
            return;
        }
        uploading.set(true);
        upload_error.set(None);
        let pfx = prefix.get_untracked();
        let store = selected_store.get_untracked();
        spawn_local(async move {
            match read_file(file).await {
                Ok((name, ctype, bytes)) => {
                    let key = join_key(&pfx, &name);
                    let token = auth::resolve_token();
                    let store = (!store.is_empty()).then_some(store.as_str());
                    let result =
                        rest::upload_object(token.as_deref(), &key, store, bytes, ctype.as_deref())
                            .await;
                    uploading.set(false);
                    match result {
                        Ok(()) => refresh(),
                        Err(e) => upload_error.set(Some(e.to_string())),
                    }
                }
                Err(e) => {
                    uploading.set(false);
                    upload_error.set(Some(e));
                }
            }
        });
    };

    // `<input type=file>` change handler: pull the first selected file and start
    // the upload, then clear the input so re-picking the same file re-fires.
    let on_file_change = move |ev: leptos::ev::Event| {
        let Some(target) = ev.target() else {
            return;
        };
        let Ok(input) = target.dyn_into::<web_sys::HtmlInputElement>() else {
            return;
        };
        if let Some(file) = input.files().and_then(|fs| fs.get(0)) {
            input.set_value("");
            do_upload(file);
        }
    };

    let on_filter_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        refresh();
    };

    // Manual scan/index of the selected store: reconcile the catalogue with the
    // backend (index new/changed files, drop vanished ones), then reload the tree.
    let scanning = RwSignal::new(false);
    let scan_notice = RwSignal::new(Option::<String>::None);
    let scan_now = move || {
        if scanning.get_untracked() {
            return;
        }
        let store = selected_store.get_untracked();
        if store.is_empty() {
            return;
        }
        scanning.set(true);
        scan_notice.set(None);
        action_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::scan_store(token.as_deref(), &store).await {
                Ok(r) => {
                    let mut msg = format!(
                        "Scanned {} file(s): {} indexed, {} unchanged, {} removed.",
                        r.scanned, r.indexed, r.unchanged, r.removed
                    );
                    if r.indexed > 0 {
                        msg.push_str(" Text extraction runs in the background.");
                    }
                    if r.truncated {
                        msg.push_str(
                            " (Listing capped at 1000; deletions past the cap weren't reconciled.)",
                        );
                    }
                    scan_notice.set(Some(msg));
                    scanning.set(false);
                    refresh();
                }
                Err(e) => {
                    action_error.set(Some(format!("Scan failed: {e}")));
                    scanning.set(false);
                }
            }
        });
    };

    // The dev token, resolved once per render for building download links.
    let token_for_links = move || auth::resolve_token();

    // The directory tree's currently *visible* rows: a row shows only when every
    // one of its ancestor directories is expanded. Reactive on both the built tree
    // and the expanded-set, so toggling a folder reveals/hides its subtree.
    let visible_rows = move || {
        expanded.with(|exp| {
            rows.with(|all| {
                all.iter()
                    .filter(|r| ancestors_expanded(&r.path, exp))
                    .cloned()
                    .collect::<Vec<TreeRow>>()
            })
        })
    };

    // The extracted-text viewer modal: the loaded text (when open), a loading
    // flag while fetching, and an error. Opening is triggered from an object's
    // "Indexed ✓" badge; `None` view + not-loading + no-error means closed.
    let text_view = RwSignal::new(Option::<ObjectText>::None);
    let text_loading = RwSignal::new(false);
    let text_error = RwSignal::new(Option::<String>::None);
    let text_open = move || {
        text_view.with(Option::is_some) || text_loading.get() || text_error.with(Option::is_some)
    };
    let close_text = move || {
        text_view.set(None);
        text_loading.set(false);
        text_error.set(None);
    };
    // Fetch + show the §10 extracted text for an ingested object.
    let view_text = move |id: String| {
        if text_loading.get_untracked() {
            return;
        }
        text_view.set(None);
        text_error.set(None);
        text_loading.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::object_text(token.as_deref(), &id).await {
                Ok(view) => text_view.set(Some(view)),
                Err(e) => text_error.set(Some(e.to_string())),
            }
            text_loading.set(false);
        });
    };

    // Content search over objects' §10 extracted text (distinct from the
    // server-side key `prefix`): runs a search, then the body swaps from the
    // catalogue table to a results list until cleared.
    let content_query = RwSignal::new(String::new());
    let content_results = RwSignal::new(Vec::<ObjectHit>::new());
    let content_searching = RwSignal::new(false);
    let content_error = RwSignal::new(Option::<String>::None);
    let content_active = RwSignal::new(false);

    let run_content_search = move || {
        let q = content_query.get_untracked().trim().to_string();
        if q.is_empty() {
            content_active.set(false);
            content_results.set(Vec::new());
            content_error.set(None);
            return;
        }
        content_searching.set(true);
        content_active.set(true);
        content_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::search_objects(token.as_deref(), &q).await {
                Ok(hits) => {
                    content_results.set(hits);
                    content_error.set(None);
                }
                Err(e) => {
                    content_results.set(Vec::new());
                    content_error.set(Some(e.to_string()));
                }
            }
            content_searching.set(false);
        });
    };
    let clear_content_search = move || {
        content_query.set(String::new());
        content_active.set(false);
        content_results.set(Vec::new());
        content_error.set(None);
    };
    let on_content_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        run_content_search();
    };

    view! {
        <section class="files-panel">
            <header class="files-header">
                <div class="files-header-titles">
                    <h2 class="files-title">"Files"</h2>
                    <span class="files-subtitle">"Browse a storage backend's files"</span>
                </div>
                <div class="files-actions">
                    <form class="files-filter" on:submit=on_content_submit>
                        <input
                            class="files-input"
                            placeholder="Search file contents…"
                            prop:value=move || content_query.get()
                            on:input=move |ev| content_query.set(event_target_value(&ev))
                        />
                        <Show
                            when=move || content_active.get()
                            fallback=|| ().into_view()
                        >
                            <button
                                class="files-btn"
                                type="button"
                                title="Clear search"
                                on:click=move |_| clear_content_search()
                            >
                                <Icon icon=MdIcon::Close />
                            </button>
                        </Show>
                    </form>
                    <form class="files-filter" on:submit=on_filter_submit>
                        <input
                            class="files-input"
                            placeholder="Filter / upload prefix…"
                            disabled=move || loading.get()
                            prop:value=move || prefix.get()
                            on:input=move |ev| prefix.set(event_target_value(&ev))
                        />
                        <button
                            class="files-btn"
                            type="submit"
                            disabled=move || loading.get()
                        >
                            "Refresh"
                        </button>
                    </form>
                    // Store switcher — shown only when the workspace has more than
                    // one storage backend (the common single-store case stays
                    // uncluttered). Picks the `?store=` the tree browses AND an
                    // upload lands on; switching reloads the tree (SOUL §9).
                    <Show
                        when=move || stores.with(|s| s.len() > 1)
                        fallback=|| ().into_view()
                    >
                        <select
                            class="files-input files-store-select"
                            title="Store to browse / upload to"
                            disabled=move || uploading.get() || loading.get()
                            prop:value=move || selected_store.get()
                            on:change=move |ev| {
                                selected_store.set(event_target_value(&ev));
                                expanded.set(HashSet::new());
                                refresh();
                            }
                        >
                            {move || {
                                stores
                                    .get()
                                    .into_iter()
                                    .map(|s| {
                                        let label = if s.is_default {
                                            format!("{} (default)", s.name)
                                        } else {
                                            s.name.clone()
                                        };
                                        view! { <option value=s.name.clone()>{label}</option> }
                                    })
                                    .collect::<Vec<_>>()
                            }}
                        </select>
                    </Show>
                    <label class="files-btn files-btn-primary files-upload">
                        {move || if uploading.get() { "Uploading…" } else { "Upload" }}
                        <input
                            class="files-upload-input"
                            type="file"
                            disabled=move || uploading.get()
                            on:change=on_file_change
                        />
                    </label>
                    <button
                        class="files-btn"
                        type="button"
                        title="Index this store: catalogue + extract text from its files (and drop files removed on disk)"
                        disabled=move || scanning.get() || loading.get()
                        on:click=move |_| scan_now()
                    >
                        {move || if scanning.get() { "Scanning…" } else { "Scan / index" }}
                    </button>
                    <button
                        class="files-btn"
                        type="button"
                        title="Add or remove storage backends"
                        on:click=move |_| manage_open.update(|o| *o = !*o)
                    >
                        {move || if manage_open.get() { "Done" } else { "Manage storage" }}
                    </button>
                </div>
            </header>

            // --- Storage manager (add / remove backends) ---------------------
            <Show when=move || manage_open.get() fallback=|| ().into_view()>
                <div class="files-manager">
                    <Show
                        when=move || manage_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="files-banner files-error">
                            {move || manage_error.get().unwrap_or_default()}
                        </div>
                    </Show>
                    <ul class="files-store-list">
                        <For
                            each=move || stores.get()
                            key=|s| s.name.clone()
                            children=move |s: StorageStore| {
                                let mut meta = if s.is_default {
                                    format!("{} · {} · default", s.kind, s.source)
                                } else {
                                    format!("{} · {}", s.kind, s.source)
                                };
                                if s.watch {
                                    meta.push_str(" · watching");
                                }
                                // Only runtime backends can be removed; config ones
                                // are declared in the TOML and are read-only.
                                let remove = if s.is_runtime() {
                                    let del_name = s.name.clone();
                                    view! {
                                        <button
                                            class="files-btn files-btn-danger"
                                            type="button"
                                            disabled=move || manage_busy.get()
                                            on:click=move |_| delete_backend(del_name.clone())
                                        >
                                            "Remove"
                                        </button>
                                    }
                                        .into_any()
                                } else {
                                    ().into_any()
                                };
                                view! {
                                    <li class="files-store-row">
                                        <span class="files-store-name">{s.name.clone()}</span>
                                        <span class="files-type">{meta}</span>
                                        {remove}
                                    </li>
                                }
                            }
                        />
                    </ul>
                    <form
                        class="files-store-form"
                        on:submit=move |ev| {
                            ev.prevent_default();
                            add_backend();
                        }
                    >
                        <input
                            class="files-input"
                            placeholder="New backend name"
                            prop:value=move || new_name.get()
                            on:input=move |ev| new_name.set(event_target_value(&ev))
                        />
                        <select
                            class="files-input"
                            prop:value=move || new_kind.get()
                            on:change=move |ev| new_kind.set(event_target_value(&ev))
                        >
                            <option value="local">"Local folder"</option>
                            <option value="s3">"S3 / compatible"</option>
                            <option value="webdav">"WebDAV"</option>
                        </select>
                        <Show
                            when=move || new_kind.with(|k| k == "local")
                            fallback=|| ().into_view()
                        >
                            <input
                                class="files-input"
                                placeholder="Local path, e.g. /data/files"
                                prop:value=move || f_local_path.get()
                                on:input=move |ev| f_local_path.set(event_target_value(&ev))
                            />
                            <label
                                class="files-store-check"
                                title="List the directory's existing files instead of an isolated, namespaced bucket. No per-workspace isolation — use only on a trusted machine."
                            >
                                <input
                                    type="checkbox"
                                    prop:checked=move || f_local_browse.get()
                                    on:change=move |ev| f_local_browse.set(event_target_checked(&ev))
                                />
                                "Browse existing directory (show files already on disk)"
                            </label>
                        </Show>
                        <Show when=move || new_kind.with(|k| k == "s3") fallback=|| ().into_view()>
                            <input
                                class="files-input"
                                placeholder="Endpoint, e.g. http://localhost:9000"
                                prop:value=move || f_s3_endpoint.get()
                                on:input=move |ev| f_s3_endpoint.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                placeholder="Bucket"
                                prop:value=move || f_s3_bucket.get()
                                on:input=move |ev| f_s3_bucket.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                placeholder="Region (optional)"
                                prop:value=move || f_s3_region.get()
                                on:input=move |ev| f_s3_region.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                placeholder="Access key"
                                prop:value=move || f_s3_access.get()
                                on:input=move |ev| f_s3_access.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                type="password"
                                placeholder="Secret key"
                                prop:value=move || f_s3_secret.get()
                                on:input=move |ev| f_s3_secret.set(event_target_value(&ev))
                            />
                            <label class="files-store-check">
                                <input
                                    type="checkbox"
                                    prop:checked=move || f_s3_path_style.get()
                                    on:change=move |ev| f_s3_path_style.set(event_target_checked(&ev))
                                />
                                "Path-style (MinIO / self-hosted)"
                            </label>
                        </Show>
                        <Show
                            when=move || new_kind.with(|k| k == "webdav")
                            fallback=|| ().into_view()
                        >
                            <input
                                class="files-input"
                                placeholder="Collection URL, e.g. https://cloud/dav/"
                                prop:value=move || f_dav_url.get()
                                on:input=move |ev| f_dav_url.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                placeholder="Username"
                                prop:value=move || f_dav_user.get()
                                on:input=move |ev| f_dav_user.set(event_target_value(&ev))
                            />
                            <input
                                class="files-input"
                                type="password"
                                placeholder="Password"
                                prop:value=move || f_dav_pass.get()
                                on:input=move |ev| f_dav_pass.set(event_target_value(&ev))
                            />
                        </Show>
                        <label
                            class="files-store-check"
                            title="Keep this store's search index in sync as files change: real-time for a local folder, periodic for S3/WebDAV."
                        >
                            <input
                                type="checkbox"
                                prop:checked=move || f_watch.get()
                                on:change=move |ev| f_watch.set(event_target_checked(&ev))
                            />
                            "Watch for changes (auto-reindex)"
                        </label>
                        <button
                            class="files-btn files-btn-primary"
                            type="submit"
                            disabled=move || manage_busy.get()
                        >
                            {move || if manage_busy.get() { "Saving…" } else { "Add backend" }}
                        </button>
                    </form>
                </div>
            </Show>

            <Show
                when=move || scan_notice.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="files-banner files-notice">
                    {move || scan_notice.get().unwrap_or_default()}
                    <button
                        class="files-btn files-btn-link"
                        type="button"
                        on:click=move |_| scan_notice.set(None)
                    >
                        "Dismiss"
                    </button>
                </div>
            </Show>

            <Show
                when=move || upload_error.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="files-banner files-error">
                    {move || {
                        format!("Upload failed: {}", upload_error.get().unwrap_or_default())
                    }}
                </div>
            </Show>

            <Show
                when=move || action_error.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="files-banner files-error">
                    {move || action_error.get().unwrap_or_default()}
                </div>
            </Show>

            <Show
                when=move || label_error.with(Option::is_some)
                fallback=|| ().into_view()
            >
                <div class="files-banner files-error">
                    {move || format!("Label failed: {}", label_error.get().unwrap_or_default())}
                </div>
            </Show>

            <div class="files-body">
                // --- Content search results (shown once a search has run) -------
                <Show when=move || content_active.get() fallback=|| ().into_view()>
                    <Show when=move || content_searching.get() fallback=|| ().into_view()>
                        <div class="files-status">"Searching…"</div>
                    </Show>
                    <Show
                        when=move || content_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="files-status files-error">
                            {move || {
                                format!("Search failed: {}", content_error.get().unwrap_or_default())
                            }}
                        </div>
                    </Show>
                    <Show
                        when=move || {
                            !content_searching.get()
                                && content_error.with(Option::is_none)
                                && content_results.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="files-status">"No files match. (Only indexed files are searchable.)"</div>
                    </Show>
                    <ul class="files-hits">
                        <For
                            each=move || content_results.get()
                            key=|h| h.id.clone()
                            children=move |h: ObjectHit| {
                                let key = h.key.clone();
                                let base = key_basename(&key).to_string();
                                let ctype = h.content_type.clone().unwrap_or_default();
                                let excerpt = h.excerpt.clone();
                                // The hit carries the store it lives on, so its
                                // download targets the right backend (content search
                                // spans every store). Empty → the default store.
                                let store = (!h.store.is_empty()).then(|| h.store.clone());
                                let href = rest::download_url(
                                    token_for_links().as_deref(),
                                    &key,
                                    store.as_deref(),
                                );
                                let dl_name = base.clone();
                                let id_text = h.id.clone();
                                view! {
                                    <li class="files-hit">
                                        <div class="files-hit-head">
                                            <span class="files-hit-name">{base}</span>
                                            <span class="files-type">{ctype}</span>
                                        </div>
                                        <div class="files-hit-excerpt">{excerpt}</div>
                                        <div class="files-hit-actions">
                                            <button
                                                class="files-btn files-btn-link"
                                                type="button"
                                                on:click=move |_| view_text(id_text.clone())
                                            >
                                                "View text"
                                            </button>
                                            <a
                                                class="files-btn files-btn-link"
                                                href=href
                                                download=dl_name
                                                target="_blank"
                                                rel="noopener"
                                            >
                                                "Download"
                                            </a>
                                        </div>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </Show>

                // --- Directory tree (when not content-searching) ---------------
                <Show when=move || !content_active.get() fallback=|| ().into_view()>
                <Show when=move || loading.get() fallback=|| ().into_view()>
                    <div class="files-status">"Loading…"</div>
                </Show>

                <Show
                    when=move || !loading.get() && load_error.with(Option::is_some)
                    fallback=|| ().into_view()
                >
                    <div class="files-status files-error">
                        {move || {
                            format!(
                                "Could not load files: {}",
                                load_error.get().unwrap_or_default(),
                            )
                        }}
                    </div>
                </Show>

                <Show
                    when=move || {
                        !loading.get()
                            && load_error.with(Option::is_none)
                            && rows.with(Vec::is_empty)
                    }
                    fallback=|| ().into_view()
                >
                    <div class="files-status">
                        "No files in this store. Upload one, or point a local store at an existing directory (Manage storage → \"Browse existing directory\") to see files already on disk."
                    </div>
                </Show>

                <Show
                    when=move || {
                        !loading.get()
                            && load_error.with(Option::is_none)
                            && !rows.with(Vec::is_empty)
                    }
                    fallback=|| ().into_view()
                >
                    <div class="files-tree">
                        <For
                            each=visible_rows
                            // Key on (store, path): switching stores re-renders every
                            // row even if a key path coincides across backends, so a
                            // reused row never shows the wrong store's metadata.
                            key=move |r| (selected_store.get_untracked(), r.path.clone())
                            children=move |r: TreeRow| {
                                let pad = format!("padding-left:{}px", r.depth * 18 + 10);
                                if r.is_dir {
                                    let toggle_path = r.path.clone();
                                    let open_path = r.path.clone();
                                    let is_open = move || expanded.with(|e| e.contains(&open_path));
                                    view! {
                                        <div
                                            class="files-tree-row files-tree-dir"
                                            style=pad
                                            on:click=move |_| {
                                                expanded
                                                    .update(|e| {
                                                        if !e.remove(&toggle_path) {
                                                            e.insert(toggle_path.clone());
                                                        }
                                                    })
                                            }
                                        >
                                            <span class="files-tree-caret">
                                                {move || if is_open() { "▾" } else { "▸" }}
                                            </span>
                                            <span class="files-tree-icon"><Icon icon=MdIcon::Folder /></span>
                                            <span class="files-tree-name">{r.name.clone()}</span>
                                            {label_strip(r.path.clone(), true)}
                                        </div>
                                    }
                                        .into_any()
                                } else {
                                    let key = r.path.clone();
                                    let del_key = r.path.clone();
                                    let dl_name = r.name.clone();
                                    let ctype =
                                        r.content_type.clone().unwrap_or_else(|| "—".into());
                                    let size = human_size(r.size);
                                    let modified = fmt_timestamp(&r.last_modified);
                                    let badge = match (r.cat_id.clone(), r.ingested) {
                                        (Some(id), true) => {
                                            view! {
                                                <button
                                                    class="files-badge files-badge-on files-badge-btn"
                                                    type="button"
                                                    title="View the extracted text indexed for search"
                                                    on:click=move |_| view_text(id.clone())
                                                >
                                                    <Icon icon=MdIcon::Check />
                                                </button>
                                            }
                                                .into_any()
                                        }
                                        _ => {
                                            view! { <span class="files-badge">"—"</span> }
                                                .into_any()
                                        }
                                    };
                                    view! {
                                        <div class="files-tree-row files-tree-file" style=pad>
                                            // Empty caret keeps file icons aligned
                                            // with sibling folders' (which have one).
                                            <span class="files-tree-caret"></span>
                                            <span class="files-tree-icon"><Icon icon=MdIcon::File /></span>
                                            <span class="files-tree-name">{r.name.clone()}</span>
                                            {label_strip(r.path.clone(), false)}
                                            <span class="files-tree-type files-type">{ctype}</span>
                                            <span class="files-tree-size">{size}</span>
                                            <span class="files-tree-modified">{modified}</span>
                                            <span class="files-tree-cell">{badge}</span>
                                            <span class="files-tree-actions">
                                                <a
                                                    class="files-btn files-btn-link"
                                                    href=move || {
                                                        rest::download_url(
                                                            token_for_links().as_deref(),
                                                            &key,
                                                            Some(&selected_store.get()),
                                                        )
                                                    }
                                                    download=dl_name
                                                    target="_blank"
                                                    rel="noopener"
                                                >
                                                    "Download"
                                                </a>
                                                <button
                                                    class="files-btn files-btn-danger"
                                                    type="button"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| {
                                                        delete(
                                                            del_key.clone(),
                                                            selected_store.get_untracked(),
                                                        )
                                                    }
                                                >
                                                    "Delete"
                                                </button>
                                            </span>
                                        </div>
                                    }
                                        .into_any()
                                }
                            }
                        />
                    </div>
                    <Show when=move || truncated.get() fallback=|| ().into_view()>
                        <div class="files-status files-tree-truncated">
                            "Showing the first 1000 files — this directory has more. Use the filter box to narrow into a subfolder."
                        </div>
                    </Show>
                </Show>
                </Show>
            </div>

            // --- Extracted-text viewer modal (§10) ---------------------------
            <Show when=text_open fallback=|| ().into_view()>
                <div class="files-modal-overlay" on:click=move |_| close_text()>
                    <div class="files-modal" on:click=move |ev| ev.stop_propagation()>
                        <header class="files-modal-header">
                            <h3 class="files-modal-title">
                                {move || {
                                    text_view.with(|v| {
                                        v.as_ref()
                                            .map(|t| t.key.clone())
                                            .unwrap_or_else(|| "Extracted text".to_string())
                                    })
                                }}
                            </h3>
                            <button
                                class="files-modal-close"
                                type="button"
                                title="Close"
                                on:click=move |_| close_text()
                            >
                                <Icon icon=MdIcon::Close />
                            </button>
                        </header>
                        <div class="files-modal-body">
                            <Show when=move || text_loading.get() fallback=|| ().into_view()>
                                <p class="files-status">"Loading…"</p>
                            </Show>
                            <Show
                                when=move || text_error.with(Option::is_some)
                                fallback=|| ().into_view()
                            >
                                <p class="files-status files-error">
                                    {move || text_error.get().unwrap_or_default()}
                                </p>
                            </Show>
                            <Show
                                when=move || {
                                    !text_loading.get() && text_view.with(Option::is_some)
                                }
                                fallback=|| ().into_view()
                            >
                                <Show
                                    when=move || {
                                        text_view.with(|v| {
                                            v.as_ref().is_some_and(|t| t.summary.is_some())
                                        })
                                    }
                                    fallback=|| ().into_view()
                                >
                                    <p class="files-modal-summary">
                                        {move || {
                                            text_view.with(|v| {
                                                v.as_ref()
                                                    .and_then(|t| t.summary.clone())
                                                    .unwrap_or_default()
                                            })
                                        }}
                                    </p>
                                </Show>
                                <Show
                                    when=move || {
                                        text_view.with(|v| v.as_ref().is_some_and(|t| !t.has_text))
                                    }
                                    fallback=|| ().into_view()
                                >
                                    <p class="files-status">
                                        "No extracted text for this object yet."
                                    </p>
                                </Show>
                                <pre class="files-modal-text">
                                    {move || {
                                        text_view
                                            .with(|v| {
                                                v.as_ref().map(|t| t.text.clone()).unwrap_or_default()
                                            })
                                    }}
                                </pre>
                                <Show
                                    when=move || {
                                        text_view.with(|v| v.as_ref().is_some_and(|t| t.truncated))
                                    }
                                    fallback=|| ().into_view()
                                >
                                    <p class="files-modal-note">
                                        "Showing the first 1 MiB — the extracted text is longer."
                                    </p>
                                </Show>
                            </Show>
                        </div>
                    </div>
                </div>
            </Show>
        </section>
    }
}

/// Read a picked browser [`web_sys::File`] into `(name, content_type, bytes)`.
///
/// `File` extends `Blob`, so `array_buffer()` yields a JS `Promise<ArrayBuffer>`
/// we await via [`wasm_bindgen_futures::JsFuture`], then copy into a `Vec<u8>`.
/// The file's MIME `type` is forwarded as the upload's content type when known.
async fn read_file(file: web_sys::File) -> Result<(String, Option<String>, Vec<u8>), String> {
    let name = file.name();
    let ctype = {
        let t = file.type_();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let buf = wasm_bindgen_futures::JsFuture::from(file.array_buffer())
        .await
        .map_err(|_| "could not read the selected file".to_string())?;
    let array = js_sys::Uint8Array::new(&buf);
    Ok((name, ctype, array.to_vec()))
}

/// Join an upload prefix and a file name into an object key, trimming stray
/// slashes. An empty prefix yields just the file name (upload to the root).
fn join_key(prefix: &str, name: &str) -> String {
    let p = prefix.trim_matches('/');
    let n = name.trim_matches('/');
    if p.is_empty() {
        n.to_string()
    } else {
        format!("{p}/{n}")
    }
}

/// The final path segment of a key (the file's display name).
fn key_basename(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// One row of the rendered directory tree — a directory or a file. Files carry
/// their backend metadata plus the catalogue link (`cat_id` + `ingested`) when a
/// matching catalogue row exists, which drives the "Indexed ✓" badge + text view.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TreeRow {
    /// Full key from the store root: a file's key, or a directory path (no trailing
    /// slash). The expand-set and the row ordering key on this.
    path: String,
    /// Last path segment (the displayed name).
    name: String,
    /// Nesting depth (count of `/` in `path`) — drives the indent.
    depth: usize,
    is_dir: bool,
    size: u64,
    content_type: Option<String>,
    last_modified: String,
    /// Catalogue object id (for the §10 text viewer), when this file is catalogued.
    cat_id: Option<String>,
    /// Whether the catalogue row has extracted text (the badge is clickable).
    ingested: bool,
}

/// Build the key-sorted tree rows (every intermediate directory + each file) from a
/// store's flat backend listing, layering in catalogue info by key. Directories are
/// synthesized from the files' key prefixes, so a folder appears even though the
/// backend lists only files. The key-lexicographic sort places each directory
/// immediately before its descendants — a valid pre-order for the indented tree.
fn build_rows(
    objects: Vec<BackendObject>,
    catalogue: &HashMap<String, (String, bool)>,
) -> Vec<TreeRow> {
    use std::collections::BTreeSet;
    // Every ancestor directory of every file (the `key[..i]` at each `/`).
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    for o in &objects {
        let key = o.key.trim_matches('/');
        for (i, ch) in key.char_indices() {
            if ch == '/' {
                dirs.insert(key[..i].to_string());
            }
        }
    }
    let mut rows: Vec<TreeRow> = Vec::with_capacity(dirs.len() + objects.len());
    for d in dirs {
        rows.push(TreeRow {
            name: key_basename(&d).to_string(),
            depth: d.matches('/').count(),
            is_dir: true,
            path: d,
            size: 0,
            content_type: None,
            last_modified: String::new(),
            cat_id: None,
            ingested: false,
        });
    }
    for o in objects {
        let key = o.key.trim_matches('/').to_string();
        let (cat_id, ingested) = match catalogue.get(&key) {
            Some((id, ing)) => (Some(id.clone()), *ing),
            None => (None, false),
        };
        rows.push(TreeRow {
            name: key_basename(&key).to_string(),
            depth: key.matches('/').count(),
            is_dir: false,
            path: key,
            size: o.size,
            content_type: o.content_type,
            last_modified: o.last_modified,
            cat_id,
            ingested,
        });
    }
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

/// Whether every ancestor directory of `path` is in `expanded` — i.e. the row at
/// `path` should be visible. A top-level row (no `/`) is always visible.
fn ancestors_expanded(path: &str, expanded: &HashSet<String>) -> bool {
    let mut start = 0;
    while let Some(i) = path[start..].find('/') {
        let cut = start + i;
        if !expanded.contains(&path[..cut]) {
            return false;
        }
        start = cut + 1;
    }
    true
}

/// Render a byte count as a compact human-readable size (binary units).
fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else if bytes < GIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    }
}

/// Format an RFC 3339 / ISO-8601 timestamp as a compact `YYYY-MM-DD HH:MM` for
/// the table. Standalone string slicing (no chrono in the wasm bundle); falls
/// back to the raw string if it isn't the expected shape.
fn fmt_timestamp(rfc3339: &str) -> String {
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

    #[test]
    fn join_key_combines_prefix_and_name() {
        assert_eq!(join_key("", "a.txt"), "a.txt");
        assert_eq!(join_key("docs", "a.txt"), "docs/a.txt");
        // Stray slashes on either side are normalized to a single separator.
        assert_eq!(join_key("docs/", "/a.txt"), "docs/a.txt");
        assert_eq!(join_key("/docs/2026/", "a.txt"), "docs/2026/a.txt");
        assert_eq!(join_key("   ".trim(), "a.txt"), "a.txt");
    }

    #[test]
    fn basename_is_the_last_segment() {
        assert_eq!(key_basename("docs/2026/report.pdf"), "report.pdf");
        // No slash: all basename.
        assert_eq!(key_basename("a.txt"), "a.txt");
    }

    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn fmt_timestamp_trims_to_minute() {
        assert_eq!(fmt_timestamp("2026-06-14T09:00:00Z"), "2026-06-14 09:00");
        assert_eq!(
            fmt_timestamp("2026-06-14T23:59:59+00:00"),
            "2026-06-14 23:59"
        );
        // Not the expected shape → returned unchanged.
        assert_eq!(fmt_timestamp("whenever"), "whenever");
    }

    fn obj(key: &str) -> BackendObject {
        BackendObject {
            key: key.into(),
            size: 10,
            etag: None,
            content_type: Some("text/plain".into()),
            last_modified: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn build_rows_synthesizes_dirs_and_layers_catalogue() {
        let mut cat: HashMap<String, (String, bool)> = HashMap::new();
        cat.insert("projects/2026/plan.md".into(), ("doc-1".into(), true));
        let rows = build_rows(
            vec![
                obj("projects/2026/plan.md"),
                obj("projects/notes.txt"),
                obj("top.txt"),
            ],
            &cat,
        );
        // Two synthesized dirs (`projects`, `projects/2026`) + three files, sorted by
        // path with each dir immediately before its descendants.
        let shape: Vec<(&str, usize, bool)> = rows
            .iter()
            .map(|r| (r.path.as_str(), r.depth, r.is_dir))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("projects", 0, true),
                ("projects/2026", 1, true),
                ("projects/2026/plan.md", 2, false),
                ("projects/notes.txt", 1, false),
                ("top.txt", 0, false),
            ]
        );
        // The catalogued file carries its badge; the others don't.
        let plan = rows
            .iter()
            .find(|r| r.path == "projects/2026/plan.md")
            .unwrap();
        assert_eq!(plan.cat_id.as_deref(), Some("doc-1"));
        assert!(plan.ingested);
        let notes = rows
            .iter()
            .find(|r| r.path == "projects/notes.txt")
            .unwrap();
        assert!(notes.cat_id.is_none());
        assert!(!notes.ingested);
    }

    #[test]
    fn ancestors_expanded_gates_visibility() {
        let mut exp = HashSet::new();
        // Top-level rows are always visible.
        assert!(ancestors_expanded("top.txt", &exp));
        assert!(ancestors_expanded("projects", &exp));
        // A nested row needs every ancestor expanded.
        assert!(!ancestors_expanded("projects/2026/plan.md", &exp));
        exp.insert("projects".to_string());
        assert!(ancestors_expanded("projects/notes.txt", &exp));
        assert!(!ancestors_expanded("projects/2026/plan.md", &exp));
        exp.insert("projects/2026".to_string());
        assert!(ancestors_expanded("projects/2026/plan.md", &exp));
    }
}
