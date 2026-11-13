use super::*;

/// The Chat panel's base frontend route. An open conversation is deep-linkable
/// at `<CHAT_ROUTE>/<id>`; a fresh, not-yet-persisted chat sits at the bare
/// route.
pub(super) const CHAT_ROUTE: &str = "/app/chat";
const CHAT_OUTBOX_STORAGE_KEY: &str = "catalerum_chat_outbox_v1";
/// Treat the final few pixels as the bottom. Fractional layout pixels and touch
/// momentum can otherwise leave the pane one pixel short and unexpectedly stop
/// following a streamed reply.
pub(super) const CHAT_BOTTOM_SLOP_PX: i32 = 24;

pub(super) fn chat_is_at_bottom(scroll_top: i32, scroll_height: i32, client_height: i32) -> bool {
    scroll_height - client_height - scroll_top <= CHAT_BOTTOM_SLOP_PX
}

#[cfg(target_arch = "wasm32")]
pub(super) fn fresh_uuid() -> String {
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .map(|c| c.random_uuid())
        .unwrap_or_else(|| {
            let mut hex = String::with_capacity(32);
            for _ in 0..32 {
                let n = (js_sys::Math::random() * 16.0) as u8;
                hex.push(char::from_digit(u32::from(n), 16).unwrap_or('0'));
            }
            format!(
                "{}-{}-4{}-a{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[13..16],
                &hex[17..20],
                &hex[20..32]
            )
        })
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn fresh_uuid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "00000000-0000-4000-a000-{:012x}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn read_chat_outbox() -> Vec<ClientChatMessage> {
    let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    else {
        return Vec::new();
    };
    storage
        .get_item(CHAT_OUTBOX_STORAGE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_chat_outbox(entries: &[ClientChatMessage]) {
    let Some(storage) = web_sys::window()
        .and_then(|w| w.local_storage().ok())
        .flatten()
    else {
        return;
    };
    if entries.is_empty() {
        let _ = storage.remove_item(CHAT_OUTBOX_STORAGE_KEY);
    } else if let Ok(raw) = serde_json::to_string(entries) {
        let _ = storage.set_item(CHAT_OUTBOX_STORAGE_KEY, &raw);
    }
}

pub(super) fn put_chat_outbox(message: &ClientChatMessage) {
    let Some(id) = message.user_message_id.as_deref() else {
        return;
    };
    let mut entries = read_chat_outbox();
    if let Some(existing) = entries
        .iter_mut()
        .find(|m| m.user_message_id.as_deref() == Some(id))
    {
        *existing = message.clone();
    } else {
        entries.push(message.clone());
    }
    write_chat_outbox(&entries);
}

pub(super) fn remove_chat_outbox(message_id: &str) {
    let mut entries = read_chat_outbox();
    entries.retain(|m| m.user_message_id.as_deref() != Some(message_id));
    write_chat_outbox(&entries);
}

pub(super) fn chat_outbox_contains(message_id: &str) -> bool {
    read_chat_outbox()
        .iter()
        .any(|m| m.user_message_id.as_deref() == Some(message_id))
}

/// The reasoning ("thinking") effort a brand-new chat starts at. It seeds the
/// pre-send picker, so the first send binds it onto the fresh conversation like
/// any other pick — choosing "Off" before sending still wins (nothing binds and
/// the thread stays at the provider default). Existing threads are untouched.
pub(super) const DEFAULT_REASONING_EFFORT: &str = "low";

/// The conversation id encoded in the current browser URL (`/app/chat/<id>`),
/// if the path carries one. Seeds the open thread from a deep link or reload; a
/// bare `/app/chat` yields `None` (a fresh chat).
pub(super) fn session_from_location() -> Option<String> {
    let path = web_sys::window()?.location().pathname().ok()?;
    let id = path
        .trim_end_matches('/')
        .strip_prefix(CHAT_ROUTE)?
        .trim_start_matches('/');
    (!id.is_empty()).then(|| id.to_string())
}

/// Reflect the open conversation in the browser URL: `/app/chat/<id>` for a
/// saved thread, or the bare `/app/chat` for a fresh/unsaved one. Uses
/// `replace_state` (not push) so switching threads tracks the address bar
/// without stacking per-thread history entries — browser Back leaves Chat
/// rather than cycling through visited threads. No-op when already at the URL.
pub(super) fn sync_location_to_session(id: Option<&str>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let target = match id {
        Some(id) => format!("{CHAT_ROUTE}/{id}"),
        None => CHAT_ROUTE.to_string(),
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
