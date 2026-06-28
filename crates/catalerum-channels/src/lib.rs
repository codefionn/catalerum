//! catalerum-channels — bidirectional Matrix / Telegram / Discord / Slack
//! integrations behind one [`Channel`] trait (SOUL §25).
//!
//! **Outbound delivery** — the [`Channel`] trait's `send` (which powers the
//! `Notify` automation action §11 / the `notify` tool §7) across four providers:
//! webhook senders [`discord::DiscordWebhookChannel`] / [`slack::SlackWebhookChannel`]
//! and token senders [`telegram::TelegramChannel`] / [`matrix::MatrixChannel`],
//! routed by name.
//!
//! **Inbound receive** — [`Channel::subscribe`] yields an [`InMessage`] stream that
//! the dispatch layer turns into `ChannelMessage` triggers (§11), so you can chat
//! with catalerum from your messenger and an agent replies on the same room/chat
//! (the multiplayer loop). Webhook channels (Discord/Slack) are outbound-only and
//! default to an empty stream; [`matrix::MatrixChannel`] (`/sync`) and
//! [`telegram::TelegramChannel`] (`getUpdates`) implement real long-poll receive.

pub mod discord;
pub mod matrix;
pub mod slack;
pub mod telegram;

use async_trait::async_trait;
use futures::stream::BoxStream;

pub use discord::DiscordWebhookChannel;
pub use matrix::MatrixChannel;
pub use slack::SlackWebhookChannel;
pub use telegram::TelegramChannel;

/// The default outbound HTTP client for channel senders: a short connect timeout
/// plus a bounded overall request timeout, so a hung messenger API (Telegram or
/// Discord) can't stall the notification path (`Notify` action §11 / `notify`
/// tool §7) indefinitely. The payloads are small JSON, so a 30 s overall cap is
/// generous. On the (practically impossible) builder failure, falls back to the
/// untimed default — matching `reqwest::Client::new()`'s own behaviour.
pub(crate) fn default_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Truncate `text` to at most `max_chars` Unicode scalar values, appending a
/// short marker when it had to cut so the result still fits `max_chars`.
///
/// Messenger APIs reject an over-long message with a `400`, which would silently
/// drop the **whole** notification — Discord caps webhook `content` at 2000 and
/// Telegram caps `text` at 4096. Capping before the send delivers at least the
/// head of a long agent message (a summary/digest easily exceeds 2000) instead
/// of losing it entirely. Cutting on a `char` boundary keeps the UTF-8 valid.
///
/// The cap is measured in `char`s (Unicode scalar values): exact for Discord
/// (which counts code points) and for all-BMP text on Telegram (whose limit is
/// in UTF-16 units, where a non-BMP char costs 2 — realistic notification text
/// is far under either limit, so the approximation is harmless in practice).
pub(crate) fn truncate_message(text: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    if text.chars().count() <= max_chars {
        return std::borrow::Cow::Borrowed(text);
    }
    const MARKER: &str = " […]";
    let keep = max_chars.saturating_sub(MARKER.chars().count());
    let mut out: String = text.chars().take(keep).collect();
    out.push_str(MARKER);
    std::borrow::Cow::Owned(out)
}

/// Like [`truncate_message`] but measuring the cap in **UTF-16 code units** — the
/// unit Telegram's `sendMessage` `text` limit (4096) is actually counted in, where
/// a non-BMP `char` (e.g. an emoji) costs 2. Counting in `char`s would let an
/// emoji-heavy message up to ~2× the cap slip past and be **rejected wholesale**
/// by Telegram (a `400` that drops the entire notification/reply); this keeps the
/// truncated result within the real limit. Cuts on a `char` boundary (valid UTF-8).
pub(crate) fn truncate_message_utf16(text: &str, max_units: usize) -> std::borrow::Cow<'_, str> {
    if text.chars().map(char::len_utf16).sum::<usize>() <= max_units {
        return std::borrow::Cow::Borrowed(text);
    }
    const MARKER: &str = " […]"; // all-BMP → 4 UTF-16 units
    let budget = max_units.saturating_sub(MARKER.encode_utf16().count());
    let mut out = String::new();
    let mut units = 0usize;
    for ch in text.chars() {
        let w = ch.len_utf16();
        if units + w > budget {
            break;
        }
        out.push(ch);
        units += w;
    }
    out.push_str(MARKER);
    std::borrow::Cow::Owned(out)
}

