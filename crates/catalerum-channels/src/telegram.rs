//! Telegram delivery + inbound via the Bot API (SOUL §25).
//!
//! - **Send** — a bot posts to a chat: `POST <base>/bot<token>/sendMessage` with
//!   `{ "chat_id": "<id>", "text": "<text>" }`.
//! - **Receive** ([`Channel::subscribe`]) — the bot long-polls
//!   `GET <base>/bot<token>/getUpdates`, yielding each new text message in the
//!   configured chat as an [`InMessage`]. Telegram does **not** deliver a bot its
//!   own `sendMessage` back through `getUpdates`, so there is no echo loop to
//!   filter (unlike Matrix).
//!
//! The bot token + chat id are the destination; `base_url` (default
//! `https://api.telegram.org`) is configurable so tests can point it at a mock.

use std::collections::VecDeque;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::{json, Value};

use crate::{Channel, ChannelError, InMessage, OutMessage, Result};

/// The default Telegram Bot API base URL.
const TELEGRAM_API: &str = "https://api.telegram.org";

/// Telegram's hard limit on `sendMessage` `text` length, in **UTF-16 code units**
/// (a non-BMP char such as an emoji costs 2). A longer message is rejected with a
/// `400`, so [`truncate_message_utf16`](crate::truncate_message_utf16) caps to it
/// in the same unit before sending.
const TELEGRAM_MAX_TEXT: usize = 4096;

/// `getUpdates` long-poll timeout in **seconds**. Kept safely **under** the shared
/// HTTP client's 30 s request timeout ([`crate::default_http_client`]) so a quiet
/// chat doesn't trip that cap and surface a spurious error every poll.
const LONG_POLL_SECS: u64 = 20;

/// Max updates fetched per `getUpdates` poll (the Telegram default + cap is 100).
const GET_UPDATES_LIMIT: u64 = 100;

/// Delivers messages to a Telegram chat via a bot's `sendMessage`.
pub struct TelegramChannel {
    client: reqwest::Client,
    base_url: String,
    bot_token: String,
    chat_id: String,
}

