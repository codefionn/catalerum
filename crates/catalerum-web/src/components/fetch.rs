//! The Fetch panel (SOUL §27, §12 — web-fetch utility).
//!
//! A single-pane utility: a form (URL + format/mode/main-content options) over
//! `POST /fetch`, and a result region showing the fetched page's metadata
//! (final URL, status, title, content type, conversion savings) and its content.
//! It is a thin client of the fetch route — the same scoped endpoint the LLM's
//! `fetch_url` tool uses (SOUL §7) — workspace-scoped + `web:read`-gated
//! server-side, with the SSRF guard in the backend (SOUL §19/§27).
//!
//! The content is always rendered as **escaped text** (a `<pre>`, never
//! `inner_html`), so even an `html`-format fetch can't execute markup in the
//! workbench — the page's bytes are shown, not run.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api::{FetchRequest, FetchedPage};
use crate::auth;
use crate::rest;

/// The Fetch panel component.
#[component]
pub fn FetchPanel() -> impl IntoView {
    let url = RwSignal::new(String::new());
    let format = RwSignal::new("markdown".to_string());
    let mode = RwSignal::new("auto".to_string());
    let main_only = RwSignal::new(true);

    let fetching = RwSignal::new(false);
    let error = RwSignal::new(Option::<String>::None);
    let page = RwSignal::new(Option::<FetchedPage>::None);

    let run = move || {
        if fetching.get_untracked() {
            return;
        }
        let u = url.get_untracked().trim().to_string();
        error.set(None);
        if u.is_empty() {
            error.set(Some("Enter a URL to fetch.".to_string()));
            return;
        }
        let body = FetchRequest {
            url: u,
            format: format.get_untracked(),
            mode: mode.get_untracked(),
            main_content_only: main_only.get_untracked(),
            wait_for: None,
            timeout_secs: None,
        };
        fetching.set(true);
        page.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::fetch_url(token.as_deref(), &body).await {
                Ok(p) => {
                    page.set(Some(p));
                    error.set(None);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            fetching.set(false);
        });
    };

    let on_submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        run();
    };

    view! {
        <section class="fetch-panel">
            <header class="fetch-header">
                <div class="fetch-header-titles">
                    <h2 class="fetch-title">"Fetch"</h2>
                    <span class="fetch-subtitle">"Retrieve a web page as Markdown, HTML, or text"</span>
                </div>
                <form class="fetch-form" on:submit=on_submit>
                    <input
                        class="fetch-url"
                        type="url"
                        placeholder="https://example.com/article"
                        disabled=move || fetching.get()
                        prop:value=move || url.get()
                        on:input=move |ev| url.set(event_target_value(&ev))
                    />
                    <div class="fetch-opts">
                        <select
                            class="fetch-select"
                            disabled=move || fetching.get()
                            on:change=move |ev| format.set(event_target_value(&ev))
                        >
                            <option value="markdown">"Markdown"</option>
                            <option value="html">"HTML"</option>
                            <option value="text">"Text"</option>
                        </select>
                        <select
                            class="fetch-select"
                            disabled=move || fetching.get()
                            on:change=move |ev| mode.set(event_target_value(&ev))
                        >
                            <option value="auto">"Auto"</option>
                            <option value="http">"HTTP"</option>
                            <option value="browser">"Browser"</option>
                        </select>
                        <label class="fetch-check">
                            <input
                                type="checkbox"
                                disabled=move || fetching.get()
                                prop:checked=move || main_only.get()
                                on:change=move |ev| main_only.set(event_target_checked(&ev))
                            />
                            "Main content only"
                        </label>
                        <button
                            class="fetch-btn"
                            type="submit"
                            disabled=move || fetching.get()
                        >
                            {move || if fetching.get() { "Fetching…" } else { "Fetch" }}
                        </button>
                    </div>
                </form>
            </header>

            <div class="fetch-body">
                <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                    <div class="fetch-status fetch-error">
                        {move || error.get().unwrap_or_default()}
                    </div>
                </Show>

                <Show
                    when=move || fetching.get() && page.with(Option::is_none)
                    fallback=|| ().into_view()
                >
                    <div class="fetch-status">"Fetching the page…"</div>
                </Show>

                <Show
                    when=move || {
                        !fetching.get() && page.with(Option::is_none) && error.with(Option::is_none)
                    }
                    fallback=|| ().into_view()
                >
                    <div class="fetch-status">"Enter a URL above and fetch a page to see it here."</div>
                </Show>

                {move || {
                    page.get().map(|p| {
                        let title = p.title.clone().unwrap_or_default();
                        let has_title = !title.trim().is_empty();
                        let ctype = p.content_type.clone().unwrap_or_default();
                        let has_ctype = !ctype.is_empty();
                        let final_url = p.url.clone();
                        let status = p.status;
                        let savings = savings_label(p.raw_bytes, p.content_bytes);
                        let content = p.content.clone();
                        let empty_content = content.trim().is_empty();
                        view! {
                            <article class="fetch-result">
                                <Show
                                    when=move || has_title
                                    fallback=|| ().into_view()
                                >
                                    <h3 class="fetch-result-title">{title.clone()}</h3>
                                </Show>
                                <div class="fetch-result-meta">
                                    <span class=move || {
                                        format!("fetch-status-pill {}", status_class(status))
                                    }>
                                        {format!("HTTP {status}")}
                                    </span>
                                    <a
                                        class="fetch-result-url"
                                        href=final_url.clone()
                                        target="_blank"
                                        rel="noopener"
                                    >
                                        {final_url.clone()}
                                    </a>
                                    <Show
                                        when=move || has_ctype
                                        fallback=|| ().into_view()
                                    >
                                        <span class="fetch-result-ctype">{ctype.clone()}</span>
                                    </Show>
                                    <Show
                                        when={
                                            let has = savings.is_some();
                                            move || has
                                        }
                                        fallback=|| ().into_view()
                                    >
                                        <span class="fetch-result-savings">
                                            {savings.clone().unwrap_or_default()}
                                        </span>
                                    </Show>
                                </div>
                                <Show
                                    when=move || empty_content
                                    fallback=move || {
                                        view! { <pre class="fetch-content">{content.clone()}</pre> }
                                    }
                                >
                                    <div class="fetch-status">"(empty page)"</div>
                                </Show>
                            </article>
                        }
                    })
                }}
            </div>
        </section>
    }
}

