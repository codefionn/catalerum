use super::*;

/// One user-turn attachment (SOUL §9) as it rides on **top** of a chat bubble: an
/// inline thumbnail for an image, a labelled download chip for anything else, or
/// inert text when the URL uses an unsafe scheme (the XSS gate). The href is
/// resolved via [`attachment_href`] so an uploaded object carries its auth token.
/// Mirrors the calendar event attachment view — both lean on the same shared
/// [`crate::components::widgets`] helpers.
pub(super) fn message_attachment_view(att: &Attachment) -> AnyView {
    let href = attachment_href(att);
    let name = attachment_label(att);
    let safe = is_safe_href(&href);
    if safe && attachment_is_image(att) {
        let alt = name.clone();
        let img_src = href.clone();
        view! {
            <a
                class="msg-attachment msg-attachment-img-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=name
            >
                <img class="msg-attachment-img" src=img_src alt=alt loading="lazy" />
            </a>
        }
        .into_any()
    } else if safe {
        let title = name.clone();
        view! {
            <a
                class="msg-attachment msg-attachment-link"
                href=href
                target="_blank"
                rel="noreferrer"
                title=title
            >
                <span class="msg-attachment-icon"><Icon icon=MdIcon::File /></span>
                <span class="msg-attachment-name">{name}</span>
            </a>
        }
        .into_any()
    } else {
        // Unsafe scheme (e.g. `javascript:`): render inert — never a clickable link.
        view! {
            <span class="msg-attachment msg-attachment-unsafe" title=att.url.clone()>
                <span class="msg-attachment-icon"><Icon icon=MdIcon::Warning /></span>
                <span class="msg-attachment-name">{name}</span>
            </span>
        }
        .into_any()
    }
}

/// Flag a streaming line as an error line (used when a turn fails before any
/// text arrives).
pub(super) fn mark_error(lines: RwSignal<Vec<ChatLine>>, id: usize) {
    lines.update(|v| {
        if let Some(l) = v.iter_mut().find(|l| l.id == id) {
            l.role = Role::Error;
        }
    });
}

/// Whether a tool result string looks like a failure. Tool errors are persisted
/// as a `{"error": "…"}` JSON object in the result row's content (the backend has
/// no separate error flag before the §Phase-3 migration), so this is a heuristic:
/// a JSON object with a top-level `error` key. A tool that legitimately returns an
/// `error` field is a false positive — accepted until the persisted flag lands.
pub(super) fn looks_like_error(content: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(content.trim())
        .ok()
        .and_then(|v| v.get("error").map(|_| ()))
        .is_some()
}

/// App authoring calls mount the affected definition inline; they do not also
/// need a generic tool card. Keep live streaming and replay on one predicate.
pub(super) fn is_ui_authoring_tool(name: &str) -> bool {
    matches!(
        name,
        "present_ui" | "create_ui_components" | "edit_ui_components" | "edit_ui"
    )
}

/// A short label for a tool call's status glyph (live spinner / ✓ / ✗).
pub(super) fn tool_status_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "spinner",
        ToolStatus::Ok => "✓",
        ToolStatus::Err => "✗",
    }
}

/// Format a tool call's duration as a compact readout (e.g. `412ms`, `1.8s`).
pub(super) fn fmt_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Format a per-turn LLM cost as a compact USD readout for the chat bubble. A
/// nonzero cost below the 4-decimal resolution shows as `<$0.0001` rather than a
/// misleading `$0.0000`.
pub(super) fn fmt_cost(usd: f64) -> String {
    if usd > 0.0 && usd < 0.0001 {
        "<$0.0001".to_string()
    } else {
        format!("${usd:.4}")
    }
}

/// Group a token count into thousands (`12345` → `12,345`) for the readouts.
pub(super) fn fmt_int(n: u32) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len + (len.saturating_sub(1)) / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(b as char);
    }
    out
}

/// Build the hover tooltip for a turn's token info-icon: this turn's input/output
/// split, a cache line (only when the provider cached anything), and the running
/// conversation total up to and including this turn. Newline-separated so the
/// native `title` tooltip renders it as multiple lines.
pub(super) fn fmt_token_tooltip(t: &TurnTokens, cumulative_total: u32) -> String {
    let mut s = format!(
        "This turn: {} in + {} out = {} tokens",
        fmt_int(t.prompt_tokens),
        fmt_int(t.completion_tokens),
        fmt_int(t.total_tokens),
    );
    if t.cached_tokens > 0 || t.cache_creation_tokens > 0 {
        s.push_str(&format!(
            "\nCache: {} read · {} written",
            fmt_int(t.cached_tokens),
            fmt_int(t.cache_creation_tokens),
        ));
    }
    s.push_str(&format!(
        "\nTotal up to here: {} tokens",
        fmt_int(cumulative_total)
    ));
    s
}