impl TelegramChannel {
    /// A channel sending as `bot_token` to `chat_id` (via the public Bot API).
    #[must_use]
    pub fn new(bot_token: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            client: crate::default_http_client(),
            base_url: TELEGRAM_API.to_string(),
            bot_token: bot_token.into(),
            chat_id: chat_id.into(),
        }
    }

    /// Override the API base URL (e.g. point at a mock in tests).
    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl Channel for TelegramChannel {
    fn kind(&self) -> &str {
        "telegram"
    }

    async fn send(&self, msg: &OutMessage) -> Result<()> {
        let url = format!(
            "{}/bot{}/sendMessage",
            self.base_url.trim_end_matches('/'),
            self.bot_token
        );
        let resp = self
            .client
            .post(&url)
            .json(&json!({
                "chat_id": self.chat_id,
                "text": crate::truncate_message_utf16(&msg.text, TELEGRAM_MAX_TEXT),
            }))
            .send()
            .await
            .map_err(|e| ChannelError::Request(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ChannelError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    async fn subscribe(&self) -> Result<BoxStream<'static, Result<InMessage>>> {
        let ctx = std::sync::Arc::new(TgCtx {
            client: self.client.clone(),
            base_url: self.base_url.trim_end_matches('/').to_string(),
            bot_token: self.bot_token.clone(),
            // Empty chat id ⇒ accept messages from any chat the bot is in (each
            // [`InMessage`] still carries its `source`); a set chat id binds this
            // channel to one chat (a group — the multiplayer case).
            chat_id: self.chat_id.clone(),
        });
        // `offset: None` + `primed: false` ⇒ the first `getUpdates` (a timeout-0
        // prime) only advances past any backlog so the stream yields messages that
        // arrive *after* subscription, not unconfirmed updates queued earlier.
        let stream = futures::stream::unfold(TgState::default(), move |mut st| {
            let ctx = ctx.clone();
            async move {
                loop {
                    if let Some(m) = st.pending.pop_front() {
                        return Some((Ok(m), st));
                    }
                    let timeout = if st.primed { LONG_POLL_SECS } else { 0 };
                    match get_updates_once(&ctx, st.offset, timeout).await {
                        Ok((max_update_id, msgs)) => {
                            if let Some(id) = max_update_id {
                                st.offset = Some(id + 1); // acknowledge the batch
                            }
                            if !st.primed {
                                st.primed = true; // discard the priming backlog
                                continue;
                            }
                            st.pending.extend(msgs);
                            // Empty (long-poll returned nothing): loop and re-poll.
                        }
                        Err(e) => {
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            return Some((Err(e), st));
                        }
                    }
                }
            }
        });
        Ok(Box::pin(stream))
    }
}

/// Immutable context shared by every `getUpdates` poll in a subscription.
struct TgCtx {
    client: reqwest::Client,
    base_url: String,
    bot_token: String,
    chat_id: String,
}

/// Mutable cursor threaded through the [`futures::stream::unfold`] loop.
#[derive(Default)]
struct TgState {
    offset: Option<i64>,
    pending: VecDeque<InMessage>,
    primed: bool,
}

/// One `getUpdates` round-trip: returns the highest `update_id` seen (to advance
/// the offset) and the new text messages in the configured chat.
async fn get_updates_once(
    ctx: &TgCtx,
    offset: Option<i64>,
    timeout_secs: u64,
) -> Result<(Option<i64>, Vec<InMessage>)> {
    let url = format!("{}/bot{}/getUpdates", ctx.base_url, ctx.bot_token);
    let mut req = ctx
        .client
        .get(&url)
        .query(&[("timeout", timeout_secs.to_string())])
        .query(&[("limit", GET_UPDATES_LIMIT.to_string())])
        .query(&[("allowed_updates", "[\"message\"]")]);
    if let Some(o) = offset {
        req = req.query(&[("offset", o.to_string())]);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| ChannelError::Request(e.to_string()))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ChannelError::Status {
            status: status.as_u16(),
            body,
        });
    }
    let v: Value = resp
        .json()
        .await
        .map_err(|e| ChannelError::Request(e.to_string()))?;
    let mut max_id: Option<i64> = None;
    let mut out = Vec::new();
    if let Some(results) = v.get("result").and_then(Value::as_array) {
        for upd in results {
            if let Some(id) = upd.get("update_id").and_then(Value::as_i64) {
                max_id = Some(max_id.map_or(id, |m| m.max(id)));
            }
            if let Some(m) = message_from_update(ctx, upd) {
                out.push(m);
            }
        }
    }
    Ok((max_id, out))
}

/// Extract an inbound text message from a `getUpdates` result, or `None` if it
/// carries no text `message`, or is from a chat other than the configured one.
fn message_from_update(ctx: &TgCtx, upd: &Value) -> Option<InMessage> {
    let message = upd.get("message")?;
    let text = message.get("text").and_then(Value::as_str)?.to_string();
    if text.is_empty() {
        return None;
    }
    let source = json_id_to_string(message.get("chat").and_then(|c| c.get("id"))?)?;
    // Bind to the configured chat when one is set; otherwise accept any chat. Trim
    // the configured id for BOTH the empty-check and the comparison — a whitespace-
    // padded config (e.g. `" 42 "` from env/TOML) must still match chat `42`, not
    // silently drop every inbound message.
    let want = ctx.chat_id.trim();
    if !want.is_empty() && want != source {
        return None;
    }
    let sender = message
        .get("from")
        .and_then(|f| f.get("id"))
        .and_then(json_id_to_string)
        .unwrap_or_default();
    let message_id = message.get("message_id").and_then(json_id_to_string);
    Some(InMessage {
        sender,
        text,
        source,
        message_id,
    })
}