/// A CSS class for the status pill by HTTP status class (2xx ok, 3xx redirect,
/// else error).
fn status_class(status: u16) -> &'static str {
    match status {
        200..=299 => "fetch-status-ok",
        300..=399 => "fetch-status-redir",
        _ => "fetch-status-bad",
    }
}

/// A human label for how much the HTML→content conversion saved, e.g.
/// `"3.2 KiB ← 48.0 KiB (7%)"`. `None` when the original size is unknown
/// (`raw_bytes == 0`, e.g. a backend that returns Markdown directly).
fn savings_label(raw_bytes: u64, content_bytes: u64) -> Option<String> {
    if raw_bytes == 0 {
        return None;
    }
    let pct = ((content_bytes as f64 / raw_bytes as f64) * 100.0).round() as u64;
    Some(format!(
        "{} ← {} ({}%)",
        human_bytes(content_bytes),
        human_bytes(raw_bytes),
        pct.min(100),
    ))
}

/// Render a byte count as a compact human-readable size (binary units).
fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_class_buckets() {
        assert_eq!(status_class(200), "fetch-status-ok");
        assert_eq!(status_class(301), "fetch-status-redir");
        assert_eq!(status_class(404), "fetch-status-bad");
        assert_eq!(status_class(500), "fetch-status-bad");
    }

    #[test]
    fn human_bytes_binary_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn savings_label_none_when_raw_unknown() {
        assert_eq!(savings_label(0, 100), None);
        let s = savings_label(48 * 1024, 3 * 1024).unwrap();
        assert!(s.contains("3.0 KiB"));
        assert!(s.contains("48.0 KiB"));
        assert!(s.contains("6%"), "{s}");
    }
}
