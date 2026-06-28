//! The Conversations panel (SOUL §12 — conversation history browser).
//!
//! A two-pane read view: a left list of the workspace's conversations (newest
//! first) and a right transcript of the selected one's messages (replay order).
//! It is a thin client of the conversation REST surface (`/conversations`,
//! `/conversations/{id}/messages`) — every call carries the dev session token and
//! is workspace-scoped server-side (SOUL §18).
//!
//! Read-only browsing: it surfaces threads created by the Chat panel, inbound
//! channels (§25), and MCP clients (§26) alike, with each message's role, text,
//! and any assistant tool calls. A selected thread can be **resumed in the live
//! Chat panel** via the transcript header's "Resume in Chat" button, which hands
//! the conversation id to the shell's one-shot `resume` signal (the shell then
//! brings the Chat panel forward and it replays + continues the thread).

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::widgets::{list_drawer_scrim, list_drawer_toggle};
use crate::api::{Conversation, Message, MessageHit};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// The Conversations panel component. `resume` is the shell's one-shot
/// "open this conversation in Chat" channel, set by the transcript header button.
#[component]
pub fn ConversationsPanel(resume: RwSignal<Option<String>>) -> impl IntoView {
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(Option::<String>::None);

    // The open conversation + its transcript.
    let selected_id = RwSignal::new(Option::<String>::None);
    let messages = RwSignal::new(Vec::<Message>::new());
    let messages_loading = RwSignal::new(false);
    let messages_error = RwSignal::new(Option::<String>::None);

    // Whether the list is open as a mobile drawer (SOUL §12); inert on desktop.
    let list_open = RwSignal::new(false);

    // Fetch a conversation's transcript.
    let open_conversation = move |id: String| {
        selected_id.set(Some(id.clone()));
        // Reveal the transcript by closing the mobile list drawer (no-op on desktop).
        list_open.set(false);
        messages_loading.set(true);
        messages_error.set(None);
        messages.set(Vec::new());
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_messages(token.as_deref(), &id).await {
                Ok(list) => {
                    messages.set(list);
                    messages_error.set(None);
                }
                Err(e) => messages_error.set(Some(e.to_string())),
            }
            messages_loading.set(false);
        });
    };

    // Fetch the conversation list; auto-open the first on first paint.
    let refresh = move || {
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_conversations(token.as_deref()).await {
                Ok(list) => {
                    if selected_id.get_untracked().is_none() {
                        if let Some(first) = list.first() {
                            open_conversation(first.id.clone());
                        }
                    }
                    conversations.set(list);
                    load_error.set(None);
                }
                Err(e) => {
                    conversations.set(Vec::new());
                    load_error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    refresh();

    // Content search across the workspace's messages.
    let search_query = RwSignal::new(String::new());
    let search_results = RwSignal::new(Vec::<MessageHit>::new());
    let searching = RwSignal::new(false);
    let search_error = RwSignal::new(Option::<String>::None);
    // True once a search has run — switches the left pane from the conversation
    // list to the results list (until cleared).
    let search_active = RwSignal::new(false);

    let run_search = move || {
        let q = search_query.get_untracked().trim().to_string();
        if q.is_empty() {
            // Empty query clears back to the conversation list.
            search_active.set(false);
            search_results.set(Vec::new());
            search_error.set(None);
            return;
        }
        searching.set(true);
        search_active.set(true);
        search_error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::search_messages(token.as_deref(), &q).await {
                Ok(hits) => {
                    search_results.set(hits);
                    search_error.set(None);
                }
                Err(e) => {
                    search_results.set(Vec::new());
                    search_error.set(Some(e.to_string()));
                }
            }
            searching.set(false);
        });
    };
    let clear_search = move || {
        search_query.set(String::new());
        search_active.set(false);
        search_results.set(Vec::new());
        search_error.set(None);
    };
    let on_search_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        run_search();
    };

    view! {
        <section class="pane-split">
            {list_drawer_scrim(list_open)}
            <aside class="pane-list list-drawer" class:list-drawer-open=move || list_open.get()>
                <header class="pane-list-header">
                    <h2 class="pane-list-title">"Conversations"</h2>
                    <button
                        class="pane-btn"
                        disabled=move || loading.get()
                        on:click=move |_| refresh()
                    >
                        "Refresh"
                    </button>
                </header>

                <div class="pane-list-body">
                    <form class="conv-search" on:submit=on_search_submit>
                        <input
                            class="conv-search-input"
                            placeholder="Search message text…"
                            prop:value=move || search_query.get()
                            on:input=move |ev| search_query.set(event_target_value(&ev))
                        />
                        <Show
                            when=move || search_active.get()
                            fallback=|| ().into_view()
                        >
                            <button
                                class="conv-search-clear"
                                type="button"
                                title="Clear search"
                                on:click=move |_| clear_search()
                            >
                                <Icon icon=MdIcon::Close />
                            </button>
                        </Show>
                    </form>

                    // --- Search results (shown once a search has run) ----------
                    <Show when=move || search_active.get() fallback=|| ().into_view()>
                        <Show when=move || searching.get() fallback=|| ().into_view()>
                            <div class="pane-list-status">"Searching…"</div>
                        </Show>
                        <Show
                            when=move || search_error.with(Option::is_some)
                            fallback=|| ().into_view()
                        >
                            <div class="pane-list-status pane-list-error">
                                {move || {
                                    format!(
                                        "Search failed: {}",
                                        search_error.get().unwrap_or_default(),
                                    )
                                }}
                            </div>
                        </Show>
                        <Show
                            when=move || {
                                !searching.get()
                                    && search_error.with(Option::is_none)
                                    && search_results.with(Vec::is_empty)
                            }
                            fallback=|| ().into_view()
                        >
                            <div class="pane-list-status">"No messages match."</div>
                        </Show>
                        <ul class="pane-items">
                            <For
                                each=move || search_results.get()
                                key=|h| h.message.id.clone()
                                children=move |h: MessageHit| {
                                    let title = match &h.conversation_title {
                                        Some(t) if !t.trim().is_empty() => t.clone(),
                                        _ => "(untitled)".to_string(),
                                    };
                                    let role = h.message.role.clone();
                                    let snip = snippet(
                                        &h.message.content,
                                        &search_query.get_untracked(),
                                        120,
                                    );
                                    let conv_id = h.message.conversation_id.clone();
                                    view! {
                                        <li>
                                            <button
                                                class="pane-item conv-hit"
                                                on:click=move |_| {
                                                    open_conversation(conv_id.clone())
                                                }
                                            >
                                                <span class="conv-hit-head">
                                                    <span class="pane-item-title">{title}</span>
                                                    <span class="conv-hit-role">{role}</span>
                                                </span>
                                                <span class="conv-hit-snippet">{snip}</span>
                                            </button>
                                        </li>
                                    }
                                }
                            />
                        </ul>
                    </Show>

                    // --- Conversation list (when not searching) ----------------
                    <Show when=move || !search_active.get() fallback=|| ().into_view()>
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
                                    "Could not load conversations: {}",
                                    load_error.get().unwrap_or_default(),
                                )
                            }}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !loading.get()
                                && load_error.with(Option::is_none)
                                && conversations.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No conversations yet."</div>
                    </Show>

                    <ul class="pane-items">
                        <For
                            each=move || conversations.get()
                            key=|c| c.id.clone()
                            children=move |c: Conversation| {
                                let id = c.id.clone();
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
                                let title = match &c.title {
                                    Some(t) if !t.trim().is_empty() => t.clone(),
                                    _ => "(untitled)".to_string(),
                                };
                                let origin = c.origin.clone();
                                let id_for_click = id.clone();
                                view! {
                                    <li>
                                        <button
                                            class=class
                                            on:click=move |_| {
                                                open_conversation(id_for_click.clone())
                                            }
                                        >
                                            <span class="pane-item-title">{title}</span>
                                            <Show
                                                when={
                                                    let has = !origin.is_empty();
                                                    move || has
                                                }
                                                fallback=|| ().into_view()
                                            >
                                                <span class="conv-item-origin">
                                                    {origin.clone()}
                                                </span>
                                            </Show>
                                        </button>
                                    </li>
                                }
                            }
                        />
                    </ul>
                    </Show>
                </div>
            </aside>

            {list_drawer_toggle("Conversations", list_open)}
            <div class="conv-transcript">
                <Show
                    when=move || selected_id.get().is_some()
                    fallback=|| {
                        view! {
                            <div class="panel-placeholder">
                                <p>"Select a conversation to read its transcript."</p>
                            </div>
                        }
                    }
                >
                    <div class="conv-transcript-head">
                        <button
                            class="pane-btn conv-resume"
                            title="Open this conversation in the Chat panel to continue it"
                            on:click=move |_| {
                                if let Some(id) = selected_id.get() {
                                    resume.set(Some(id));
                                }
                            }
                        >
                            "Resume in Chat"
                        </button>
                    </div>

                    <Show when=move || messages_loading.get() fallback=|| ().into_view()>
                        <div class="pane-list-status">"Loading messages…"</div>
                    </Show>

                    <Show
                        when=move || messages_error.with(Option::is_some)
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status pane-list-error">
                            {move || messages_error.get().unwrap_or_default()}
                        </div>
                    </Show>

                    <Show
                        when=move || {
                            !messages_loading.get()
                                && messages_error.with(Option::is_none)
                                && messages.with(Vec::is_empty)
                        }
                        fallback=|| ().into_view()
                    >
                        <div class="pane-list-status">"No messages in this conversation."</div>
                    </Show>

                    <div class="conv-messages">
                        <For
                            each=move || messages.get()
                            key=|m| m.id.clone()
                            children=move |m: Message| {
                                let role = m.role.clone();
                                let role_class = format!("conv-msg conv-msg-{}", role_token(&role));
                                let badge_class =
                                    format!("conv-role conv-role-{}", role_token(&role));
                                let when = fmt_ts(&m.created_at);
                                let content = m.content.clone();
                                let has_content = !content.trim().is_empty();
                                let tool_calls = m.tool_calls.clone();
                                let has_tools = !tool_calls.is_empty();
                                view! {
                                    <div class=role_class>
                                        <div class="conv-msg-head">
                                            <span class=badge_class>{role}</span>
                                            <span class="conv-msg-when">{when}</span>
                                        </div>
                                        <Show
                                            when=move || has_content
                                            fallback=|| ().into_view()
                                        >
                                            <div class="conv-msg-text">{content.clone()}</div>
                                        </Show>
                                        <Show when=move || has_tools fallback=|| ().into_view()>
                                            <ul class="conv-tools">
                                                {tool_calls
                                                    .iter()
                                                    .map(|t| {
                                                        let line = format!("→ {}({})", t.name, t.arguments);
                                                        view! { <li class="conv-tool">{line}</li> }
                                                    })
                                                    .collect::<Vec<_>>()}
                                            </ul>
                                        </Show>
                                    </div>
                                }
                            }
                        />
                    </div>
                </Show>
            </div>
        </section>
    }
}

