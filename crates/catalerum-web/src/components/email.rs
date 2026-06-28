//! The Email panel (SOUL §28, §12 — read-only inbox view).
//!
//! A three-pane read view: a mailboxes rail (the per-account tree), a middle
//! column with a filter bar (sender / unread / content) over the email list,
//! and a right detail pane for the selected message. On narrow screens the
//! rail becomes a left drawer toggled from the list header and the list/detail
//! panes stack.
//! It is a thin client of the email read surface (`/mailboxes`, `/emails`,
//! `/emails/{id}`) — every call carries the dev session token and is
//! workspace-scoped + `email:read`-gated server-side (SOUL §18/§19).
//!
//! Read-only by design: catalerum reads mail, it never sends or replies (§14).
//! An HTML body renders inside a **fully sandboxed iframe** — never injected
//! into the workbench DOM (no `inner_html`) — after a server-side sanitizer
//! pass, with remote images blocked until a per-message opt-in. See
//! [`render_body`] for the layered security model.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::{JsCast, JsValue};

use crate::api::{Attachment, EmailDetail, EmailView, Mailbox};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// The Email panel's base frontend route. The open message is deep-linkable at
/// `<EMAIL_ROUTE>/<id>`; the bare route is "no message open".
const EMAIL_ROUTE: &str = "/app/email";

/// The message id encoded in the current browser URL (`/app/email/<id>`), if the
/// path carries one. Seeds the open message from a deep link or reload; a bare
/// `/app/email` yields `None`.
fn email_from_location() -> Option<String> {
    let path = web_sys::window()?.location().pathname().ok()?;
    let id = path
        .trim_end_matches('/')
        .strip_prefix(EMAIL_ROUTE)?
        .trim_start_matches('/');
    (!id.is_empty()).then(|| id.to_string())
}