/// Like [`truncate_message`] but measuring the cap in **UTF-8 bytes** — the unit
/// Matrix's per-event (PDU) 65536-byte limit is counted in, where a non-ASCII
/// `char` costs 2–4 bytes. Counting in `char`s would let a CJK/emoji-heavy
/// message up to ~4× the byte budget slip past and be **rejected wholesale** by
/// the homeserver (dropping the whole notification/reply). Cuts on a `char`
/// boundary so the result stays valid UTF-8 and within `max_bytes`.
pub(crate) fn truncate_message_bytes(text: &str, max_bytes: usize) -> std::borrow::Cow<'_, str> {
    if text.len() <= max_bytes {
        return std::borrow::Cow::Borrowed(text);
    }
    const MARKER: &str = " […]"; // 6 UTF-8 bytes
    let budget = max_bytes.saturating_sub(MARKER.len());
    let mut end = 0usize;
    for (i, ch) in text.char_indices() {
        if i + ch.len_utf8() > budget {
            break;
        }
        end = i + ch.len_utf8();
    }
    let mut out = String::with_capacity(end + MARKER.len());
    out.push_str(&text[..end]);
    out.push_str(MARKER);
    std::borrow::Cow::Owned(out)
}

/// A channel delivery error.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// The request could not be sent (DNS, TLS, connection, timeout).
    #[error("channel request failed: {0}")]
    Request(String),
    /// The channel returned a non-success HTTP status.
    #[error("channel returned status {status}: {body}")]
    Status { status: u16, body: String },
}

/// Result over a [`ChannelError`].
pub type Result<T> = std::result::Result<T, ChannelError>;

/// An outbound message to deliver (SOUL §25). Text-only for now; rich content
/// (embeds, attachments) layers on later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutMessage {
    pub text: String,
}

impl OutMessage {
    /// A plain-text message.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// An inbound message received on a channel (SOUL §25). The dispatch layer turns
/// one into a `ChannelMessage` trigger (§11); `sender` lets an automation/agent
/// know **which participant** in a multi-party room spoke (multiplayer), and
/// `source` is the room/chat it arrived in (so a reply can target it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InMessage {
    /// Provider-native sender id (e.g. a Matrix `@user:hs`, a Telegram user id).
    pub sender: String,
    /// The message body.
    pub text: String,
    /// Provider-native source the message arrived in (room id / chat id).
    pub source: String,
    /// Provider-native message id, for dedup/threading. `None` if unavailable.
    pub message_id: Option<String>,
}

/// A messaging integration that can **deliver** and (optionally) **receive**
/// messages (SOUL §25). The destination is configured per channel instance (e.g.
/// a Discord webhook URL, a Matrix room + access token).
#[async_trait]
pub trait Channel: Send + Sync {
    /// A stable kind label (`"discord"`, `"telegram"`, …) for logs + audit.
    fn kind(&self) -> &str;

    /// Deliver `msg` to this channel's configured destination.
    ///
    /// # Errors
    /// [`ChannelError`] if the request fails or the channel rejects it.
    async fn send(&self, msg: &OutMessage) -> Result<()>;

    /// Subscribe to inbound messages on this channel (SOUL §25). Each yielded
    /// [`InMessage`] is a message that arrived **after** subscription.
    ///
    /// The default is an **empty** stream: a webhook-only channel (a Discord or
    /// Slack incoming webhook) cannot receive, so it never yields. Channels that
    /// can listen — [`matrix::MatrixChannel`] (`/sync`),
    /// [`telegram::TelegramChannel`] (`getUpdates`) — override this with a real
    /// long-poll stream.
    ///
    /// # Errors
    /// [`ChannelError`] if the initial subscribe handshake fails.
    async fn subscribe(&self) -> Result<BoxStream<'static, Result<InMessage>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::{truncate_message, truncate_message_bytes, truncate_message_utf16};