/// The CSS/token suffix for a message role (`user`/`assistant`/`system`/`tool`,
/// else `other` for an unrecognized role).
fn role_token(role: &str) -> &'static str {
    match role {
        "user" => "user",
        "assistant" => "assistant",
        "system" => "system",
        "tool" => "tool",
        _ => "other",
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

/// A short preview of `content` centred on the first case-insensitive match of
/// `query`, capped at `max` characters with leading/trailing ellipses when
/// truncated. Operates on `char`s so it never splits a UTF-8 codepoint; an empty
/// query (or no match) yields a head snippet.
fn snippet(content: &str, query: &str, max: usize) -> String {
    let chars: Vec<char> = content.chars().collect();
    if chars.len() <= max {
        return content.to_string();
    }
    let q = query.trim().to_lowercase();
    let match_char = if q.is_empty() {
        0
    } else {
        content
            .to_lowercase()
            .find(&q)
            .map_or(0, |byte| content[..byte].chars().count())
    };
    // Window of `max` chars, biased to show context before the match, re-anchored
    // so it never runs past the end.
    let ctx = max / 3;
    let mut start = match_char.saturating_sub(ctx);
    let end = (start + max).min(chars.len());
    start = end.saturating_sub(max);
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&chars[start..end]);
    if end < chars.len() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_centres_on_the_match_and_ellipsizes() {
        // Short content is returned whole, no ellipses.
        assert_eq!(snippet("hello world", "world", 50), "hello world");
        // A match deep in long content yields a centred, ellipsized window.
        let long = format!("{}NEEDLE{}", "a".repeat(60), "b".repeat(60));
        let s = snippet(&long, "needle", 30);
        assert!(s.contains("NEEDLE"), "kept the match: {s}");
        assert!(s.starts_with('…') && s.ends_with('…'), "ellipsized: {s}");
        assert!(s.chars().count() <= 32, "bounded (+ ellipses): {s}");
        // No match → a head snippet (trailing ellipsis only).
        let head = snippet(&"x".repeat(100), "zzz", 20);
        assert!(!head.starts_with('…') && head.ends_with('…'));
        assert!(head.chars().count() <= 21);
    }

    #[test]
    fn role_token_maps_known_and_unknown() {
        assert_eq!(role_token("user"), "user");
        assert_eq!(role_token("assistant"), "assistant");
        assert_eq!(role_token("system"), "system");
        assert_eq!(role_token("tool"), "tool");
        assert_eq!(role_token("function"), "other");
    }

    #[test]
    fn fmt_ts_trims_to_minute() {
        assert_eq!(fmt_ts("2026-06-18T09:00:00Z"), "2026-06-18 09:00");
        assert_eq!(fmt_ts("nope"), "nope");
    }
}