/// Reflect the open message in the browser URL as `/app/email/<id>`, or the bare
/// `/app/email` when none is open. Uses `replace_state` so opening messages
/// tracks the address bar without stacking per-message history entries. No-op
/// when already at the URL.
fn sync_location_to_email(id: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match id {
        Some(id) => format!("{EMAIL_ROUTE}/{id}"),
        None => EMAIL_ROUTE.to_string(),
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

/// The Email panel component.
#[component]
pub fn EmailPanel() -> impl IntoView {
    let mailboxes = RwSignal::new(Vec::<Mailbox>::new());
    let emails = RwSignal::new(Vec::<EmailView>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // Filters. The mailbox filter is by **id** (mailbox names collide across
    // accounts — every account has an `INBOX`); `""` = all mailboxes.
    let filter_mailbox_id = RwSignal::new(String::new());
    let filter_sender = RwSignal::new(String::new());
    let filter_content = RwSignal::new(String::new());
    let filter_unread = RwSignal::new("all".to_string());
    // Accounts (connections) whose mailbox list is folded shut in the sidebar.
    // Default-expanded: an id is only in the set after the user collapses it.
    let collapsed = RwSignal::new(std::collections::HashSet::<String>::new());
    // The mailboxes rail as a mobile drawer (inert on desktop, where the rail
    // is always visible as the panel's first column).
    let mbx_open = RwSignal::new(false);

    // The open email + its detail.
    let selected_id = RwSignal::new(Option::<String>::None);
    let detail = RwSignal::new(Option::<EmailDetail>::None);
    let detail_loading = RwSignal::new(false);
    let detail_error = RwSignal::new(Option::<String>::None);
    // A failed attachment / raw-`.eml` download surfaced in the message pane
    // (e.g. the stored blob was pruned → the object 404s). Reset per open.
    let download_error = RwSignal::new(Option::<String>::None);
    // A read/unread toggle in flight (debounces the button).
    let marking = RwSignal::new(false);

    // Fetch the email list under the current filters.
    let load_emails = move || {
        loading.set(true);
        load_error.set(None);
        let mb = filter_mailbox_id.get_untracked();
        let sender = filter_sender.get_untracked();
        let content = filter_content.get_untracked();
        let unread = unread_filter(&filter_unread.get_untracked());
        spawn_local(async move {
            let token = auth::resolve_token();
            let mb_opt = (!mb.is_empty()).then_some(mb.as_str());
            let sender_trimmed = sender.trim();
            let sender_opt = (!sender_trimmed.is_empty()).then_some(sender_trimmed);
            let content_trimmed = content.trim();
            let content_opt = (!content_trimmed.is_empty()).then_some(content_trimmed);
            match rest::list_emails(token.as_deref(), mb_opt, sender_opt, content_opt, unread).await
            {
                Ok(list) => {
                    emails.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    emails.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    // Fetch the mailbox list for the account tree — also re-fetched after a
    // read/unread toggle so the badge counts stay honest (best-effort).
    let load_mailboxes = move || {
        spawn_local(async move {
            let token = auth::resolve_token();
            if let Ok(list) = rest::list_mailboxes(token.as_deref()).await {
                mailboxes.set(list);
            }
        });
    };

    // Toggle one email's read/unread state (SOUL §28) — the **local** `seen`
    // flag only; the provider's mailbox is never written (§14). On success the
    // open detail, its list row, and the sidebar badges all update.
    let set_read_state = move |id: String, unread: bool| {
        if marking.get_untracked() {
            return;
        }
        marking.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::set_email_read(token.as_deref(), &id, unread).await {
                Ok(state) => {
                    detail.update(|d| {
                        if let Some(d) = d.as_mut() {
                            if d.id == state.id {
                                d.unread = state.unread;
                            }
                        }
                    });
                    emails.update(|list| {
                        if let Some(row) = list.iter_mut().find(|r| r.id == state.id) {
                            row.unread = state.unread;
                        }
                    });
                    load_mailboxes();
                }
                Err(e) => detail_error.set(Some(format!("Could not update read state: {e}"))),
            }
            marking.set(false);
        });
    };

    // Download a stored object (an attachment key or the raw `.eml` key) to disk.
    // Fetches the bytes over the authed object route so a pruned/absent blob
    // (`404`) surfaces as an in-pane notice instead of a broken navigation; on
    // success it hands the bytes to the browser via a client-side download.
    let start_download = move |key: String, filename: String, content_type: Option<String>| {
        download_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::fetch_object_bytes(token.as_deref(), &key, None).await {
                Ok(bytes) => {
                    if let Err(e) = trigger_download(&bytes, &filename, content_type.as_deref()) {
                        download_error.set(Some(format!("Could not download {filename}: {e}")));
                    }
                }
                Err(e) => {
                    download_error.set(Some(format!("Could not download {filename}: {e}")));
                }
            }
        });
    };

    // Fetch one email's detail.
    let open_email = move |id: String| {
        selected_id.set(Some(id.clone()));
        detail.set(None);
        detail_loading.set(true);
        detail_error.set(None);
        download_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::get_email(token.as_deref(), &id).await {
                Ok(d) => {
                    detail.set(Some(d));
                    detail_error.set(None);
                }
                Err(e) => detail_error.set(Some(e.to_string())),
            }
            detail_loading.set(false);
        });
    };

    // Close the open message and return to the list. On mobile this swaps the
    // full-screen detail back to the list (the detail pane is hidden while
    // nothing is selected); on desktop it just clears the right pane.
    let close_email = move || {
        selected_id.set(None);
        detail.set(None);
        detail_loading.set(false);
        detail_error.set(None);
        download_error.set(None);
    };

    load_mailboxes();
    load_emails();

    // On mount, open a `/app/email/<id>` deep link (reload or shared URL);
    // `open_email` fetches the message detail by id, independent of the list
    // load. Sets `selected_id` synchronously so the mirror effect below sees it.
    if let Some(id) = email_from_location() {
        open_email(id);
    }

    // Mirror the open message into the URL as `/app/email/<id>` so it's
    // deep-linkable and survives reload; tracks every open via the single
    // `selected_id` signal. See `sync_location_to_email`.
    Effect::new(move |_| {
        selected_id.with(|id| sync_location_to_email(id.as_deref()));
    });

    let on_filter_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        load_emails();
    };

    // Select a mailbox (or `""` = all) from the account tree. Also closes the
    // mobile drawer so the newly filtered list is immediately visible.
    let select_mailbox = move |id: String| {
        filter_mailbox_id.set(id);
        mbx_open.set(false);
        load_emails();
    };

    // The account tree: mailboxes grouped by their owning connection, in the
    // listing's order (see [`group_mailboxes`]).
    let account_groups = move || group_mailboxes(&mailboxes.get());
    let total_unread = move || mailboxes.with(|l| l.iter().map(|m| m.unread_count).sum::<i64>());

    // The list column's title: the selected mailbox's name (ids are what the
    // filter carries — names collide across accounts).
    let current_mailbox = move || {
        let id = filter_mailbox_id.get();
        if id.is_empty() {
            "All mailboxes".to_string()
        } else {
            mailboxes.with(|l| {
                l.iter()
                    .find(|m| m.id == id)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| "Mailbox".to_string())
            })
        }
    };

    view! {
        <section
            class="pane-split email-panel"
            class:email-panel-detail=move || selected_id.get().is_some()
        >
            <button
                class="email-mbx-scrim"
                class:email-mbx-scrim-open=move || mbx_open.get()
                aria-label="Close mailboxes"
                tabindex="-1"
                on:click=move |_| mbx_open.set(false)
            ></button>
            <aside class="pane-list email-mailboxes" class:email-mailboxes-open=move || mbx_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Email"</h2>
                </header>
                <nav class="email-accounts">
                    <button
                        class=move || {
                            let mut c = String::from("email-mbx email-mbx-all");
                            if filter_mailbox_id.get().is_empty() {
                                c.push_str(" email-mbx-active");
                            }
                            c
                        }
                        type="button"
                        on:click=move |_| select_mailbox(String::new())
                    >
                        <span class="email-mbx-name">"All mailboxes"</span>
                        {move || unread_badge(total_unread())}
                    </button>
                    <For
                        each=account_groups
                        key=|g| g.connection_id.clone()
                        children=move |g: AccountGroup| {
                            let conn_id = g.connection_id.clone();
                            let toggle_id = conn_id.clone();
                            let is_open = {
                                let conn_id = conn_id.clone();
                                move || !collapsed.get().contains(&conn_id)
                            };
                            let arrow = {
                                let is_open = is_open.clone();
                                move || if is_open() { "▾" } else { "▸" }
                            };
                            let unread_sum: i64 =
                                g.mailboxes.iter().map(|m| m.unread_count).sum();
                            // The rows re-render inside the fold closure below, so
                            // the mailboxes ride a Copy `StoredValue`.
                            let mbs = StoredValue::new(g.mailboxes.clone());
                            view! {
                                <div class="email-account">
                                    <button
                                        class="email-account-header"
                                        type="button"
                                        title="Expand / collapse this account"
                                        on:click=move |_| {
                                            collapsed.update(|set| {
                                                if !set.remove(&toggle_id) {
                                                    set.insert(toggle_id.clone());
                                                }
                                            });
                                        }
                                    >
                                        <span class="email-account-arrow">{arrow}</span>
                                        <span class="email-account-name">{g.name.clone()}</span>
                                        {unread_badge(unread_sum)}
                                    </button>
                                    {move || {
                                        is_open().then(|| {
                                            let rows = mbs
                                                .get_value()
                                                .iter()
                                                .map(|m| {
                                                    let id = m.id.clone();
                                                    let id_cls = id.clone();
                                                    let name = m.name.clone();
                                                    let count = m.unread_count;
                                                    view! {
                                                        <li>
                                                            <button
                                                                class=move || {
                                                                    let mut c = String::from("email-mbx");
                                                                    if filter_mailbox_id.get() == id_cls {
                                                                        c.push_str(" email-mbx-active");
                                                                    }
                                                                    c
                                                                }
                                                                type="button"
                                                                on:click=move |_| select_mailbox(id.clone())
                                                            >
                                                                <span class="email-mbx-name">{name}</span>
                                                                {unread_badge(count)}
                                                            </button>
                                                        </li>
                                                    }
                                                })
                                                .collect::<Vec<_>>();
                                            view! {
                                                <ul class="email-account-mailboxes">{rows}</ul>
                                            }
                                        })
                                    }}
                                </div>
                            }
                        }
                    />
                </nav>
            </aside>

            <aside class="pane-list email-list">
                <header class="email-list-header">
                    <div class="email-list-titlebar">
                        <button
                            class="email-mbx-toggle"
                            type="button"
                            title="Mailboxes"
                            on:click=move |_| mbx_open.update(|o| *o = !*o)
                        >
                            <Icon icon=MdIcon::Menu />
                        </button>
                        <h3 class="email-list-title">{current_mailbox}</h3>
                    </div>
                    <form class="email-filters" on:submit=on_filter_submit>
                        <select
                            class="email-select"
                            on:change=move |ev| {
                                filter_unread.set(event_target_value(&ev));
                                load_emails();
                            }
                        >
                            <option value="all">"All"</option>
                            <option value="unread">"Unread"</option>
                            <option value="read">"Read"</option>
                        </select>
                        <input
                            class="email-input"
                            placeholder="From contains…"
                            prop:value=move || filter_sender.get()
                            on:input=move |ev| filter_sender.set(event_target_value(&ev))
                        />
                        <input
                            class="email-input"
                            placeholder="Search subject/body…"
                            prop:value=move || filter_content.get()
                            on:input=move |ev| filter_content.set(event_target_value(&ev))
                        />
                        <button class="email-btn" type="submit" disabled=move || loading.get()>
                            "Apply"
                        </button>
                    </form>
                </header>

                <div class="pane-list-body">
                    <Show when=move || loading.get() fallback=|| ().into_view()>
                        <div class="email-status">"Loading…"</div>
                    </Show>

                    <Show
                        when=move || !loading.get() && load_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="email-status email-error">
                            {move || {
                                format!(
                                    "Could not load email: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && emails.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="email-status">"No emails match."</div>
                    </Show>

                    <ul class="email-items">
                        <For
                            each=move || emails.get()
                            key=|e| e.id.clone()
                            children=move |e: EmailView| {
                                let id = e.id.clone();
                                let is_active = {
                                    let id = id.clone();
                                    move || selected_id.get().as_deref() == Some(id.as_str())
                                };
                                let unread = e.unread;
                                let class = move || {
                                    let mut c = String::from("email-item");
                                    if is_active() {
                                        c.push_str(" email-item-active");
                                    }
                                    if unread {
                                        c.push_str(" email-item-unread");
                                    }
                                    c
                                };
                                let subject = if e.subject.trim().is_empty() {
                                    "(no subject)".to_string()
                                } else {
                                    e.subject.clone()
                                };
                                let from = e.from.clone().unwrap_or_default();
                                let when = e
                                    .received_at
                                    .as_deref()
                                    .map(fmt_ts)
                                    .unwrap_or_default();
                                let attach = e.has_attachments;
                                let folders = cross_folder_badge(e.folder_count, &e.also_in);
                                let id_for_click = id.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            on:click=move |_| open_email(id_for_click.clone())
                                        >
                                            <span class="email-item-row1">
                                                <span class="email-item-subject">{subject}</span>
                                                {folders
                                                    .map(|(label, tip)| {
                                                        view! {
                                                            <span class="email-item-folders" title=tip>
                                                                {label}
                                                            </span>
                                                        }
                                                    })}
                                                <Show
                                                    when=move || attach
                                                    fallback=|| ().into_view()
                                                >
                                                    <span class="email-attach" title="has attachments"><Icon icon=MdIcon::Attachment /></span>
                                                </Show>
                                            </span>
                                            <span class="email-item-row2">
                                                <span class="email-item-from">{from}</span>
                                                <span class="email-item-when">{when}</span>
                                            </span>
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                </div>
            </aside>

            <div class="email-detail">
                <Show
                    when=move || selected_id.get().is_some()
                    fallback=|| {
                        view! {
                            <div class="email-detail-empty">
                                <p>"Select an email to read it."</p>
                            </div>
                        }
                    }
                >
                    // Mobile-only Back bar: returns from the full-screen detail
                    // to the list (hidden on desktop, where both panes coexist).
                    <div class="email-detail-topbar">
                        <button
                            class="email-back-btn"
                            type="button"
                            on:click=move |_| close_email()
                        >
                            "← Inbox"
                        </button>
                    </div>

                    <Show when=move || detail_loading.get() fallback=|| ().into_view()>
                        <div class="email-status">"Loading message…"</div>
                    </Show>

                    <Show
                        when=move || detail_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="email-status email-error">
                            {move || detail_error.get().unwrap_or_default()}
                        </div>
                    </Show>

                    {move || {
                        detail.get().map(|d| {
                            let subject = if d.subject.trim().is_empty() {
                                "(no subject)".to_string()
                            } else {
                                d.subject.clone()
                            };
                            let from = d.from.as_ref().map(|a| a.display()).unwrap_or_default();
                            let to = join_addrs(&d.to);
                            let cc = join_addrs(&d.cc);
                            let when = d.received_at.as_deref().map(fmt_ts).unwrap_or_default();
                            let mailbox = d.mailbox.clone();
                            let folders = cross_folder_badge(d.folder_count, &d.also_in);
                            let has_to = !to.is_empty();
                            let has_cc = !cc.is_empty();

                            // One download button per archived attachment. Each
                            // resolves its `/storage/objects/<key>` ref to a key and
                            // fetches it through `start_download`; a ref that isn't a
                            // stored object surfaces a notice rather than acting.
                            let att_items = d
                                .attachments
                                .iter()
                                .map(|att| {
                                    let name = attachment_display_name(att);
                                    let title = att.url.clone();
                                    let size = att.size.map(human_size);
                                    let url = att.url.clone();
                                    let ctype = att.content_type.clone();
                                    let name_dl = name.clone();
                                    view! {
                                        <li class="email-attach-item">
                                            <button
                                                class="email-attach-btn"
                                                type="button"
                                                title=title
                                                on:click=move |_| {
                                                    match object_key_from_url(&url) {
                                                        Some(key) => start_download(
                                                            key.to_string(),
                                                            name_dl.clone(),
                                                            ctype.clone(),
                                                        ),
                                                        None => download_error.set(Some(format!(
                                                            "{name_dl}: unsupported attachment location",
                                                        ))),
                                                    }
                                                }
                                            >
                                                <span class="email-attach-icon"><Icon icon=MdIcon::Attachment /></span>
                                                <span class="email-attach-name">{name}</span>
                                                {size
                                                    .map(|s| {
                                                        view! { <span class="email-attach-size">{s}</span> }
                                                    })}
                                            </button>
                                        </li>
                                    }
                                })
                                .collect::<Vec<_>>();
                            let has_atts = !att_items.is_empty();

                            // The archived raw `.eml`, if present — same download path
                            // (the `raw_ref` is itself a stored-object key).
                            let raw_btn = d.raw_ref.clone().map(|key| {
                                let fname = eml_filename(&key);
                                view! {
                                    <button
                                        class="email-attach-btn email-attach-eml"
                                        type="button"
                                        title="Download the archived raw message"
                                        on:click=move |_| {
                                            start_download(
                                                key.clone(),
                                                fname.clone(),
                                                Some("message/rfc822".to_string()),
                                            )
                                        }
                                    >
                                        "⤓ Download original (.eml)"
                                    </button>
                                }
                            });
                            let has_raw = raw_btn.is_some();
                            let show_attachments = has_atts || has_raw;

                            // Read/unread toggle — flips catalerum's LOCAL `seen`
                            // flag only; the provider's mailbox is never written
                            // (SOUL §14), so a provider re-sync may overwrite it.
                            let unread_now = d.unread;
                            let mark_id = d.id.clone();
                            let mark_label = if unread_now {
                                "Mark as read"
                            } else {
                                "Mark as unread"
                            };
                            view! {
                                <article class="email-message">
                                    <div class="email-subject-row">
                                        <h3 class="email-subject">{subject}</h3>
                                        <button
                                            class="email-mark-btn"
                                            type="button"
                                            title="Updates catalerum's copy only — the mail provider is never written"
                                            disabled=move || marking.get()
                                            on:click=move |_| set_read_state(
                                                mark_id.clone(),
                                                !unread_now,
                                            )
                                        >
                                            {mark_label}
                                        </button>
                                    </div>
                                    <div class="email-meta">
                                        <div class="email-meta-row">
                                            <span class="email-meta-k">"From"</span>
                                            <span class="email-meta-v">{from}</span>
                                        </div>
                                        <Show when=move || has_to fallback=|| ().into_view()>
                                            <div class="email-meta-row">
                                                <span class="email-meta-k">"To"</span>
                                                <span class="email-meta-v">{to.clone()}</span>
                                            </div>
                                        </Show>
                                        <Show when=move || has_cc fallback=|| ().into_view()>
                                            <div class="email-meta-row">
                                                <span class="email-meta-k">"Cc"</span>
                                                <span class="email-meta-v">{cc.clone()}</span>
                                            </div>
                                        </Show>
                                        <div class="email-meta-row">
                                            <span class="email-meta-k">"Mailbox"</span>
                                            <span class="email-meta-v">
                                                {format!("{mailbox}  ·  {when}")}
                                                {folders
                                                    .map(|(label, tip)| {
                                                        view! {
                                                            <span
                                                                class="email-item-folders email-meta-folders"
                                                                title=tip
                                                            >
                                                                {label}
                                                            </span>
                                                        }
                                                    })}
                                            </span>
                                        </div>
                                    </div>
                                    <Show
                                        when=move || download_error.with(Option::is_some)
                                        fallback=|| ().into_view()
                                    >
                                        <div class="email-status email-error">
                                            {move || download_error.get().unwrap_or_default()}
                                        </div>
                                    </Show>
                                    {show_attachments
                                        .then(|| {
                                            view! {
                                                <section class="email-attachments">
                                                    {raw_btn}
                                                    {has_atts
                                                        .then(|| {
                                                            view! {
                                                                <div class="email-attachments-head">
                                                                    "Attachments"
                                                                </div>
                                                                <ul class="email-attach-list">{att_items}</ul>
                                                            }
                                                        })}
                                                </section>
                                            }
                                        })}
                                    {render_body(&d)}
                                </article>
                            }
                        })
                    }}
                </Show>
            </div>
        </section>
    }
}

/// One sidebar account section: an email connection and its mailboxes, in the
/// mailbox listing's order.
#[derive(Clone, Debug, PartialEq)]
struct AccountGroup {
    /// The owning connection id (the section's expand/collapse key).
    connection_id: String,
    /// The section header: the connection's display name, with a generic
    /// fallback for an older server that doesn't carry `connection_name`.
    name: String,
    /// The account's mailboxes.
    mailboxes: Vec<Mailbox>,
}

/// Group the workspace's mailboxes by their owning connection (account) for the
/// sidebar tree, preserving the listing's order (first-seen connection first,
/// mailboxes in listing order within it).
fn group_mailboxes(mailboxes: &[Mailbox]) -> Vec<AccountGroup> {
    let mut groups: Vec<AccountGroup> = Vec::new();
    for m in mailboxes {
        match groups
            .iter_mut()
            .find(|g| g.connection_id == m.connection_id)
        {
            Some(g) => g.mailboxes.push(m.clone()),
            None => {
                let name = m.connection_name.trim();
                groups.push(AccountGroup {
                    connection_id: m.connection_id.clone(),
                    name: if name.is_empty() {
                        "Email account".to_string()
                    } else {
                        name.to_string()
                    },
                    mailboxes: vec![m.clone()],
                });
            }
        }
    }
    groups
}

/// A small unread-count pill, or nothing when the count is zero (an all-read
/// mailbox shows no badge rather than a `0`).
fn unread_badge(count: i64) -> Option<impl IntoView> {
    (count > 0).then(|| view! { <span class="email-badge">{count.to_string()}</span> })
}

/// The cross-folder badge for a message filed in more than one folder (SOUL §29):
/// given how many distinct folders share its `Message-ID` (`folder_count`) and the OTHER
/// folder names (`also_in`), returns `(label, tooltip)` for a small list-row / detail
/// badge, or `None` when the message is single-filed (`folder_count <= 1`). One named
/// other folder reads "also in <name>"; otherwise it collapses to "+N folders" with the
/// full list in the tooltip. Orientation only — clicking still opens the listed row.
fn cross_folder_badge(folder_count: usize, also_in: &[String]) -> Option<(String, String)> {
    if folder_count <= 1 {
        return None;
    }
    let others = folder_count - 1;
    let label = match (also_in, others) {
        ([only], 1) => format!("also in {only}"),
        (_, 1) => "+1 folder".to_string(),
        (_, n) => format!("+{n} folders"),
    };
    let tooltip = if also_in.is_empty() {
        format!(
            "Also filed in {others} other folder{}",
            if others == 1 { "" } else { "s" }
        )
    } else {
        format!("Also in: {}", also_in.join(", "))
    };
    Some((label, tooltip))
}

/// Render an email body: the HTML body when present (HTML mail is the message's
/// intended rendering), else the plain-text body, else a "(no body)" note.
///
/// The HTML branch is contained by three independent layers:
/// 1. the API already sanitized `body_html` through an allowlist (no scripts,
///    event handlers, forms, or dangerous URL schemes reach this client);
/// 2. it renders inside `<iframe sandbox>` WITHOUT `allow-scripts` or
///    `allow-same-origin` — the message gets an opaque origin: no cookies, no
///    session token, no parent DOM, no workbench API reach — and never touches
///    the page's own DOM (no `inner_html`);
/// 3. the frame document carries its own CSP ([`email_frame_doc`]) denying
///    every network load, so remote images / tracking pixels stay blocked
///    until the user presses "Load remote images" for this message.
///
/// Links open in a fresh tab (`<base target="_blank">` + the popup sandbox
/// tokens); nothing a message contains can navigate the workbench itself.
fn render_body(d: &EmailDetail) -> impl IntoView {
    let text = d
        .body_text
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .cloned();
    let html = d
        .body_html
        .as_ref()
        .filter(|s| !s.trim().is_empty())
        .cloned();
    match (html, text) {
        (Some(h), _) => {
            // Per-open opt-in; recreated (→ blocked again) whenever the detail
            // view re-renders for another message.
            let allow_remote = RwSignal::new(false);
            let srcdoc = move || email_frame_doc(&h, allow_remote.get());
            view! {
                <Show when=move || !allow_remote.get() fallback=|| ().into_view()>
                    <div class="email-remote-bar">
                        "Remote images are blocked."
                        <button
                            class="email-remote-btn"
                            type="button"
                            on:click=move |_| allow_remote.set(true)
                        >
                            "Load remote images"
                        </button>
                    </div>
                </Show>
                <iframe
                    class="email-html-frame"
                    title="Email message"
                    sandbox="allow-popups allow-popups-to-escape-sandbox"
                    referrerpolicy="no-referrer"
                    srcdoc=srcdoc
                ></iframe>
            }
            .into_any()
        }
        (None, Some(t)) => view! { <pre class="email-body">{t}</pre> }.into_any(),
        (None, None) => view! { <div class="email-empty">"(no body)"</div> }.into_any(),
    }
}

/// The complete `srcdoc` document wrapping a (server-sanitized) HTML email body.
///
/// It is the containment around the body, not the sanitizer: a `<meta>` CSP
/// denies every subresource except inline styles and `data:` images —
/// `allow_remote` additionally permits `http(s)` images for this render (the
/// user's per-message opt-in; flipping it swaps the whole frame document, since
/// a live document's CSP can't be relaxed) — `<base target="_blank">` sends
/// every link to a new tab instead of navigating inside the frame, and a
/// minimal default style keeps unstyled mail readable. The white canvas is
/// intentional and not themed: HTML mail is authored against a light background.
fn email_frame_doc(sanitized_html: &str, allow_remote: bool) -> String {
    let img_src = if allow_remote {
        "img-src https: http: data:"
    } else {
        "img-src data:"
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" \
         content=\"default-src 'none'; {img_src}; style-src 'unsafe-inline'\">\
         <base target=\"_blank\">\
         <style>\
         body {{ margin: .75rem; font: .92rem/1.55 system-ui, sans-serif; \
         color: #111; background: #fff; word-break: break-word; }} \
         img {{ max-width: 100%; height: auto; }} \
         pre {{ white-space: pre-wrap; }}\
         </style>\
         </head><body>{sanitized_html}</body></html>"
    )
}

/// Join a recipient list into a single `"A <a@x>, B <b@x>"` line.
fn join_addrs(addrs: &[crate::api::EmailAddress]) -> String {
    addrs
        .iter()
        .map(crate::api::EmailAddress::display)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Map the unread-filter select value to the API's `unread` query: `unread` →
/// `Some(true)`, `read` → `Some(false)`, anything else (`all`) → `None`.
fn unread_filter(value: &str) -> Option<bool> {
    match value {
        "unread" => Some(true),
        "read" => Some(false),
        _ => None,
    }
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

/// The object key inside an attachment's `/storage/objects/<key>` reference, or
/// `None` when the `url` isn't a workspace-store path (e.g. an external link).
/// Email archival always emits store refs; this guards the rare non-store case.
fn object_key_from_url(url: &str) -> Option<&str> {
    url.strip_prefix("/storage/objects/")
        .filter(|k| !k.is_empty())
}

/// The last `/`-separated segment of a path/URL (its basename), else `""`.
fn path_basename(s: &str) -> &str {
    s.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// A display name for an attachment: its declared filename, else the ref's
/// basename, else the raw ref.
fn attachment_display_name(att: &Attachment) -> String {
    if let Some(f) = att
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        return f.to_string();
    }
    let base = path_basename(&att.url);
    if base.is_empty() {
        att.url.clone()
    } else {
        base.to_string()
    }
}

/// The download filename for the archived raw message: the `raw_ref` key's
/// basename, else `original.eml`.
fn eml_filename(raw_ref: &str) -> String {
    let base = path_basename(raw_ref);
    if base.is_empty() {
        "original.eml".to_string()
    } else {
        base.to_string()
    }
}

/// A compact human size (`"1.5 KB"`), or exact bytes under 1 KiB (`"512 B"`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Standard base64 (RFC 4648, with `=` padding) of `bytes`. Used to build a
/// `data:` URL for a client-side download — the `Url`/`Blob` object-URL bindings
/// aren't in this crate's `web-sys` feature set, so a data URL is the
/// no-new-feature path to hand bytes to the browser's download.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Hand `bytes` to the browser as a file download named `filename`. Builds a
/// `data:` URL and clicks a detached `<a download>` — no `Url`/`Blob` object-URL
/// bindings needed (they're outside this crate's `web-sys` features). Returns an
/// error string on the (unexpected) DOM failure so the caller can surface it.
fn trigger_download(
    bytes: &[u8],
    filename: &str,
    content_type: Option<&str>,
) -> Result<(), String> {
    let mime = content_type
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("application/octet-stream");
    let data_url = format!("data:{mime};base64,{}", base64_encode(bytes));
    let document = web_sys::window()
        .and_then(|w| w.document())
        .ok_or("no document")?;
    let anchor = document
        .create_element("a")
        .map_err(|_| "could not create download link".to_string())?;
    anchor
        .set_attribute("href", &data_url)
        .map_err(|_| "could not set download link".to_string())?;
    anchor
        .set_attribute("download", filename)
        .map_err(|_| "could not set download name".to_string())?;
    let anchor: web_sys::HtmlElement = anchor
        .dyn_into()
        .map_err(|_| "download link is not clickable".to_string())?;
    anchor.click();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::EmailAddress;

    #[test]
    fn email_frame_doc_blocks_remote_loads_until_opt_in() {
        let doc = email_frame_doc("<p>hi</p>", false);
        // The body is embedded, wrapped by a deny-by-default CSP…
        assert!(doc.contains("<p>hi</p>"), "{doc}");
        assert!(doc.contains("default-src 'none'"), "{doc}");
        assert!(doc.contains("img-src data:"), "{doc}");
        assert!(
            !doc.contains("https:"),
            "no remote sources by default: {doc}"
        );
        // …and links leave the sandbox instead of navigating the frame.
        assert!(doc.contains(r#"<base target="_blank">"#), "{doc}");

        // The opt-in render additionally permits remote images — nothing else.
        let remote = email_frame_doc("<p>hi</p>", true);
        assert!(remote.contains("img-src https: http: data:"), "{remote}");
        assert!(remote.contains("default-src 'none'"), "{remote}");
    }

    #[test]
    fn cross_folder_badge_labels_and_tooltips() {
        // Single-filed → no badge.
        assert_eq!(cross_folder_badge(1, &[]), None);
        assert_eq!(cross_folder_badge(0, &["X".into()]), None);

        // Exactly one named other folder → "also in <name>".
        let (label, tip) = cross_folder_badge(2, &["Archive".into()]).unwrap();
        assert_eq!(label, "also in Archive");
        assert_eq!(tip, "Also in: Archive");

        // Several others → "+N folders" with the full list in the tooltip.
        let (label, tip) =
            cross_folder_badge(3, &["Archive".to_string(), "Sent".to_string()]).unwrap();
        assert_eq!(label, "+2 folders");
        assert_eq!(tip, "Also in: Archive, Sent");

        // Cross-filed but the other name(s) weren't carried (capped/omitted): count still
        // drives the label, and the tooltip counts the others.
        let (label, tip) = cross_folder_badge(2, &[]).unwrap();
        assert_eq!(label, "+1 folder");
        assert_eq!(tip, "Also filed in 1 other folder");
        let (label, tip) = cross_folder_badge(4, &[]).unwrap();
        assert_eq!(label, "+3 folders");
        assert_eq!(tip, "Also filed in 3 other folders");
    }

    fn mailbox(id: &str, conn: &str, conn_name: &str, name: &str, unread: i64) -> Mailbox {
        Mailbox {
            id: id.to_string(),
            workspace_id: "ws".to_string(),
            connection_id: conn.to_string(),
            connection_name: conn_name.to_string(),
            external_id: format!("ext-{id}"),
            name: name.to_string(),
            read_only: true,
            unread_count: unread,
        }
    }

    #[test]
    fn group_mailboxes_groups_by_connection_in_listing_order() {
        // Two accounts, interleaved in the listing: grouping keeps first-seen
        // connection order and in-listing mailbox order within each account.
        let groups = group_mailboxes(&[
            mailbox("m1", "c-work", "Work", "INBOX", 3),
            mailbox("m2", "c-home", "Home", "INBOX", 0),
            mailbox("m3", "c-work", "Work", "Archive", 1),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].connection_id, "c-work");
        assert_eq!(groups[0].name, "Work");
        assert_eq!(
            groups[0]
                .mailboxes
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["INBOX", "Archive"]
        );
        assert_eq!(groups[1].connection_id, "c-home");
        // Per-account unread is the sum of its mailboxes (the header badge).
        let work_unread: i64 = groups[0].mailboxes.iter().map(|m| m.unread_count).sum();
        assert_eq!(work_unread, 4);

        // An older server without `connection_name` gets a generic header.
        let unnamed = group_mailboxes(&[mailbox("m4", "c-x", "  ", "INBOX", 0)]);
        assert_eq!(unnamed[0].name, "Email account");

        // Empty in, empty out.
        assert!(group_mailboxes(&[]).is_empty());
    }

    #[test]
    fn unread_filter_maps_select_values() {
        assert_eq!(unread_filter("unread"), Some(true));
        assert_eq!(unread_filter("read"), Some(false));
        assert_eq!(unread_filter("all"), None);
        assert_eq!(unread_filter(""), None);
    }

    #[test]
    fn join_addrs_renders_named_and_bare() {
        let addrs = vec![
            EmailAddress {
                name: Some("Ada".into()),
                address: "ada@x.com".into(),
            },
            EmailAddress {
                name: None,
                address: "bob@x.com".into(),
            },
        ];
        assert_eq!(join_addrs(&addrs), "Ada <ada@x.com>, bob@x.com");
        assert_eq!(join_addrs(&[]), "");
    }

    #[test]
    fn fmt_ts_trims_to_minute() {
        assert_eq!(fmt_ts("2026-06-18T09:00:00Z"), "2026-06-18 09:00");
        assert_eq!(fmt_ts("nope"), "nope");
    }

    #[test]
    fn object_key_from_url_strips_store_prefix() {
        assert_eq!(
            object_key_from_url("/storage/objects/emails/mb1/42/attachments/1-a.pdf"),
            Some("emails/mb1/42/attachments/1-a.pdf"),
        );
        // An external link is not a stored object.
        assert_eq!(object_key_from_url("https://example.com/a.png"), None);
        // The bare prefix carries no key.
        assert_eq!(object_key_from_url("/storage/objects/"), None);
    }

    #[test]
    fn attachment_display_name_prefers_filename_then_basename() {
        let named = Attachment {
            url: "/storage/objects/emails/1/attachments/1-report.pdf".into(),
            filename: Some("Q3 Report.pdf".into()),
            content_type: None,
            size: None,
        };
        assert_eq!(attachment_display_name(&named), "Q3 Report.pdf");

        // No (or blank) filename → fall back to the ref's basename.
        let unnamed = Attachment {
            url: "/storage/objects/emails/1/attachments/2-logo.png".into(),
            filename: Some("   ".into()),
            content_type: None,
            size: None,
        };
        assert_eq!(attachment_display_name(&unnamed), "2-logo.png");
    }

    #[test]
    fn eml_filename_uses_basename_with_fallback() {
        assert_eq!(eml_filename("emails/mb1/42/raw.eml"), "raw.eml");
        assert_eq!(eml_filename(""), "original.eml");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1_048_576), "1.0 MB");
    }

    #[test]
    fn base64_encode_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