    #[test]
    fn short_text_passes_through_unborrowed() {
        let m = truncate_message("hello", 2000);
        assert_eq!(m, "hello");
        assert!(matches!(m, std::borrow::Cow::Borrowed(_)), "no allocation");
        // Exactly at the limit is not truncated.
        let exact = "x".repeat(10);
        assert_eq!(truncate_message(&exact, 10), exact);
    }

    #[test]
    fn over_limit_is_cut_with_marker_and_fits() {
        let long = "a".repeat(3000);
        let out = truncate_message(&long, 2000);
        assert!(out.ends_with(" […]"), "marker appended");
        assert_eq!(
            out.chars().count(),
            2000,
            "result fits exactly within the cap"
        );
        assert!(out.starts_with("aaaa"), "keeps the head");
    }

    #[test]
    fn cuts_on_a_char_boundary_for_multibyte_text() {
        // Each `é` is 2 UTF-8 bytes; a naive byte cut would split one. 50 chars
        // capped to 10 must stay valid UTF-8 and be 10 chars (marker included).
        let s = "é".repeat(50);
        let out = truncate_message(&s, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with(" […]"));
        // `String` is always valid UTF-8 — the assert is that we didn't panic
        // slicing mid-codepoint (a byte cut would have).
    }

    #[test]
    fn utf16_truncation_keeps_emoji_within_the_real_limit() {
        // Each 😀 is one `char` but **two** UTF-16 units. 3000 of them = 3000 chars
        // but 6000 UTF-16 units — over Telegram's 4096-unit cap, so char-counting
        // (`truncate_message`) would leave ~6000 units and Telegram would 400 it.
        let emoji = "😀".repeat(3000);
        let out = truncate_message_utf16(&emoji, 4096);
        let units: usize = out.chars().map(char::len_utf16).sum();
        assert!(
            units <= 4096,
            "result must fit the real UTF-16 limit: {units}"
        );
        assert!(out.ends_with(" […]"), "truncation is marked");
        assert!(out.starts_with('😀'), "keeps the head");
    }

    #[test]
    fn byte_truncation_keeps_multibyte_text_within_the_byte_limit() {
        // Each 中 is one `char` but **three** UTF-8 bytes. 1000 of them = 1000 chars
        // but 3000 bytes — char-counting would let it past a 1500-byte cap and a
        // homeserver would reject the whole event.
        let cjk = "中".repeat(1000);
        let out = truncate_message_bytes(&cjk, 1500);
        assert!(
            out.len() <= 1500,
            "result must fit the byte limit: {}",
            out.len()
        );
        assert!(out.ends_with(" […]"), "truncation is marked");
        assert!(out.starts_with('中'), "keeps the head");
        // The cut lands on a char boundary (String is always valid UTF-8 — this
        // would have panicked on a mid-codepoint byte slice).
        assert!(out.chars().next().is_some());
    }

    #[test]
    fn byte_truncation_passes_short_text_through_borrowed() {
        let m = truncate_message_bytes("hello 世界", 1500);
        assert_eq!(m, "hello 世界");
        assert!(matches!(m, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn utf16_truncation_passes_short_and_bmp_text_through() {
        // Under the cap → borrowed, no allocation.
        let m = truncate_message_utf16("hello 😀", 4096);
        assert_eq!(m, "hello 😀");
        assert!(matches!(m, std::borrow::Cow::Borrowed(_)));
        // All-BMP text behaves exactly like char-counting (1 unit per char).
        let bmp = "x".repeat(5000);
        let out = truncate_message_utf16(&bmp, 4096);
        assert_eq!(out.chars().count(), 4096);
        assert!(out.ends_with(" […]"));
    }
}