/// Stringify a Telegram numeric (or already-string) id field. Telegram ids are
/// JSON numbers; we carry them as strings to match [`InMessage`] + the trigger.
fn json_id_to_string(v: &Value) -> Option<String> {
    v.as_i64()
        .map(|n| n.to_string())
        .or_else(|| v.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    #[tokio::test]
    async fn send_posts_telegram_sendmessage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/bot123:secret/sendMessage"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server)
            .await;

        let channel = TelegramChannel::new("123:secret", "42").with_base_url(server.uri());
        assert_eq!(channel.kind(), "telegram");
        channel
            .send(&OutMessage::text("build green ✅"))
            .await
            .unwrap();

        // The bot token is in the path; the body is `{ chat_id, text }`.
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let sent: Request = reqs.into_iter().next().unwrap();
        let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body, json!({ "chat_id": "42", "text": "build green ✅" }));
    }

    #[tokio::test]
    async fn telegram_error_status_surfaces() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("Unauthorized"))
            .mount(&server)
            .await;
        let channel = TelegramChannel::new("bad", "1").with_base_url(server.uri());
        let err = channel.send(&OutMessage::text("x")).await.unwrap_err();
        assert!(matches!(err, ChannelError::Status { status: 401, .. }));
    }

    #[test]
    fn message_from_update_trims_configured_chat_id() {
        // A whitespace-padded `chat_id` (common from env/TOML) must still match its
        // chat — the trim is applied to BOTH the empty-check and the comparison, so a
        // config of `" 42 "` no longer silently drops every inbound message.
        let ctx = TgCtx {
            client: reqwest::Client::new(),
            base_url: "http://x".into(),
            bot_token: "t".into(),
            chat_id: " 42 ".into(),
        };
        let upd = json!({ "message": { "chat": { "id": 42 }, "text": "hi", "from": { "id": 7 } } });
        let msg = message_from_update(&ctx, &upd).expect("padded chat_id still matches chat 42");
        assert_eq!(msg.text, "hi");
        assert_eq!(msg.source, "42");
        // A different chat is still filtered out.
        let other =
            json!({ "message": { "chat": { "id": 99 }, "text": "hi", "from": { "id": 7 } } });
        assert!(message_from_update(&ctx, &other).is_none());
        // A blank chat_id accepts any chat (unbound).
        let any_ctx = TgCtx {
            chat_id: "   ".into(),
            ..ctx
        };
        assert!(message_from_update(&any_ctx, &other).is_some());
    }

    #[tokio::test]
    async fn subscribe_yields_new_messages_from_the_bound_chat() {
        let server = MockServer::start().await;
        // Prime (timeout=0): no backlog to discard.
        Mock::given(method("GET"))
            .and(path("/bot123:secret/getUpdates"))
            .and(query_param("timeout", "0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": []
            })))
            .mount(&server)
            .await;
        // Live poll (timeout=20): one message in the bound chat (42) and one in a
        // different chat (99), which must be filtered out.
        Mock::given(method("GET"))
            .and(path("/bot123:secret/getUpdates"))
            .and(query_param("timeout", "20"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": [
                    { "update_id": 10, "message": {
                        "message_id": 7, "from": { "id": 555, "username": "alice" },
                        "chat": { "id": 99 }, "text": "wrong chat" } },
                    { "update_id": 11, "message": {
                        "message_id": 8, "from": { "id": 555, "username": "alice" },
                        "chat": { "id": 42 }, "text": "hello bot" } }
                ]
            })))
            .mount(&server)
            .await;

        let channel = TelegramChannel::new("123:secret", "42").with_base_url(server.uri());
        let mut stream = channel.subscribe().await.unwrap();
        let msg = stream.next().await.unwrap().unwrap();
        assert_eq!(
            msg.text, "hello bot",
            "the other chat's message is filtered"
        );
        assert_eq!(msg.source, "42");
        assert_eq!(msg.sender, "555");
        assert_eq!(msg.message_id.as_deref(), Some("8"));
    }
}
